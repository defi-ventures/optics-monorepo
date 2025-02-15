use async_trait::async_trait;
use color_eyre::{eyre::bail, Report, Result};
use thiserror::Error;

use ethers::core::types::H256;
use futures_util::future::{join, join_all, select_all};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    task::JoinHandle,
    time::sleep,
};
use tracing::{info, info_span, instrument::Instrumented, Instrument};

use optics_base::{cancel_task, AgentCore, ConnectionManagers, Homes, OpticsAgent};
use optics_core::{
    db::HomeDB, ChainCommunicationError, Common, ConnectionManager, DoubleUpdate,
    FailureNotification, Home, SignedUpdate, Signers, TxOutcome,
};

use crate::settings::WatcherSettings as Settings;

#[derive(Debug, Error)]
enum WatcherError {
    #[error("Syncing finished")]
    SyncingFinished,
}

#[derive(Debug)]
pub struct ContractWatcher<C>
where
    C: Common + ?Sized + 'static,
{
    interval: u64,
    committed_root: H256,
    tx: mpsc::Sender<SignedUpdate>,
    contract: Arc<C>,
}

impl<C> ContractWatcher<C>
where
    C: Common + ?Sized + 'static,
{
    pub fn new(
        interval: u64,
        from: H256,
        tx: mpsc::Sender<SignedUpdate>,
        contract: Arc<C>,
    ) -> Self {
        Self {
            interval,
            committed_root: from,
            tx,
            contract,
        }
    }

    async fn poll_and_send_update(&mut self) -> Result<()> {
        let update_opt = self
            .contract
            .signed_update_by_old_root(self.committed_root)
            .await?;

        if update_opt.is_none() {
            return Ok(());
        }

        let new_update = update_opt.unwrap();
        self.committed_root = new_update.update.new_root;

        self.tx.send(new_update).await?;
        Ok(())
    }

    #[tracing::instrument]
    fn spawn(mut self) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            loop {
                self.poll_and_send_update().await?;
                sleep(Duration::from_secs(self.interval)).await;
            }
        })
    }
}

#[derive(Debug)]
pub struct HistorySync<C>
where
    C: Common + ?Sized + 'static,
{
    interval: u64,
    committed_root: H256,
    tx: mpsc::Sender<SignedUpdate>,
    contract: Arc<C>,
}

