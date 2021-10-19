#![allow(clippy::enum_variant_names)]

use async_trait::async_trait;
use color_eyre::Result;
use ethers::contract::abigen;
use ethers::core::types::{Signature, H256};
use optics_core::db::OpticsDB;
/*
use optics_core::traits::CommittedMessage;
use optics_core::SignedUpdateWithMeta;
use optics_core::{
    traits::{
        ChainCommunicationError, Common, DoubleUpdate, Home, RawCommittedMessage, State, TxOutcome,
    },
    Message, SignedUpdate, Update, UpdateMeta,
*/
use optics_core::*;
use optics_core::{
    ChainCommunicationError, Common, DoubleUpdate, Home, Message, RawCommittedMessage,
    SignedUpdate, State, TxOutcome, Update,
};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio::try_join;
use tracing::{info, info_span, instrument};
use tracing::{instrument::Instrumented, Instrument};

use std::cmp::min;
use std::time::Duration;
use std::{convert::TryFrom, convert::TryInto, error::Error as StdError, sync::Arc};

use crate::report_tx;

static LAST_INSPECTED: &str = "home_indexer_last_inspected";

#[allow(missing_docs)]
abigen!(
    EthereumHomeInternal,
    "./chains/optics-ethereum/abis/Home.abi.json"
);

struct HomeIndexer<M>
where
    M: ethers::providers::Middleware,
{
    agent_name: String,
    home_name: String,
    contract: Arc<EthereumHomeInternal<M>>,
    provider: Arc<M>,
    db: OpticsDB,
    from_height: u32,
    chunk_size: u32,
    indexed_height: Arc<prometheus::IntGaugeVec>,
}

impl<M> HomeIndexer<M>
where
    M: ethers::providers::Middleware + 'static,
{
    #[instrument(err, skip(self))]
    async fn sync_updates(&self, from: u32, to: u32) -> Result<()> {
        let mut events = self
            .contract
            .update_filter()
            .from_block(from)
            .to_block(to)
            .query_with_meta()
            .await?;

        events.sort_by(|a, b| {
            let mut ordering = a.1.block_number.cmp(&b.1.block_number);
            if ordering == std::cmp::Ordering::Equal {
                ordering = a.1.transaction_index.cmp(&b.1.transaction_index);
            }

            ordering
        });

        let updates_with_meta = events.iter().map(|event| {
            let signature = Signature::try_from(event.0.signature.as_slice())
                .expect("chain accepted invalid signature");

            let update = Update {
                home_domain: event.0.home_domain,
                previous_root: event.0.old_root.into(),
                new_root: event.0.new_root.into(),
            };

            SignedUpdateWithMeta {
                signed_update: SignedUpdate { update, signature },
                metadata: UpdateMeta {
                    block_number: event.1.block_number.as_u64(),
                },
            }
        });

        for update_with_meta in updates_with_meta {
            self.db
                .store_latest_update(&self.home_name, &update_with_meta.signed_update)?;
            self.db.store_update_metadata(
                &self.home_name,
                update_with_meta.signed_update.update.new_root,
                update_with_meta.metadata,
            )?;

            info!(
                "Stored new update in db. Block number: {}. Previous root: {}. New root: {}.",
                &update_with_meta.metadata.block_number,
                &update_with_meta.signed_update.update.previous_root,
                &update_with_meta.signed_update.update.new_root,
            );
        }

        Ok(())
    }

    #[instrument(err, skip(self))]
    async fn sync_leaves(&self, from: u32, to: u32) -> Result<()> {
        let events = self
            .contract
            .dispatch_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        let messages = events.into_iter().map(|f| RawCommittedMessage {
            leaf_index: f.leaf_index.as_u32(),
            committed_root: f.committed_root.into(),
            message: f.message,
        });

        for message in messages {
            self.db
                .store_raw_committed_message(&self.home_name, &message)?;

            let committed_message: CommittedMessage = message.try_into()?;
            info!(
                "Stored new message in db. Leaf index: {}. Origin: {}. Destination: {}. Nonce: {}.",
                &committed_message.leaf_index,
                &committed_message.message.origin,
                &committed_message.message.destination,
                &committed_message.message.nonce
            );
        }

        Ok(())
    }

    fn spawn(self) -> Instrumented<JoinHandle<Result<()>>> {
        let span = info_span!("HomeIndexer");

        tokio::spawn(async move {
            let mut next_height: u32 = self
                .db
                .retrieve_decodable(&self.home_name, "", LAST_INSPECTED)
                .expect("db failure")
                .unwrap_or(self.from_height);
            info!(
                next_height = next_height,
                "resuming indexer from {}", next_height
            );

            loop {
                self.indexed_height
                    .with_label_values(&[&self.home_name, &self.agent_name])
                    .set(next_height as i64);

                let tip = self.provider.get_block_number().await?.as_u32();
                let candidate = next_height + self.chunk_size;
                let to = min(tip, candidate);

                info!(
                    next_height = next_height,
                    to = to,
                    "indexing block heights {}...{}",
                    next_height,
                    to
                );

                // TODO(james): these shouldn't have to go in lockstep
                try_join!(
                    self.sync_updates(next_height, to),
                    self.sync_leaves(next_height, to)
                )?;

                self.db
                    .store_encodable(&self.home_name, "", LAST_INSPECTED, &next_height)?;
                next_height = to;
                // sleep here if we've caught up
                if to == tip {
                    sleep(Duration::from_secs(100)).await;
                }
            }
        })
        .instrument(span)
    }
}

