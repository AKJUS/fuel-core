use async_graphql::Context;
use fuel_core_storage::{
    Error as StorageError,
    IsNotFound,
};
use std::{
    net::SocketAddr,
    sync::OnceLock,
    time::Duration,
};

pub mod api_service;
pub(crate) mod block_height_subscription;
pub mod database;
pub(crate) mod extensions;
pub(crate) mod indexation;
pub mod ports;
pub mod storage;
pub mod worker_service;

#[derive(Clone, Debug)]
pub struct Config {
    pub config: ServiceConfig,
    pub utxo_validation: bool,
    pub debug: bool,
    pub historical_execution: bool,
    pub max_tx: usize,
    pub max_gas: u64,
    pub max_size: usize,
    pub max_txpool_dependency_chain_length: usize,
    pub chain_name: String,
}

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub addr: SocketAddr,
    pub number_of_threads: usize,
    pub database_batch_size: usize,
    pub max_queries_depth: usize,
    pub max_queries_complexity: usize,
    pub max_queries_recursive_depth: usize,
    pub max_queries_resolver_recursive_depth: usize,
    pub max_queries_directives: usize,
    pub max_concurrent_queries: usize,
    pub request_body_bytes_limit: usize,
    /// Number of blocks that the node can be lagging behind the required fuel block height
    /// before it will be considered out of sync.
    pub required_fuel_block_height_tolerance: u32,
    /// The time to wait before dropping the request if the node is lagging behind the required
    /// fuel block height.
    pub required_fuel_block_height_timeout: Duration,
    /// Time to wait after submitting a query before debug info will be logged about query.
    pub query_log_threshold_time: Duration,
    pub api_request_timeout: Duration,
    pub assemble_tx_dry_run_limit: usize,
    pub assemble_tx_estimate_predicates_limit: usize,
    /// Configurable cost parameters to limit graphql queries complexity
    pub costs: Costs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Costs {
    pub balance_query: usize,
    pub coins_to_spend: usize,
    pub get_peers: usize,
    pub estimate_predicates: usize,
    pub assemble_tx: usize,
    pub dry_run: usize,
    pub storage_read_replay: usize,
    pub submit: usize,
    pub submit_and_await: usize,
    pub status_change: usize,
    pub storage_read: usize,
    pub tx_get: usize,
    pub tx_status_read: usize,
    pub tx_raw_payload: usize,
    pub block_header: usize,
    pub block_transactions: usize,
    pub block_transactions_ids: usize,
    pub storage_iterator: usize,
    pub bytecode_read: usize,
    pub state_transition_bytecode_read: usize,
    pub da_compressed_block_read: usize,
}

#[cfg(feature = "test-helpers")]
impl Default for Costs {
    fn default() -> Self {
        DEFAULT_QUERY_COSTS
    }
}

const BALANCES_QUERY_COST_WITH_INDEXATION: usize = 0;
const BALANCES_QUERY_COST_WITHOUT_INDEXATION: usize = 40001;

pub const DEFAULT_QUERY_COSTS: Costs = Costs {
    balance_query: BALANCES_QUERY_COST_WITH_INDEXATION,
    coins_to_spend: 40001,
    get_peers: 40001,
    estimate_predicates: 40001,
    dry_run: 12000,
    assemble_tx: 76_000,
    storage_read_replay: 40001,
    submit: 40001,
    submit_and_await: 40001,
    status_change: 40001,
    storage_read: 40,
    tx_get: 50,
    tx_status_read: 50,
    tx_raw_payload: 150,
    block_header: 150,
    block_transactions: 1500,
    block_transactions_ids: 50,
    storage_iterator: 100,
    bytecode_read: 8000,
    state_transition_bytecode_read: 76_000,
    da_compressed_block_read: 4000,
};

pub fn query_costs() -> &'static Costs {
    QUERY_COSTS.get().unwrap_or(&DEFAULT_QUERY_COSTS)
}

pub static QUERY_COSTS: OnceLock<Costs> = OnceLock::new();

#[cfg(feature = "test-helpers")]
fn default_query_costs(balances_indexation_enabled: bool) -> Costs {
    let mut cost = DEFAULT_QUERY_COSTS;

    if !balances_indexation_enabled {
        cost.balance_query = BALANCES_QUERY_COST_WITHOUT_INDEXATION;
    }

    cost
}

fn initialize_query_costs(
    costs: Costs,
    _balances_indexation_enabled: bool,
) -> anyhow::Result<()> {
    #[cfg(feature = "test-helpers")]
    if costs != default_query_costs(_balances_indexation_enabled) {
        // We don't support setting these values in test contexts, because
        // it can lead to unexpected behavior if multiple tests try to
        // initialize different values.
        anyhow::bail!("cannot initialize queries with non-default costs in tests")
    }

    QUERY_COSTS.get_or_init(|| costs);

    Ok(())
}

pub trait IntoApiResult<T> {
    fn into_api_result<NewT, E>(self) -> Result<Option<NewT>, E>
    where
        NewT: From<T>,
        E: From<StorageError>;
}

impl<T> IntoApiResult<T> for Result<T, StorageError> {
    fn into_api_result<NewT, E>(self) -> Result<Option<NewT>, E>
    where
        NewT: From<T>,
        E: From<StorageError>,
    {
        if self.is_not_found() {
            Ok(None)
        } else {
            Ok(Some(self?.into()))
        }
    }
}

pub fn require_historical_execution(ctx: &Context<'_>) -> async_graphql::Result<()> {
    let config = ctx.data_unchecked::<Config>();

    if config.historical_execution {
        Ok(())
    } else {
        Err(async_graphql::Error::new(
            "`--historical-execution` is required for this operation",
        ))
    }
}
