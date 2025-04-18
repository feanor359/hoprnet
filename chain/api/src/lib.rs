//! Crate containing the API object for chain operations used by the HOPRd node.

pub mod config;
pub mod errors;
pub mod executors;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use config::ChainNetworkConfig;
use executors::{EthereumTransactionExecutor, RpcEthereumClient, RpcEthereumClientConfig};
use hopr_async_runtime::prelude::{sleep, spawn, JoinHandle};
use hopr_chain_actions::action_queue::{ActionQueue, ActionQueueConfig};
use hopr_chain_actions::action_state::IndexerActionTracker;
use hopr_chain_actions::payload::SafePayloadGenerator;
use hopr_chain_actions::ChainActions;
use hopr_chain_indexer::{block::Indexer, handlers::ContractEventHandlers, IndexerConfig};
use hopr_chain_rpc::client::SimpleJsonRpcRetryPolicy;
use hopr_chain_rpc::rpc::{RpcOperations, RpcOperationsConfig};
use hopr_chain_rpc::HoprRpcOperations;
pub use hopr_chain_types::chain_events::SignificantChainEvent;
use hopr_chain_types::ContractAddresses;
use hopr_crypto_types::prelude::*;
use hopr_db_sql::HoprDbAllOperations;
use hopr_internal_types::account::AccountEntry;
pub use hopr_internal_types::channels::ChannelEntry;
use hopr_internal_types::prelude::ChannelDirection;
use hopr_primitive_types::prelude::*;

use crate::errors::{HoprChainError, Result};

/// The default HTTP request engine
///
/// TODO: Should be an internal type, `hopr_lib::chain` must be moved to this package
#[cfg(feature = "runtime-async-std")]
pub type DefaultHttpRequestor = hopr_chain_rpc::client::surf_client::SurfRequestor;

// Both features could be enabled during testing; therefore, we only use tokio when its
// exclusively enabled.
#[cfg(all(feature = "runtime-tokio", not(feature = "runtime-async-std")))]
pub type DefaultHttpRequestor = hopr_chain_rpc::client::reqwest_client::ReqwestRequestor;

/// The default JSON RPC provider client
///
/// TODO: Should be an internal type, `hopr_lib::chain` must be moved to this package
pub type JsonRpcClient = hopr_chain_rpc::client::JsonRpcProviderClient<DefaultHttpRequestor, SimpleJsonRpcRetryPolicy>;

/// Checks whether the node can be registered with the Safe in the NodeSafeRegistry
pub async fn can_register_with_safe<Rpc: HoprRpcOperations>(
    me: Address,
    safe_address: Address,
    rpc: &Rpc,
) -> Result<bool> {
    let target_address = rpc.get_module_target_address().await?;
    debug!(node_address = %me, %safe_address, %target_address, "can register with safe");

    if target_address != safe_address {
        // cannot proceed when the safe address is not the target/owner of given module
        return Err(HoprChainError::Api("safe is not the module target".into()));
    }

    let registered_address = rpc.get_safe_from_node_safe_registry(me).await?;
    info!(%registered_address, "currently registered Safe address in NodeSafeRegistry");

    if registered_address.is_zero() {
        info!("Node is not associated with a Safe in NodeSafeRegistry yet");
        Ok(true)
    } else if registered_address != safe_address {
        Err(HoprChainError::Api(
            "Node is associated with a different Safe in NodeSafeRegistry".into(),
        ))
    } else {
        info!("Node is associated with correct Safe in NodeSafeRegistry");
        Ok(false)
    }
}