/// A reference to a Home contract on some Ethereum chain
#[derive(Debug)]
pub struct EthereumHome<M>
where
    M: ethers::providers::Middleware,
{
    contract: Arc<EthereumHomeInternal<M>>,
    db: OpticsDB,
    domain: u32,
    name: String,
    provider: Arc<M>,
}

impl<M> EthereumHome<M>
where
    M: ethers::providers::Middleware + 'static,
{
    /// Create a reference to a Home at a specific Ethereum address on some
    /// chain
    pub fn new(
        provider: Arc<M>,
        ContractLocator {
            name,
            domain,
            address,
        }: &ContractLocator,
        db: OpticsDB,
    ) -> Self {
        Self {
            contract: Arc::new(EthereumHomeInternal::new(address, provider.clone())),
            domain: *domain,
            name: name.to_owned(),
            db,
            provider,
        }
    }
}

#[async_trait]
impl<M> Common for EthereumHome<M>
where
    M: ethers::providers::Middleware + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    #[tracing::instrument(err, skip(self))]
    async fn status(&self, txid: H256) -> Result<Option<TxOutcome>, ChainCommunicationError> {
        let receipt_opt = self
            .contract
            .client()
            .get_transaction_receipt(txid)
            .await
            .map_err(|e| Box::new(e) as Box<dyn StdError + Send + Sync>)?;

        Ok(receipt_opt.map(Into::into))
    }

    #[tracing::instrument(err, skip(self))]
    async fn updater(&self) -> Result<H256, ChainCommunicationError> {
        Ok(self.contract.updater().call().await?.into())
    }

    #[tracing::instrument(err, skip(self))]
    async fn state(&self) -> Result<State, ChainCommunicationError> {
        let state = self.contract.state().call().await?;
        match state {
            0 => Ok(State::Waiting),
            1 => Ok(State::Failed),
            _ => unreachable!(),
        }
    }

    #[tracing::instrument(err, skip(self))]
    async fn committed_root(&self) -> Result<H256, ChainCommunicationError> {
        Ok(self.contract.committed_root().call().await?.into())
    }

    #[tracing::instrument(err, skip(self))]
    async fn signed_update_by_old_root(
        &self,
        old_root: H256,
    ) -> Result<Option<SignedUpdate>, ChainCommunicationError> {
        loop {
            if let Some(update) = self.db.update_by_previous_root(&self.name, old_root)? {
                return Ok(Some(update));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    #[tracing::instrument(err, skip(self))]
    async fn signed_update_by_new_root(
        &self,
        new_root: H256,
    ) -> Result<Option<SignedUpdate>, ChainCommunicationError> {
        loop {
            if let Some(update) = self.db.update_by_new_root(&self.name, new_root)? {
                return Ok(Some(update));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    #[tracing::instrument(err, skip(self), fields(hexSignature = %format!("0x{}", hex::encode(update.signature.to_vec()))))]
    async fn update(&self, update: &SignedUpdate) -> Result<TxOutcome, ChainCommunicationError> {
        let tx = self.contract.update(
            update.update.previous_root.to_fixed_bytes(),
            update.update.new_root.to_fixed_bytes(),
            update.signature.to_vec(),
        );

        Ok(report_tx!(tx).into())
    }

    #[tracing::instrument(err, skip(self))]
    async fn double_update(
        &self,
        double: &DoubleUpdate,
    ) -> Result<TxOutcome, ChainCommunicationError> {
        let tx = self.contract.double_update(
            double.0.update.previous_root.to_fixed_bytes(),
            [
                double.0.update.new_root.to_fixed_bytes(),
                double.1.update.new_root.to_fixed_bytes(),
            ],
            double.0.signature.to_vec(),
            double.1.signature.to_vec(),
        );
        let response = report_tx!(tx);

        Ok(response.into())
    }

    /// Start an indexing task that syncs chain state
    fn index(
        &self,
        agent_name: String,
        from_height: u32,
        chunk_size: u32,
        indexed_height: Arc<prometheus::IntGaugeVec>,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let indexer = HomeIndexer {
            agent_name,
            home_name: self.name.to_owned(),
            contract: self.contract.clone(),
            db: self.db.clone(),
            from_height,
            provider: self.provider.clone(),
            chunk_size,
            indexed_height,
        };
        indexer.spawn()
    }
}

#[async_trait]
impl<M> Home for EthereumHome<M>
where
    M: ethers::providers::Middleware + 'static,
{
    fn local_domain(&self) -> u32 {
        self.domain
    }

    #[tracing::instrument(err, skip(self))]
    async fn raw_message_by_nonce(
        &self,
        destination: u32,
        nonce: u32,
    ) -> Result<Option<RawCommittedMessage>, ChainCommunicationError> {
        loop {
            if let Some(update) = self.db.message_by_nonce(&self.name, destination, nonce)? {
                return Ok(Some(update));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    #[tracing::instrument(err, skip(self))]
    async fn raw_message_by_leaf(
        &self,
        leaf: H256,
    ) -> Result<Option<RawCommittedMessage>, ChainCommunicationError> {
        loop {
            if let Some(update) = self.db.message_by_leaf(&self.name, leaf)? {
                return Ok(Some(update));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn leaf_by_tree_index(
        &self,
        tree_index: usize,
    ) -> Result<Option<H256>, ChainCommunicationError> {
        loop {
            if let Some(update) = self.db.leaf_by_leaf_index(&self.name, tree_index as u32)? {
                return Ok(Some(update));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    #[tracing::instrument(err, skip(self))]
    async fn nonces(&self, destination: u32) -> Result<u32, ChainCommunicationError> {
        Ok(self.contract.nonces(destination).call().await?)
    }

    #[tracing::instrument(err, skip(self))]
    async fn dispatch(&self, message: &Message) -> Result<TxOutcome, ChainCommunicationError> {
        let tx = self.contract.dispatch(
            message.destination,
            message.recipient.to_fixed_bytes(),
            message.body.clone(),
        );

        Ok(report_tx!(tx).into())
    }

    async fn queue_contains(&self, root: H256) -> Result<bool, ChainCommunicationError> {
        Ok(self.contract.queue_contains(root.into()).call().await?)
    }

    #[tracing::instrument(err, skip(self), fields(hexSignature = %format!("0x{}", hex::encode(update.signature.to_vec()))))]
    async fn improper_update(
        &self,
        update: &SignedUpdate,
    ) -> Result<TxOutcome, ChainCommunicationError> {
        let tx = self.contract.improper_update(
            update.update.previous_root.to_fixed_bytes(),
            update.update.new_root.to_fixed_bytes(),
            update.signature.to_vec(),
        );

        Ok(report_tx!(tx).into())
    }

    #[tracing::instrument(err, skip(self))]
    async fn produce_update(&self) -> Result<Option<Update>, ChainCommunicationError> {
        let (a, b) = self.contract.suggest_update().call().await?;

        let previous_root: H256 = a.into();
        let new_root: H256 = b.into();

        if new_root.is_zero() {
            return Ok(None);
        }

        Ok(Some(Update {
            home_domain: self.local_domain(),
            previous_root,
            new_root,
        }))
    }
}
