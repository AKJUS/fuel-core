use clap::Parser;
use ethers::{
    providers::Middleware,
    types::{
        Log,
        SyncingStatus,
        U256,
    },
};
use fuel_core::{
    chain_config::Randomize,
    coins_query::CoinsQueryError::{
        self,
        InsufficientCoins,
    },
    combined_database::CombinedDatabase,
    database::Database,
    fuel_core_graphql_api::storage::relayed_transactions::RelayedTransactionStatuses,
    relayer,
    service::{
        Config,
        FuelService,
    },
    state::{
        historical_rocksdb::StateRewindPolicy,
        rocks_db::{
            ColumnsPolicy,
            DatabaseConfig,
        },
    },
};
use fuel_core_client::client::{
    FuelClient,
    pagination::{
        PageDirection,
        PaginationRequest,
    },
    types::{
        CoinType,
        RelayedTransactionStatus as ClientRelayedTransactionStatus,
        TransactionStatus,
    },
};
use fuel_core_poa::service::Mode;
use fuel_core_relayer::{
    ports::Transactional,
    test_helpers::{
        EvtToLog,
        LogTestHelper,
        middleware::MockMiddleware,
    },
};
use fuel_core_storage::{
    StorageAsMut,
    StorageAsRef,
    tables::Messages,
};
use fuel_core_types::{
    entities::relayer::transaction::RelayedTransactionStatus as FuelRelayedTransactionStatus,
    fuel_asm::*,
    fuel_crypto::*,
    fuel_tx::*,
    fuel_types::{
        BlockHeight,
        Nonce,
    },
};
use fuel_types::Bytes20;
use hyper::{
    Body,
    Request,
    Response,
    Server,
    service::{
        make_service_fn,
        service_fn,
    },
};
use rand::{
    Rng,
    SeedableRng,
    prelude::StdRng,
};
use serde_json::json;
use std::{
    convert::Infallible,
    net::{
        Ipv4Addr,
        SocketAddr,
    },
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use test_helpers::{
    assemble_tx::{
        AssembleAndRunTx,
        SigningAccount,
    },
    config_with_fee,
    default_signing_wallet,
    fuel_core_driver::FuelCoreDriver,
};
use tokio::sync::oneshot::Sender;

enum MessageKind {
    Retryable { nonce: u64, amount: u64 },
    NonRetryable { nonce: u64, amount: u64 },
}

#[tokio::test(flavor = "multi_thread")]
async fn relayer_can_download_logs() {
    let mut config = Config::local_node();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];
    let message = |nonce, block_number: u64| {
        make_message_event(
            Nonce::from(nonce),
            block_number,
            contract_address,
            None,
            None,
            None,
            None,
            0,
        )
    };

    let logs = vec![message(1, 3), message(2, 5)];
    let expected_messages: Vec<_> = logs.iter().map(|l| l.to_msg()).collect();
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(vec![
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    ]);
    let db = Database::in_memory();

    let srv = FuelService::from_database(db.clone(), config)
        .await
        .unwrap();

    // wait for relayer to catch up
    srv.await_relayer_synced().await.unwrap();
    // Wait for the block producer to create a block that targets the latest da height.
    srv.shared
        .poa_adapter
        .manually_produce_blocks(
            None,
            Mode::Blocks {
                number_of_blocks: 1,
            },
        )
        .await
        .unwrap();

    // check the db for downloaded messages
    for msg in expected_messages {
        assert_eq!(
            *db.storage::<Messages>().get(msg.id()).unwrap().unwrap(),
            msg
        );
    }
    srv.send_stop_signal_and_await_shutdown().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_are_spendable_after_relayer_is_synced() {
    let mut rng = StdRng::seed_from_u64(1234);
    let mut config = config_with_fee();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];

    // setup a real spendable message
    let secret_key: SecretKey = SecretKey::random(&mut rng);
    let pk = secret_key.public_key();
    let recipient = Input::owner(&pk);
    let sender = Address::zeroed();
    let amount = 100;
    let nonce = Nonce::from(2u64);
    let logs = vec![make_message_event(
        nonce,
        5,
        contract_address,
        Some(sender.into()),
        Some(recipient.into()),
        Some(amount),
        None,
        0,
    )];
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(vec![
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    ]);

    // setup fuel node with mocked eth url
    let db = Database::in_memory();

    let srv = FuelService::from_database(db.clone(), config)
        .await
        .unwrap();

    let client = FuelClient::from(srv.bound_address);

    // wait for relayer to catch up to eth node
    srv.await_relayer_synced().await.unwrap();
    // Wait for the block producer to create a block that targets the latest da height.
    client.produce_blocks(1, None).await.unwrap();

    // verify we have downloaded the message
    let query = client
        .messages(
            None,
            PaginationRequest {
                cursor: None,
                results: 1,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    // we should have one message before spending
    assert_eq!(query.results.len(), 1);

    // attempt to spend the message downloaded from the relayer
    let status = client
        .run_script(vec![op::ret(0)], vec![], SigningAccount::Wallet(secret_key))
        .await
        .unwrap();

    // verify transaction executed successfully
    assert!(
        matches!(&status, &TransactionStatus::Success { .. }),
        "{:?}",
        &status
    );

    // verify message state is spent
    let query = client
        .messages(
            None,
            PaginationRequest {
                cursor: None,
                results: 1,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    // there should be no messages after spending
    assert_eq!(query.results.len(), 0);

    srv.send_stop_signal_and_await_shutdown().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn can_find_failed_relayed_tx() {
    let mut db = CombinedDatabase::in_memory();
    let id = [1; 32].into();
    let block_height: BlockHeight = 999.into();
    let failure = "lolz".to_string();

    // given
    let status = FuelRelayedTransactionStatus::Failed {
        block_height,
        failure: failure.clone(),
    };
    db.off_chain_mut()
        .storage_as_mut::<RelayedTransactionStatuses>()
        .insert(&id, &status)
        .unwrap();

    // when
    let srv = FuelService::from_combined_database(db.clone(), Config::local_node())
        .await
        .unwrap();
    let client = FuelClient::from(srv.bound_address);

    // then
    let expected = Some(ClientRelayedTransactionStatus::Failed {
        block_height,
        failure,
    });
    let actual = client.relayed_transaction_status(&id).await.unwrap();
    assert_eq!(expected, actual);
}

#[tokio::test(flavor = "multi_thread")]
async fn can_restart_node_with_relayer_data() {
    let mut rng = StdRng::seed_from_u64(1234);
    let mut config = config_with_fee();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];

    // setup a real spendable message
    let secret_key: SecretKey = SecretKey::random(&mut rng);
    let pk = secret_key.public_key();
    let recipient = Input::owner(&pk);
    let sender = Address::zeroed();
    let amount = 100;
    let nonce = Nonce::from(2u64);
    let logs = vec![make_message_event(
        nonce,
        5,
        contract_address,
        Some(sender.into()),
        Some(recipient.into()),
        Some(amount),
        None,
        0,
    )];
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(vec![
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    ]);

    let tmp_dir = tempfile::TempDir::new().unwrap();

    {
        // Given
        let database = CombinedDatabase::open(
            tmp_dir.path(),
            Default::default(),
            DatabaseConfig::config_for_tests(),
        )
        .unwrap();

        let service = FuelService::from_combined_database(database, config.clone())
            .await
            .unwrap();
        let client = FuelClient::from(service.bound_address);
        client.health().await.unwrap();

        for _ in 0..5 {
            client
                .run_script(vec![op::ret(1)], vec![], default_signing_wallet())
                .await
                .unwrap();
        }

        service.send_stop_signal_and_await_shutdown().await.unwrap();
    }

    {
        // When
        let database = CombinedDatabase::open(
            tmp_dir.path(),
            Default::default(),
            DatabaseConfig::config_for_tests(),
        )
        .unwrap();
        let service = FuelService::from_combined_database(database, config)
            .await
            .unwrap();
        let client = FuelClient::from(service.bound_address);

        // Then
        client.health().await.unwrap();
        service.send_stop_signal_and_await_shutdown().await.unwrap();
    }
}

#[allow(clippy::too_many_arguments)]
fn make_message_event(
    nonce: Nonce,
    block_number: u64,
    contract_address: Bytes20,
    sender: Option<[u8; 32]>,
    recipient: Option<[u8; 32]>,
    amount: Option<u64>,
    data: Option<Vec<u8>>,
    log_index: u64,
) -> Log {
    let message = fuel_core_relayer::bridge::MessageSentFilter {
        nonce: U256::from_big_endian(nonce.as_ref()),
        sender: sender.unwrap_or_default(),
        recipient: recipient.unwrap_or_default(),
        amount: amount.unwrap_or_default(),
        data: data.map(Into::into).unwrap_or_default(),
    };
    let mut log = message.into_log();
    log.address =
        fuel_core_relayer::test_helpers::convert_to_address(contract_address.as_slice());
    log.block_number = Some(block_number.into());
    log.log_index = Some(log_index.into());
    log
}

async fn spawn_eth_node(eth_node: Arc<MockMiddleware>) -> EthNodeHandle {
    // Construct our SocketAddr to listen on...
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

    // And a MakeService to handle each connection...
    let make_service = make_service_fn(move |_conn| {
        let eth_node = eth_node.clone();
        async move {
            Ok::<_, Infallible>(service_fn({
                let eth_node = eth_node.clone();
                move |req| handle(eth_node.clone(), req)
            }))
        }
    });

    // Then bind and serve...
    let server = Server::bind(&addr).serve(make_service);
    let addr = server.local_addr();

    let (shutdown, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let graceful = server.with_graceful_shutdown(async {
            rx.await.ok();
        });
        // And run forever...
        if let Err(e) = graceful.await {
            eprintln!("server error: {e}");
        }
    });
    EthNodeHandle {
        shutdown,
        address: addr,
    }
}

pub(crate) struct EthNodeHandle {
    pub(crate) shutdown: Sender<()>,
    pub(crate) address: SocketAddr,
}

async fn handle(
    mock: Arc<MockMiddleware>,
    req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let body = hyper::body::to_bytes(req).await.unwrap();

    let v: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();
    let mut o = match v {
        serde_json::Value::Object(o) => o,
        _ => unreachable!(),
    };
    let id = o.get("id").unwrap().as_u64().unwrap();
    let method = o.get("method").unwrap().as_str().unwrap();
    let r = match method {
        "eth_getBlockByNumber" => {
            let r = mock.get_block(id).await.unwrap().unwrap();
            json!({ "id": id, "jsonrpc": "2.0", "result": r })
        }
        "eth_syncing" => {
            let r = mock.syncing().await.unwrap();
            match r {
                SyncingStatus::IsFalse => {
                    json!({ "id": id, "jsonrpc": "2.0", "result": false })
                }
                SyncingStatus::IsSyncing(status) => {
                    json!({ "id": id, "jsonrpc": "2.0", "result": {
                        "starting_block": status.starting_block,
                        "current_block": status.current_block,
                        "highest_block": status.highest_block,
                    } })
                }
            }
        }
        "eth_getLogs" => {
            let params = o.remove("params").unwrap();
            let params: Vec<_> = serde_json::from_value(params).unwrap();
            let r = mock.get_logs(&params[0]).await.unwrap();
            json!({ "id": id, "jsonrpc": "2.0", "result": r })
        }
        _ => unreachable!("Mock handler for method not defined"),
    };

    let r = serde_json::to_vec(&r).unwrap();

    Ok(Response::new(Body::from(r)))
}

trait ToStdErrorString {
    fn to_str_error_string(self) -> String;
}
impl ToStdErrorString for CoinsQueryError {
    fn to_str_error_string(self) -> String {
        fuel_core_client::client::from_strings_errors_to_std_error(vec![self.to_string()])
            .to_string()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn balances_and_coins_to_spend_never_return_retryable_messages() {
    let mut rng = StdRng::seed_from_u64(1234);
    let mut config = Config::local_node();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];
    const TIMEOUT: Duration = Duration::from_secs(1);

    // Large enough to get all messages, but not to trigger the "query is too complex" error.
    const UNLIMITED_QUERY_RESULTS: i32 = 100;

    // Given

    // setup a retryable and non-retryable message
    let secret_key: SecretKey = SecretKey::random(&mut rng);
    let public_key = secret_key.public_key();
    let recipient = Input::owner(&public_key);

    const RETRYABLE_AMOUNT: u64 = 99;
    const RETRYABLE_NONCE: u64 = 0;
    const NON_RETRYABLE_AMOUNT: u64 = 100;
    const NON_RETRYABLE_NONCE: u64 = 1;
    let messages = vec![
        MessageKind::Retryable {
            nonce: RETRYABLE_NONCE,
            amount: RETRYABLE_AMOUNT,
        },
        MessageKind::NonRetryable {
            nonce: NON_RETRYABLE_NONCE,
            amount: NON_RETRYABLE_AMOUNT,
        },
    ];
    let logs: Vec<_> = setup_messages(&messages, &recipient, &contract_address);

    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(vec![
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    ]);

    config.utxo_validation = true;

    // setup fuel node with mocked eth url
    let db = Database::in_memory();

    let srv = FuelService::from_database(db.clone(), config)
        .await
        .unwrap();

    let client = FuelClient::from(srv.bound_address);
    let base_asset_id = *client
        .consensus_parameters(0)
        .await
        .unwrap()
        .unwrap()
        .base_asset_id();

    // When

    // wait for relayer to catch up to eth node
    srv.await_relayer_synced().await.unwrap();
    // Wait for the block producer to create a block that targets the latest da height.
    srv.shared
        .poa_adapter
        .manually_produce_blocks(
            None,
            Mode::Blocks {
                number_of_blocks: 1,
            },
        )
        .await
        .unwrap();

    // Balances are processed in the off-chain worker, so we need to wait for it
    // to process the messages before we can assert the balances.
    let result = tokio::time::timeout(TIMEOUT, async {
        loop {
            let query = client
                .balances(
                    &recipient,
                    PaginationRequest {
                        cursor: None,
                        results: UNLIMITED_QUERY_RESULTS,
                        direction: PageDirection::Forward,
                    },
                )
                .await
                .unwrap();

            if !query.results.is_empty() {
                break;
            }
        }
    })
    .await;
    if result.is_err() {
        panic!("Off-chain worker didn't process balances within timeout")
    }

    // Then

    // Expect two messages to be available
    let query = client
        .messages(
            None,
            PaginationRequest {
                cursor: None,
                results: UNLIMITED_QUERY_RESULTS,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    assert_eq!(query.results.len(), 2);
    let total_amount = query.results.iter().map(|m| m.amount).sum::<u64>();
    assert_eq!(total_amount, NON_RETRYABLE_AMOUNT + RETRYABLE_AMOUNT);

    // Expect only the non-retryable message balance to be returned via "balance"
    let query = client
        .balance(&recipient, Some(&base_asset_id))
        .await
        .unwrap();
    assert_eq!(query, NON_RETRYABLE_AMOUNT as u128);

    // Expect only the non-retryable message balance to be returned via "balances"
    let query = client
        .balances(
            &recipient,
            PaginationRequest {
                cursor: None,
                results: UNLIMITED_QUERY_RESULTS,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    assert_eq!(query.results.len(), 1);
    let total_amount = query
        .results
        .iter()
        .map(|m| {
            assert_eq!(m.asset_id, base_asset_id);
            m.amount
        })
        .sum::<u128>();
    assert_eq!(total_amount, NON_RETRYABLE_AMOUNT as u128);

    // Expect only the non-retryable message balance to be returned via "coins to spend"
    let query = client
        .coins_to_spend(
            &recipient,
            vec![(base_asset_id, NON_RETRYABLE_AMOUNT as u128, None)],
            None,
        )
        .await
        .unwrap();
    let message_coins: Vec<_> = query
        .iter()
        .flatten()
        .map(|m| {
            let CoinType::MessageCoin(m) = m else {
                panic!("should have message coin")
            };
            m
        })
        .collect();
    assert_eq!(message_coins.len(), 1);
    assert_eq!(message_coins[0].amount, NON_RETRYABLE_AMOUNT);
    assert_eq!(message_coins[0].nonce, NON_RETRYABLE_NONCE.into());

    // Expect no messages when querying more than the available non-retryable amount
    let query = client
        .coins_to_spend(
            &recipient,
            vec![(base_asset_id, (NON_RETRYABLE_AMOUNT + 1) as u128, None)],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        query.to_string(),
        InsufficientCoins {
            owner: recipient,
            asset_id: base_asset_id,
            collected_amount: 100,
        }
        .to_str_error_string()
    );

    srv.send_stop_signal_and_await_shutdown().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();
}

#[tokio::test]
async fn relayer_db_can_be_rewinded() {
    // Given
    let rollback_target_height = 0;
    let num_da_blocks = 10;
    let mut rng = StdRng::seed_from_u64(1234);
    let mut config = config_with_fee();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];

    let logs: Vec<_> = (1..=num_da_blocks)
        .map(|block_height| {
            make_message_event(
                Nonce::randomize(&mut rng),
                block_height,
                contract_address,
                Some(rng.r#gen()),
                Some(rng.r#gen()),
                Some(rng.r#gen()),
                None,
                0,
            )
        })
        .collect();
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    eth_node.update_data(|data| data.best_block.number = Some(num_da_blocks.into()));

    let eth_node_handle = spawn_eth_node(Arc::new(eth_node)).await;

    let relayer_url = format!("http://{}", eth_node_handle.address);

    let tmp_dir = TempDir::new().unwrap();
    let open_db = |tmp_dir: &TempDir| {
        CombinedDatabase::open(
            tmp_dir.path(),
            StateRewindPolicy::RewindFullRange,
            DatabaseConfig {
                cache_capacity: Some(16 * 1024 * 1024 * 1024),
                max_fds: -1,
                columns_policy: ColumnsPolicy::Lazy,
            },
        )
        .expect("Failed to create database")
    };

    let driver = FuelCoreDriver::spawn_feeless_with_directory(
        tmp_dir,
        &[
            "--debug",
            "--poa-instant",
            "true",
            "--state-rewind-duration",
            "7d",
            "--enable-relayer",
            "--relayer",
            &relayer_url,
        ],
    )
    .await
    .unwrap();
    let srv = &driver.node;

    let client = FuelClient::from(srv.bound_address);

    srv.await_relayer_synced().await.unwrap();
    client.produce_blocks(1, None).await.unwrap();

    srv.send_stop_signal_and_await_shutdown().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();

    // When
    let tmp_dir = driver.kill().await;

    let db = open_db(&tmp_dir);
    let relayer_block_height_before_rollback = db.relayer().latest_da_height();
    db.shutdown();

    let target_block_height = rollback_target_height.to_string();
    let target_da_block_height = rollback_target_height.to_string();
    let args = [
        "_IGNORED_",
        "--db-path",
        tmp_dir.path().to_str().unwrap(),
        "--target-block-height",
        target_block_height.as_str(),
        "--target-da-block-height",
        target_da_block_height.as_str(),
    ];

    let command = fuel_core_bin::cli::rollback::Command::parse_from(args);
    fuel_core_bin::cli::rollback::exec(command).await.unwrap();

    let db = open_db(&tmp_dir);
    let relayer_block_height_after_rollback =
        db.relayer().latest_height_from_metadata().unwrap();

    // Then
    assert_eq!(
        relayer_block_height_before_rollback.unwrap().as_u64(),
        num_da_blocks
    );
    assert!(relayer_block_height_after_rollback.is_none());
}

fn setup_messages(
    messages: &[MessageKind],
    recipient: &Address,
    contract_address: &Bytes20,
) -> Vec<Log> {
    const SENDER: Address = Address::zeroed();

    messages
        .iter()
        .map(|m| match m {
            MessageKind::Retryable { nonce, amount } => make_message_event(
                Nonce::from(*nonce),
                5,
                *contract_address,
                Some(SENDER.into()),
                Some((*recipient).into()),
                Some(*amount),
                Some(vec![1]),
                0,
            ),
            MessageKind::NonRetryable { nonce, amount } => make_message_event(
                Nonce::from(*nonce),
                5,
                *contract_address,
                Some(SENDER.into()),
                Some((*recipient).into()),
                Some(*amount),
                None,
                0,
            ),
        })
        .collect()
}