/// Waits until the given address is funded.
///
/// This is done by querying the RPC provider for balance with backoff until `max_delay` argument.
pub async fn wait_for_funds<Rpc: HoprRpcOperations>(
    address: Address,
    min_balance: Balance,
    max_delay: Duration,
    rpc: &Rpc,
) -> Result<()> {
    let multiplier = 1.05;
    let mut current_delay = Duration::from_secs(2).min(max_delay);

    while current_delay <= max_delay {
        match rpc.get_balance(address, min_balance.balance_type()).await {
            Ok(current_balance) => {
                info!(balance = %current_balance, "balance status");
                if current_balance.ge(&min_balance) {
                    info!("node is funded");
                    return Ok(());
                } else {
                    warn!("still unfunded, trying again soon");
                }
            }
            Err(e) => error!(error = %e, "failed to fetch balance from the chain"),
        }

        sleep(current_delay).await;
        current_delay = current_delay.mul_f64(multiplier);
    }

    Err(HoprChainError::Api("timeout waiting for funds".into()))
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum HoprChainProcess {
    Indexer,
    OutgoingOnchainActionQueue,
}

type ActionQueueType<T> = ActionQueue<
    T,
    IndexerActionTracker,
    EthereumTransactionExecutor<
        hopr_chain_rpc::TypedTransaction,
        RpcEthereumClient<
            RpcOperations<
                hopr_chain_rpc::client::JsonRpcProviderClient<DefaultHttpRequestor, SimpleJsonRpcRetryPolicy>,
                DefaultHttpRequestor,
            >,
        >,
        SafePayloadGenerator,
    >,
>;

/// Represents all chain interactions exported to be used in the hopr-lib
///
/// NOTE: instead of creating a unified interface the [HoprChain] exports
/// some functionality (e.g. the [ChainActions] as a referentially used)
/// object. This behavior will be refactored and hidden behind a trait
/// in the future implementations.
#[derive(Debug, Clone)]
pub struct HoprChain<T: HoprDbAllOperations + Send + Sync + Clone + std::fmt::Debug> {
    me_onchain: ChainKeypair,
    safe_address: Address,
    contract_addresses: ContractAddresses,
    indexer_cfg: IndexerConfig,
    indexer_events_tx: async_channel::Sender<SignificantChainEvent>,
    db: T,
    hopr_chain_actions: ChainActions<T>,
    action_queue: ActionQueueType<T>,
    action_state: Arc<IndexerActionTracker>,
    rpc_operations: RpcOperations<JsonRpcClient, DefaultHttpRequestor>,
}

impl<T: HoprDbAllOperations + Send + Sync + Clone + std::fmt::Debug + 'static> HoprChain<T> {
    #[allow(clippy::too_many_arguments)] // TODO: refactor this function into a reasonable group of components once fully rearchitected
    pub fn new(
        me_onchain: ChainKeypair,
        db: T,
        // --
        chain_config: ChainNetworkConfig,
        module_address: Address,
        // --
        contract_addresses: ContractAddresses,
        safe_address: Address,
        indexer_cfg: IndexerConfig,
        indexer_events_tx: async_channel::Sender<SignificantChainEvent>,
    ) -> Self {
        // TODO: extract this from the global config type
        let mut rpc_http_config = hopr_chain_rpc::HttpPostRequestorConfig::default();
        if let Some(max_rpc_req) = chain_config.max_requests_per_sec {
            rpc_http_config.max_requests_per_sec = Some(max_rpc_req); // override the default if set
        }

        // TODO: extract this from the global config type
        let rpc_http_retry_policy = SimpleJsonRpcRetryPolicy {
            min_retries: Some(2),
            ..SimpleJsonRpcRetryPolicy::default()
        };

        // TODO: extract this from the global config type
        let rpc_cfg = RpcOperationsConfig {
            chain_id: chain_config.chain.chain_id as u64,
            contract_addrs: contract_addresses,
            module_address,
            safe_address,
            expected_block_time: Duration::from_millis(chain_config.chain.block_time),
            tx_polling_interval: Duration::from_millis(chain_config.tx_polling_interval),
            finality: chain_config.confirmations,
            max_block_range_fetch_size: chain_config.max_block_range,
            ..Default::default()
        };

        // TODO: extract this from the global config type
        let rpc_client_cfg = RpcEthereumClientConfig::default();

        // TODO: extract this from the global config type
        let action_queue_cfg = ActionQueueConfig::default();

        // --- Configs done ---

        let requestor = DefaultHttpRequestor::new(rpc_http_config);

        // Build JSON RPC client
        let rpc_client = JsonRpcClient::new(
            &chain_config.chain.default_provider,
            requestor.clone(),
            rpc_http_retry_policy,
        );

        // Build RPC operations
        let rpc_operations =
            RpcOperations::new(rpc_client, requestor, &me_onchain, rpc_cfg).expect("failed to initialize RPC");

        // Build the Ethereum Transaction Executor that uses RpcOperations as backend
        let ethereum_tx_executor = EthereumTransactionExecutor::new(
            RpcEthereumClient::new(rpc_operations.clone(), rpc_client_cfg),
            SafePayloadGenerator::new(&me_onchain, contract_addresses, module_address),
        );

        // Build the Action Queue
        let action_queue = ActionQueue::new(
            db.clone(),
            IndexerActionTracker::default(),
            ethereum_tx_executor,
            action_queue_cfg,
        );

        let action_state = action_queue.action_state();
        let action_sender = action_queue.new_sender();

        // Instantiate Chain Actions
        let hopr_chain_actions = ChainActions::new(&me_onchain, db.clone(), action_sender);

        Self {
            me_onchain,
            safe_address,
            contract_addresses,
            indexer_cfg,
            indexer_events_tx,
            db,
            hopr_chain_actions,
            action_queue,
            action_state,
            rpc_operations,
        }
    }

    /// Execute all processes of the [`HoprChain`] object.
    ///
    /// This method will spawn the [`HoprChainProcess::Indexer`] and [`HoprChainProcess::OutgoingOnchainActionQueue`]
    /// processes and return join handles to the calling function.
    pub async fn start(&self) -> errors::Result<HashMap<HoprChainProcess, JoinHandle<()>>> {
        let mut processes: HashMap<HoprChainProcess, JoinHandle<()>> = HashMap::new();

        processes.insert(
            HoprChainProcess::OutgoingOnchainActionQueue,
            spawn(self.action_queue.clone().start()),
        );
        processes.insert(
            HoprChainProcess::Indexer,
            Indexer::new(
                self.rpc_operations.clone(),
                ContractEventHandlers::new(
                    self.contract_addresses,
                    self.safe_address,
                    self.me_onchain.clone(),
                    self.db.clone(),
                ),
                self.db.clone(),
                self.indexer_cfg,
                self.indexer_events_tx.clone(),
            )
            .start()
            .await?,
        );

        Ok(processes)
    }

    pub fn me_onchain(&self) -> Address {
        self.me_onchain.public().to_address()
    }

    pub fn action_state(&self) -> Arc<IndexerActionTracker> {
        self.action_state.clone()
    }

    pub async fn accounts_announced_on_chain(&self) -> errors::Result<Vec<AccountEntry>> {
        Ok(self.db.get_accounts(None, true).await?)
    }

    pub async fn channel(&self, src: &Address, dest: &Address) -> errors::Result<ChannelEntry> {
        self.db
            .get_channel_by_parties(None, src, dest, false)
            .await
            .map_err(HoprChainError::from)
            .and_then(|v| {
                v.ok_or(errors::HoprChainError::Api(format!(
                    "Channel entry not available {}-{}",
                    src, dest
                )))
            })
    }

    pub async fn channels_from(&self, src: &Address) -> errors::Result<Vec<ChannelEntry>> {
        Ok(self.db.get_channels_via(None, ChannelDirection::Outgoing, src).await?)
    }

    pub async fn channels_to(&self, dest: &Address) -> errors::Result<Vec<ChannelEntry>> {
        Ok(self.db.get_channels_via(None, ChannelDirection::Incoming, dest).await?)
    }

    pub async fn all_channels(&self) -> errors::Result<Vec<ChannelEntry>> {
        Ok(self.db.get_all_channels(None).await?)
    }

    pub async fn ticket_price(&self) -> errors::Result<Option<U256>> {
        Ok(self.db.get_indexer_data(None).await?.ticket_price.map(|b| b.amount()))
    }

    pub async fn safe_allowance(&self) -> errors::Result<Balance> {
        Ok(self.db.get_safe_hopr_allowance(None).await?)
    }

    pub fn actions_ref(&self) -> &ChainActions<T> {
        &self.hopr_chain_actions
    }

    pub fn actions_mut_ref(&mut self) -> &mut ChainActions<T> {
        &mut self.hopr_chain_actions
    }

    pub fn rpc(&self) -> &RpcOperations<JsonRpcClient, DefaultHttpRequestor> {
        &self.rpc_operations
    }

    pub async fn get_balance(&self, balance_type: BalanceType) -> errors::Result<Balance> {
        Ok(self.rpc_operations.get_balance(self.me_onchain(), balance_type).await?)
    }

    pub async fn get_safe_balance(&self, safe_address: Address, balance_type: BalanceType) -> errors::Result<Balance> {
        Ok(self.rpc_operations.get_balance(safe_address, balance_type).await?)
    }

    pub async fn get_channel_closure_notice_period(&self) -> errors::Result<Duration> {
        Ok(self.rpc_operations.get_channel_closure_notice_period().await?)
    }

    pub async fn get_eligibility_status(&self) -> errors::Result<bool> {
        Ok(self.rpc_operations.get_eligibility_status(self.me_onchain()).await?)
    }

    pub async fn get_minimum_winning_probability(&self) -> errors::Result<f64> {
        Ok(self.rpc_operations.get_minimum_network_winning_probability().await?)
    }

    pub async fn get_minimum_ticket_price(&self) -> errors::Result<Balance> {
        Ok(self.rpc_operations.get_minimum_network_ticket_price().await?)
    }
}
