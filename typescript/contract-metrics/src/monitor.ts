import {OpticsContext} from '@optics-xyz/multi-provider';
import {getEvents} from "@optics-xyz/multi-provider/dist/optics/events/fetch";
import * as contexts from "./registerContext";
import {logMonitorMetrics, writeUnprocessedMessages} from "./print";
import config from './config';

const cliArgs = process.argv.slice(2)

switch (cliArgs[0]) {
  case 'once':
    main(false)
    break;

  default:
    main(true)
    break;
}


async function main(forever: boolean) {
  if(forever){
    config.metrics.startServer(9090)
  }
  do {
    // write results to disk if we're not running forever
    await monitorAll(!forever);
    if(forever){
      config.baseLogger.info('Sleeping for 120 seconds.')
      await new Promise(resolve => setTimeout(resolve, 120000));
    }
  } while (forever);
}

async function monitorAll(shouldWrite: boolean) {
  for (let network of config.networks) {
    const origin = network;
    const remotes = config.networks.filter((m) => m != origin);
    const cont = (config.environment == 'production') ? contexts.mainnet : contexts.dev
    try {
      await monitor(cont, origin, remotes, shouldWrite);
    } catch(e){
      config.baseLogger.error({error: e}, `Encountered an Error while processing ${origin}!`)
      continue
    }
    
  }
}

async function monitor(
  context: OpticsContext,
  origin: string,
  remotes: string[],
  shouldWrite: boolean
) {
  config.baseLogger.info(`Checking ${origin}`);
  config.baseLogger.info(`Get Dispatch logs from ${origin}`);
  const home = context.mustGetCore(origin).home;
  const dispatchFilter = home.filters.Dispatch();
  const dispatchLogs = await getEvents(
      context,
      origin,
      home,
      dispatchFilter,
  );

  const processedLogs = [];
  for (let remote of remotes) {
    config.baseLogger.info(`Get Process logs from ${remote} for ${origin}`);
    const replica = context.mustGetReplicaFor(origin, remote);
    const processFilter = replica.filters.Process();
    const processLogs = await getEvents(
        context,
        remote,
        replica,
        processFilter,
    );
    processedLogs.push(...processLogs);
  }

  const unprocessedDetails = await getUnprocessedDetails(origin, dispatchLogs, processedLogs);

  // console.log
  logMonitorMetrics(origin, dispatchLogs, processedLogs, unprocessedDetails);
  config.metrics.setBridgeState(origin, config.environment, dispatchLogs.length, processedLogs.length, unprocessedDetails.length)
  // write details to file
  await writeUnprocessedMessages(unprocessedDetails, origin);
}

async function getUnprocessedDetails(origin: string, dispatchLogs: any[], processedLogs: any[]) {
  const processedMessageHashes = processedLogs.map(
      (log: any) => log.args.messageHash,
  );
  const unprocessedMessages = dispatchLogs.filter(
      (log: any) => !processedMessageHashes.includes(log.args.messageHash),
  );
  const promises = unprocessedMessages.map(async (log) => {
    const transaction = await log.getTransaction();
    return {
      chain: origin,
      transactionHash: transaction.hash,
      messageHash: log.args[0],
      leafIndex: log.args[1].toNumber(),
    };
  });
  return Promise.all(promises);
}