impl<C> HistorySync<C>
where
    C: Common + ?Sized + 'static,
{
    pub fn new(
        interval: u64,
        from: H256,
        tx: mpsc::Sender<SignedUpdate>,
        contract: Arc<C>,
    ) -> Self {
        Self {
            committed_root: from,
            tx,
            contract,
            interval,
        }
    }

    async fn update_history(&mut self) -> Result<()> {
        let previous_update = self
            .contract
            .signed_update_by_new_root(self.committed_root)
            .await?;

        if previous_update.is_none() {
            // Task finished
            return Err(Report::new(WatcherError::SyncingFinished));
        }

        // Dispatch to the handler
        let previous_update = previous_update.unwrap();

        // set up for next loop iteration
        self.committed_root = previous_update.update.previous_root;
        self.tx.send(previous_update).await?;
        if self.committed_root.is_zero() {
            // Task finished
            return Err(Report::new(WatcherError::SyncingFinished));
        }

        Ok(())
    }

    #[tracing::instrument]
    fn spawn(mut self) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            loop {
                let res = self.update_history().await;
                if res.is_err() {
                    // Syncing done
                    break;
                }

                sleep(Duration::from_secs(self.interval)).await;
            }

            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct UpdateHandler {
    rx: mpsc::Receiver<SignedUpdate>,
    home_db: HomeDB,
    home: Arc<Homes>,
}

impl UpdateHandler {
    pub fn new(rx: mpsc::Receiver<SignedUpdate>, home_db: HomeDB, home: Arc<Homes>) -> Self {
        Self { rx, home_db, home }
    }

    fn check_double_update(&mut self, update: &SignedUpdate) -> Result<(), DoubleUpdate> {
        let old_root = update.update.previous_root;
        let new_root = update.update.new_root;

        match self
            .home_db
            .update_by_previous_root(old_root)
            .expect("!db_get")
        {
            Some(existing) => {
                if existing.update.new_root != new_root {
                    return Err(DoubleUpdate(existing, update.to_owned()));
                }
            }
            None => {
                self.home_db.store_latest_update(update).expect("!db_put");
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    fn spawn(mut self) -> JoinHandle<Result<DoubleUpdate>> {
        tokio::spawn(async move {
            loop {
                let update = self.rx.recv().await;
                // channel is closed
                if update.is_none() {
                    bail!("Channel closed.")
                }

                let update = update.unwrap();
                let old_root = update.update.previous_root;

                if old_root == self.home.committed_root().await? {
                    // It is okay if tx reverts
                    let _ = self.home.update(&update).await;
                }

                if let Err(double_update) = self.check_double_update(&update) {
                    return Ok(double_update);
                }
            }
        })
    }
}

type TaskMap = Arc<RwLock<HashMap<String, Instrumented<JoinHandle<Result<()>>>>>>;

#[derive(Debug)]
pub struct Watcher {
    signer: Arc<Signers>,
    interval_seconds: u64,
    sync_tasks: TaskMap,
    watch_tasks: TaskMap,
    connection_managers: Vec<ConnectionManagers>,
    core: AgentCore,
}

impl AsRef<AgentCore> for Watcher {
    fn as_ref(&self) -> &AgentCore {
        &self.core
    }
}

#[allow(clippy::unit_arg)]
impl Watcher {
    /// Instantiate a new watcher.
    pub fn new(
        signer: Signers,
        interval_seconds: u64,
        connection_managers: Vec<ConnectionManagers>,
        core: AgentCore,
    ) -> Self {
        Self {
            signer: Arc::new(signer),
            interval_seconds,
            sync_tasks: Default::default(),
            watch_tasks: Default::default(),
            connection_managers,
            core,
        }
    }

    async fn shutdown(&self) {
        for (_, v) in self.watch_tasks.write().await.drain() {
            cancel_task!(v);
        }
        for (_, v) in self.sync_tasks.write().await.drain() {
            cancel_task!(v);
        }
    }

    // Handle a double-update once it has been detected.
    #[tracing::instrument]
    async fn handle_failure(
        &self,
        double: &DoubleUpdate,
    ) -> Vec<Result<TxOutcome, ChainCommunicationError>> {
        // Create vector of double update futures
        let mut double_update_futs: Vec<_> = self
            .core
            .replicas
            .values()
            .map(|replica| replica.double_update(double))
            .collect();
        double_update_futs.push(self.core.home.double_update(double));

        // Created signed failure notification
        let signed_failure = FailureNotification {
            home_domain: self.home().local_domain(),
            updater: self.home().updater().await.unwrap().into(),
        }
        .sign_with(self.signer.as_ref())
        .await
        .expect("!sign");

        // Create vector of futures for unenrolling replicas (one per
        // connection manager)
        let mut unenroll_futs = Vec::new();
        for connection_manager in self.connection_managers.iter() {
            unenroll_futs.push(connection_manager.unenroll_replica(&signed_failure));
        }

        // Join both vectors of double update and unenroll futures and
        // return vector containing all results
        let (double_update_res, unenroll_res) =
            join(join_all(double_update_futs), join_all(unenroll_futs)).await;
        double_update_res
            .into_iter()
            .chain(unenroll_res.into_iter())
            .collect()
    }

    fn run_watch_tasks(
        &self,
        double_update_tx: oneshot::Sender<DoubleUpdate>,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let home_db = HomeDB::new(self.db(), self.home().name().to_owned());
        let home = self.home();
        let replicas = self.replicas().clone();
        let interval_seconds = self.interval_seconds;
        let sync_tasks = self.sync_tasks.clone();
        let watch_tasks = self.watch_tasks.clone();

        tokio::spawn(async move {
            // Spawn update handler
            let (tx, rx) = mpsc::channel(200);
            let handler = UpdateHandler::new(rx, home_db, home.clone()).spawn();

            // For each replica, spawn polling and history syncing tasks
            for (name, replica) in replicas {
                let from = replica.committed_root().await?;

                watch_tasks.write().await.insert(
                    (*name).to_owned(),
                    ContractWatcher::new(interval_seconds, from, tx.clone(), replica.clone())
                        .spawn()
                        .in_current_span(),
                );
                sync_tasks.write().await.insert(
                    (*name).to_owned(),
                    HistorySync::new(interval_seconds, from, tx.clone(), replica)
                        .spawn()
                        .in_current_span(),
                );
            }

            // Spawn polling and history syncing tasks for home
            let from = home.committed_root().await?;
            let home_watcher =
                ContractWatcher::new(interval_seconds, from, tx.clone(), home.clone())
                    .spawn()
                    .in_current_span();
            let home_sync = HistorySync::new(interval_seconds, from, tx.clone(), home)
                .spawn()
                .in_current_span();

            // Wait for update handler to encounter error OR fraud
            let double_update_res = handler.await?;

            // Cancel running tasks
            tracing::info!("Update handler has resolved. Cancelling all other tasks");
            cancel_task!(home_watcher);
            cancel_task!(home_sync);

            // If double update found, send through oneshot
            if let Err(e) = double_update_tx.send(double_update_res?) {
                bail!("Failed to send double update through oneshot: {:?}", e);
            }

            Ok(())
        })
        .in_current_span()
    }
}

#[async_trait]
#[allow(clippy::unit_arg)]
impl OpticsAgent for Watcher {
    const AGENT_NAME: &'static str = "watcher";

    type Settings = Settings;

    #[tracing::instrument(err)]
    async fn from_settings(settings: Self::Settings) -> Result<Self>
    where
        Self: Sized,
    {
        let mut connection_managers = vec![];
        for chain_setup in settings.connection_managers.iter() {
            let signer = settings.base.get_signer(&chain_setup.name).await;
            let manager = chain_setup.try_into_connection_manager(signer).await;
            connection_managers.push(manager);
        }

        let (connection_managers, errors): (Vec<_>, Vec<_>) =
            connection_managers.into_iter().partition(Result::is_ok);

        // Report any invalid ConnectionManager chain setups
        errors.into_iter().for_each(|e| {
            let err = e.unwrap_err();
            tracing::error!("{:?}", err)
        });

        let connection_managers: Vec<_> = connection_managers
            .into_iter()
            .map(Result::unwrap)
            .collect();

        let core = settings.as_ref().try_into_core("watcher").await?;

        Ok(Self::new(
            settings.watcher.try_into_signer().await?,
            settings.interval.parse().expect("invalid uint"),
            connection_managers,
            core,
        ))
    }

    #[tracing::instrument]
    fn run(&self, _name: &str) -> Instrumented<tokio::task::JoinHandle<Result<()>>> {
        panic!("Watcher::run should not be called. Always call run_all")
    }

    fn run_many(&self, _replicas: &[&str]) -> Instrumented<JoinHandle<Result<()>>> {
        panic!("Watcher::run_many should not be called. Always call run_all")
    }

    fn run_all(self) -> Instrumented<JoinHandle<Result<()>>>
    where
        Self: Sized + 'static,
    {
        tokio::spawn(async move {
            info!("Starting Watcher tasks");

            // Indexer setup
            let block_height = self
                .as_ref()
                .metrics
                .new_int_gauge(
                    "block_height",
                    "Height of a recently observed block",
                    &["network", "agent"],
                )
                .expect("failed to register block_height metric")
                .with_label_values(&[self.home().name(), Self::AGENT_NAME]);
            let indexer = &self.as_ref().indexer;
            let index_task = self
                .home()
                .index(indexer.from(), indexer.chunk_size(), block_height);

            // Watcher watch tasks setup
            let (double_update_tx, mut double_update_rx) = oneshot::channel::<DoubleUpdate>();
            let watch_tasks = self.run_watch_tasks(double_update_tx);

            // Race index and run tasks
            info!("selecting");
            let tasks = vec![index_task, watch_tasks];
            let (_, _, remaining) = select_all(tasks).await;

            // Cancel lagging task and watcher polling/syncing tasks
            for task in remaining.into_iter() {
                cancel_task!(task);
            }
            self.shutdown().await;

            // Check if double update was sent during run task
            match double_update_rx.try_recv() {
                Ok(double_update) => {
                    tracing::error!(
                        double_update = ?double_update,
                        "Double update detected! Notifying all contracts and unenrolling replicas! Double update: {:?}",
                        double_update
                    );
                    self.handle_failure(&double_update)
                        .await
                        .iter()
                        .for_each(|res| tracing::info!("{:#?}", res));

                    bail!(
                        r#"
                        Double update detected!
                        All contracts notified!
                        Replicas unenrolled!
                        Watcher has been shut down!
                    "#
                    )
                }
                Err(_) => Ok(()),
            }
        })
        .instrument(info_span!("Watcher::run_all"))
    }
}

#[cfg(test)]
mod test {
    use optics_base::IndexSettings;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use ethers::core::types::H256;
    use ethers::signers::{LocalWallet, Signer};

    use optics_base::Replicas;
    use optics_core::{DoubleUpdate, SignedFailureNotification, Update};
    use optics_test::{
        mocks::{MockConnectionManagerContract, MockHomeContract, MockReplicaContract},
        test_utils,
    };

    use super::*;

    #[tokio::test]
    async fn contract_watcher_polls_and_sends_update() {
        let signer: LocalWallet =
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap();

        let first_root = H256::from([1; 32]);
        let second_root = H256::from([2; 32]);

        let signed_update = Update {
            home_domain: 1,
            previous_root: first_root,
            new_root: second_root,
        }
        .sign_with(&signer)
        .await
        .expect("!sign");

        let mut mock_home = MockHomeContract::new();
        {
            let signed_update = signed_update.clone();
            // home.signed_update_by_old_root called once and
            // returns mock value signed_update when called with first_root
            mock_home
                .expect__signed_update_by_old_root()
                .withf(move |r: &H256| *r == first_root)
                .times(1)
                .return_once(move |_| Ok(Some(signed_update)));
        }

        let mut home: Arc<Homes> = Arc::new(mock_home.into());
        let (tx, mut rx) = mpsc::channel(200);
        {
            let mut contract_watcher =
                ContractWatcher::new(3, first_root, tx.clone(), home.clone());

            contract_watcher
                .poll_and_send_update()
                .await
                .expect("Should have received Ok(())");

            assert_eq!(contract_watcher.committed_root, second_root);
            assert_eq!(rx.recv().await.unwrap(), signed_update);
        }

        let mock_home = Arc::get_mut(&mut home).unwrap();
        mock_home.checkpoint();
    }

    #[tokio::test]
    async fn history_sync_updates_history() {
        let signer: LocalWallet =
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap();

        let zero_root = H256::zero(); // Original zero root
        let first_root = H256::from([1; 32]);
        let second_root = H256::from([2; 32]);

        // Zero root to first root
        let first_signed_update = Update {
            home_domain: 1,
            previous_root: zero_root,
            new_root: first_root,
        }
        .sign_with(&signer)
        .await
        .expect("!sign");

        // First root to second root
        let second_signed_update = Update {
            home_domain: 1,
            previous_root: first_root,
            new_root: second_root,
        }
        .sign_with(&signer)
        .await
        .expect("!sign");

        let mut mock_home = MockHomeContract::new();
        {
            let first_signed_update = first_signed_update.clone();
            let second_signed_update = second_signed_update.clone();
            // home.signed_update_by_new_root called once with second_root
            // and returns mock value second_signed_update
            mock_home
                .expect__signed_update_by_new_root()
                .withf(move |r: &H256| *r == second_root)
                .times(1)
                .return_once(move |_| Ok(Some(second_signed_update)));
            // home.signed_update_by_new_root called once with first_root
            // and returns mock value first_signed_update
            mock_home
                .expect__signed_update_by_new_root()
                .withf(move |r: &H256| *r == first_root)
                .times(1)
                .return_once(move |_| Ok(Some(first_signed_update)));
        }

        let mut home: Arc<Homes> = Arc::new(mock_home.into());
        let (tx, mut rx) = mpsc::channel(200);
        {
            let mut history_sync = HistorySync::new(3, second_root, tx.clone(), home.clone());

            // First update_history call returns first -> second update
            history_sync
                .update_history()
                .await
                .expect("Should have received Ok(())");

            assert_eq!(history_sync.committed_root, first_root);
            assert_eq!(rx.recv().await.unwrap(), second_signed_update);

            // Second update_history call returns zero -> first update
            // and should return WatcherError::SyncingFinished
            history_sync
                .update_history()
                .await
                .expect_err("Should have received WatcherError::SyncingFinished");

            assert_eq!(history_sync.committed_root, zero_root);
            assert_eq!(rx.recv().await.unwrap(), first_signed_update)
        }

        let mock_home = Arc::get_mut(&mut home).unwrap();
        mock_home.checkpoint();
    }

    #[tokio::test]
    async fn update_handler_detects_double_update() {
        test_utils::run_test_db(|db| async move {
            let signer: LocalWallet =
                "1111111111111111111111111111111111111111111111111111111111111111"
                    .parse()
                    .unwrap();

            let first_root = H256::from([1; 32]);
            let second_root = H256::from([2; 32]);
            let third_root = H256::from([3; 32]);
            let bad_third_root = H256::from([4; 32]);

            let first_update = Update {
                home_domain: 1,
                previous_root: first_root,
                new_root: second_root,
            }
            .sign_with(&signer)
            .await
            .expect("!sign");

            let second_update = Update {
                home_domain: 1,
                previous_root: second_root,
                new_root: third_root,
            }
            .sign_with(&signer)
            .await
            .expect("!sign");

            let bad_second_update = Update {
                home_domain: 1,
                previous_root: second_root,
                new_root: bad_third_root,
            }
            .sign_with(&signer)
            .await
            .expect("!sign");

            {
                let (_tx, rx) = mpsc::channel(200);
                let mut handler = UpdateHandler {
                    rx,
                    home_db: HomeDB::new(db, "home_1".to_owned()),
                    home: Arc::new(MockHomeContract::new().into()),
                };

                let _first_update_ret = handler
                    .check_double_update(&first_update)
                    .expect("Update should have been valid");

                let _second_update_ret = handler
                    .check_double_update(&second_update)
                    .expect("Update should have been valid");

                let bad_second_update_ret = handler
                    .check_double_update(&bad_second_update)
                    .expect_err("Update should have been invalid");
                assert_eq!(
                    bad_second_update_ret,
                    DoubleUpdate(second_update, bad_second_update)
                );
            }
        })
        .await
    }

    #[tokio::test]
    async fn it_fails_contracts_and_unenrolls_replicas_on_double_update() {
        test_utils::run_test_db(|db| async move {
            let home_domain = 1;

            let updater: LocalWallet =
                "1111111111111111111111111111111111111111111111111111111111111111"
                    .parse()
                    .unwrap();

            // Double update setup
            let first_root = H256::from([1; 32]);
            let second_root = H256::from([2; 32]);
            let bad_second_root = H256::from([3; 32]);

            let update = Update {
                home_domain,
                previous_root: first_root,
                new_root: second_root,
            }
            .sign_with(&updater)
            .await
            .expect("!sign");

            let bad_update = Update {
                home_domain,
                previous_root: first_root,
                new_root: bad_second_root,
            }
            .sign_with(&updater)
            .await
            .expect("!sign");

            let double = DoubleUpdate(update, bad_update);
            let signed_failure = FailureNotification {
                home_domain,
                updater: updater.address().into(),
            }
            .sign_with(&updater)
            .await
            .expect("!sign");

            // Contract setup
            let mut mock_connection_manager_1 = MockConnectionManagerContract::new();
            let mut mock_connection_manager_2 = MockConnectionManagerContract::new();

            let mut mock_home = MockHomeContract::new();
            let mut mock_replica_1 = MockReplicaContract::new();
            let mut mock_replica_2 = MockReplicaContract::new();

            // Home and replica expectations
            {
                // home.local_domain returns `home_domain`
                mock_home
                    .expect__local_domain()
                    .times(1)
                    .return_once(move || home_domain);

                // home.updater returns `updater` address
                let updater = updater.clone();
                mock_home
                    .expect__updater()
                    .times(1)
                    .return_once(move || Ok(updater.address().into()));

                // home.double_update called once
                let double = double.clone();
                mock_home
                    .expect__double_update()
                    .withf(move |d: &DoubleUpdate| *d == double)
                    .times(1)
                    .return_once(move |_| {
                        Ok(TxOutcome {
                            txid: H256::default(),
                            executed: true,
                        })
                    });
            }
            {
                // replica_1.double_update called once
                let double = double.clone();
                mock_replica_1
                    .expect__double_update()
                    .withf(move |d: &DoubleUpdate| *d == double)
                    .times(1)
                    .return_once(move |_| {
                        Ok(TxOutcome {
                            txid: H256::default(),
                            executed: true,
                        })
                    });
            }
            {
                // replica_2.double_update called once
                let double = double.clone();
                mock_replica_2
                    .expect__double_update()
                    .withf(move |d: &DoubleUpdate| *d == double)
                    .times(1)
                    .return_once(move |_| {
                        Ok(TxOutcome {
                            txid: H256::default(),
                            executed: true,
                        })
                    });
            }

            // Connection manager expectations
            {
                // connection_manager_1.unenroll_replica called once
                let signed_failure = signed_failure.clone();
                mock_connection_manager_1
                    .expect__unenroll_replica()
                    .withf(move |f: &SignedFailureNotification| *f == signed_failure)
                    .times(1)
                    .return_once(move |_| {
                        Ok(TxOutcome {
                            txid: H256::default(),
                            executed: true,
                        })
                    });
            }
            {
                // connection_manager_2.unenroll_replica called once
                let signed_failure = signed_failure.clone();
                mock_connection_manager_2
                    .expect__unenroll_replica()
                    .withf(move |f: &SignedFailureNotification| *f == signed_failure)
                    .times(1)
                    .return_once(move |_| {
                        Ok(TxOutcome {
                            txid: H256::default(),
                            executed: true,
                        })
                    });
            }

            // Watcher agent setup
            let connection_managers: Vec<ConnectionManagers> = vec![
                mock_connection_manager_1.into(),
                mock_connection_manager_2.into(),
            ];

            let home = Arc::new(mock_home.into());
            let replica_1 = Arc::new(mock_replica_1.into());
            let replica_2 = Arc::new(mock_replica_2.into());

            let mut replica_map: HashMap<String, Arc<Replicas>> = HashMap::new();
            replica_map.insert("replica_1".into(), replica_1);
            replica_map.insert("replica_2".into(), replica_2);

            let core = AgentCore {
                home,
                replicas: replica_map,
                db: db,
                indexer: IndexSettings::default(),
                settings: optics_base::Settings::default(),
                metrics: Arc::new(
                    optics_base::CoreMetrics::new(
                        "watcher_test",
                        None,
                        Arc::new(prometheus::Registry::new()),
                    )
                    .expect("could not make metrics"),
                ),
            };

            let mut watcher = Watcher::new(updater.into(), 1, connection_managers, core);
            watcher.handle_failure(&double).await;

            // Checkpoint connection managers
            for connection_manager in watcher.connection_managers.iter_mut() {
                connection_manager.checkpoint();
            }

            // Checkpoint home
            let mock_home = Arc::get_mut(&mut watcher.core.home).unwrap();
            mock_home.checkpoint();

            // Checkpoint replicas
            for replica in watcher.core.replicas.values_mut() {
                Arc::get_mut(replica).unwrap().checkpoint();
            }
        })
        .await
    }
}
