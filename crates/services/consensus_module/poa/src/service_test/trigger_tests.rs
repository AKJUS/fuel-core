use mockall::Sequence;
use tokio::{
    sync::Notify,
    time::Instant,
};

use super::*;

#[tokio::test]
async fn clean_startup_shutdown_each_trigger() -> anyhow::Result<()> {
    for trigger in [
        Trigger::Never,
        Trigger::Instant,
        Trigger::Interval {
            block_time: Duration::new(1, 0),
        },
    ] {
        let mut ctx_builder = TestContextBuilder::new();
        ctx_builder.with_config(Config {
            trigger,
            signer: SignMode::Key(test_signing_key()),
            metrics: false,
            ..Default::default()
        });
        let ctx = ctx_builder.build().await;

        assert_eq!(ctx.stop().await, State::Stopped);
    }

    Ok(())
}

#[tokio::test]
async fn never_trigger_never_produces_blocks() {
    const TX_COUNT: usize = 10;
    let mut rng = StdRng::seed_from_u64(1234u64);
    let mut ctx_builder = TestContextBuilder::new();
    ctx_builder.with_config(Config {
        trigger: Trigger::Never,
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    });

    // initialize txpool with some txs
    let txs = (0..TX_COUNT).map(|_| make_tx(&mut rng)).collect::<Vec<_>>();
    let TxPoolContext {
        txpool,
        new_txs_notifier,
        ..
    } = MockTransactionPool::new_with_txs(txs.clone());
    ctx_builder.with_txpool(txpool);

    let mut importer = MockBlockImporter::default();
    importer
        .expect_commit_result()
        .returning(|_| panic!("Should not commit result"));
    importer
        .expect_block_stream()
        .returning(|| Box::pin(tokio_stream::pending()));
    ctx_builder.with_importer(importer);
    let ctx = ctx_builder.build().await;
    new_txs_notifier.send_replace(());

    // Make sure enough time passes for the block to be produced
    time::sleep(Duration::new(10, 0)).await;

    // Stop
    assert_eq!(ctx.stop().await, State::Stopped);
}

struct DefaultContext {
    rng: StdRng,
    test_ctx: TestContext,
    block_import: broadcast::Receiver<SealedBlock>,
    new_txs_notifier: watch::Sender<()>,
    txs: Arc<StdMutex<Vec<Script>>>,
}

impl DefaultContext {
    async fn new(config: Config) -> Self {
        let mut rng = StdRng::seed_from_u64(1234u64);
        let mut ctx_builder = TestContextBuilder::new();
        ctx_builder.with_config(config);
        // initialize txpool with some txs
        let tx1 = make_tx(&mut rng);
        let TxPoolContext {
            txpool,
            new_txs_notifier,
            txs,
        } = MockTransactionPool::new_with_txs(vec![tx1]);
        ctx_builder.with_txpool(txpool);

        let (block_import_sender, block_import_receiver) = broadcast::channel(100);
        let mut importer = MockBlockImporter::default();
        importer.expect_commit_result().returning(move |result| {
            let (result, _) = result.into();
            let sealed_block = result.sealed_block;
            block_import_sender.send(sealed_block)?;
            Ok(())
        });
        importer
            .expect_block_stream()
            .returning(|| Box::pin(tokio_stream::pending()));

        let mut block_producer = MockBlockProducer::default();
        block_producer
            .expect_produce_and_execute_block()
            .returning(|_, time, _, _| {
                let mut block = Block::default();
                block.header_mut().set_time(time);
                block.header_mut().recalculate_metadata();
                Ok(UncommittedResult::new(
                    ExecutionResult {
                        block,
                        ..Default::default()
                    },
                    Default::default(),
                ))
            });

        ctx_builder.with_importer(importer);
        ctx_builder.with_producer(block_producer);

        let test_ctx = ctx_builder.build().await;

        Self {
            rng,
            test_ctx,
            block_import: block_import_receiver,
            new_txs_notifier,
            txs,
        }
    }

    fn now(&self) -> Tai64 {
        self.test_ctx.time.watch().now()
    }

    fn advance_time_with_tokio(&mut self) {
        self.test_ctx.time.advance_with_tokio();
    }

    fn advance_time(&mut self, duration: Duration) {
        self.test_ctx.time.advance(duration);
    }

    fn rewind_time(&mut self, duration: Duration) {
        self.test_ctx.time.rewind(duration);
    }
}

#[tokio::test]
async fn instant_trigger_produces_block_instantly() {
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Instant,
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;

    ctx.new_txs_notifier.send_replace(());

    // Make sure it's produced
    assert!(ctx.block_import.recv().await.is_ok());

    // Stop
    assert_eq!(ctx.test_ctx.stop().await, State::Stopped);
}

#[tokio::test]
async fn interval_trigger_produces_blocks_periodically() -> anyhow::Result<()> {
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval {
            block_time: Duration::new(2, 0),
        },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    ctx.new_txs_notifier.send_replace(());

    // Make sure no blocks are produced yet
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Pause time until a single block is produced, and a bit more
    time::sleep(Duration::new(3, 0)).await;

    // Make sure the empty block is actually produced
    assert!(ctx.block_import.try_recv().is_ok());
    // Emulate tx status update to trigger the execution
    ctx.new_txs_notifier.send_replace(());

    // Make sure no blocks are produced before next interval
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Pause time until a the next block is produced
    time::sleep(Duration::new(2, 0)).await;

    // Make sure it's produced
    assert!(ctx.block_import.try_recv().is_ok());

    // Emulate tx status update to trigger the execution
    ctx.new_txs_notifier.send_replace(());

    time::sleep(Duration::from_millis(1)).await;

    // Make sure blocks are not produced before the block time is used
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Pause time until a the next block is produced
    time::sleep(Duration::new(2, 0)).await;

    // Make sure only one block is produced
    assert!(ctx.block_import.try_recv().is_ok());
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Stop
    ctx.test_ctx.service.stop_and_await().await?;

    Ok(())
}

#[tokio::test]
async fn service__if_commit_result_fails_then_retry_commit_result_after_one_second()
-> anyhow::Result<()> {
    // given
    let config = Config {
        trigger: Trigger::Interval {
            block_time: Duration::new(2, 0),
        },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    };
    let block_production_waitpoint = Arc::new(Notify::new());
    let block_production_waitpoint_trigger = block_production_waitpoint.clone();

    let mut ctx_builder = TestContextBuilder::new();
    ctx_builder.with_config(config);
    let mock_tx_pool = MockTransactionPool::no_tx_updates();
    ctx_builder.with_txpool(mock_tx_pool);

    let mut importer = MockBlockImporter::default();
    let mut seq = Sequence::new();
    // First attempt fails
    importer
        .expect_commit_result()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| Err(anyhow::anyhow!("Error in production")));
    // Second attempt should be triggered after 1 second and success
    importer
        .expect_commit_result()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            block_production_waitpoint_trigger.notify_waiters();
            Ok(())
        });
    importer
        .expect_block_stream()
        .returning(|| Box::pin(tokio_stream::pending()));
    ctx_builder.with_importer(importer);
    let test_ctx = ctx_builder.build().await;

    let before_retry = Instant::now();

    // when
    block_production_waitpoint.notified().await;

    // then
    assert!(before_retry.elapsed() >= Duration::from_secs(1));

    test_ctx.service.stop_and_await().await?;
    Ok(())
}

#[tokio::test]
async fn interval_trigger_doesnt_react_to_full_txpool() -> anyhow::Result<()> {
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval {
            block_time: Duration::new(2, 0),
        },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;

    // Brackets to release the lock.
    {
        let mut guard = ctx.txs.lock().unwrap();
        // Fill txpool completely and notify about new transaction.
        for _ in 0..1_000 {
            guard.push(make_tx(&mut ctx.rng));
        }
        ctx.new_txs_notifier.send_replace(());
    }

    // Make sure blocks are not produced before the block time has elapsed
    time::sleep(Duration::new(1, 0)).await;
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Make sure only one block per round is produced
    for _ in 0..5 {
        time::sleep(Duration::new(2, 0)).await;
        assert!(ctx.block_import.try_recv().is_ok());
        assert!(matches!(
            ctx.block_import.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    // Stop
    ctx.test_ctx.service.stop_and_await().await?;

    Ok(())
}

#[tokio::test]
async fn interval_trigger_produces_blocks_in_the_future_when_time_is_lagging() {
    // Given

    let block_time = Duration::from_secs(10);
    let offset = Duration::from_secs(1);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval { block_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    ctx.new_txs_notifier.send_replace(());
    let start_time = ctx.now();

    // When

    // We produce three blocks without advancing real time.
    time::sleep(block_time * 3 + offset).await;
    let first_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();
    let second_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();
    let third_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // Then

    // We should only have produced the three blocks.
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Even though real time is frozen, the blocks should advance into the future.
    assert_eq!(first_block_time, start_time + block_time.as_secs());
    assert_eq!(second_block_time, start_time + block_time.as_secs() * 2);
    assert_eq!(third_block_time, start_time + block_time.as_secs() * 3);

    ctx.test_ctx.service.stop_and_await().await.unwrap();
}

#[tokio::test]
async fn interval_trigger_produces_blocks_with_current_time_when_block_production_is_lagging()
 {
    // Given

    let block_time = Duration::from_secs(10);
    let second_block_delay = Duration::from_secs(5);
    let offset = Duration::from_secs(1);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval { block_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    ctx.new_txs_notifier.send_replace(());
    let start_time = ctx.now();

    // When

    // We produce the first block in real time.
    time::sleep(block_time + offset).await;
    ctx.advance_time_with_tokio();
    let first_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // But we produce second block with a delay relative to real time.
    ctx.advance_time(block_time + second_block_delay);
    time::sleep(block_time).await;
    let second_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // And the third block is produced without advancing real time.
    time::sleep(block_time).await;
    let third_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // Then

    // We should only have produced the three blocks.
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // The fist block should be produced after the given block time.
    assert_eq!(first_block_time, start_time + block_time.as_secs());

    // The second block should have a delay in its timestamp.
    assert_eq!(
        second_block_time,
        first_block_time
            + block_time.as_secs()
            + offset.as_secs()
            + second_block_delay.as_secs()
    );

    // The third block should be produced `block_time` in the future relative to the second block time.
    assert_eq!(third_block_time, second_block_time + block_time.as_secs());

    ctx.test_ctx.service.stop_and_await().await.unwrap();
}

#[tokio::test]
async fn interval_trigger_produces_blocks_in_the_future_when_time_rewinds() {
    // Given

    let block_time = Duration::from_secs(10);
    let offset = Duration::from_secs(1);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval { block_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    ctx.new_txs_notifier.send_replace(());
    let start_time = ctx.now();

    // When

    // We produce the first block in real time.
    time::sleep(block_time + offset).await;
    ctx.advance_time_with_tokio();
    let first_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // And we rewind time before attempting to produce the next block.
    ctx.rewind_time(block_time);
    time::sleep(block_time).await;
    let second_block_time = ctx.block_import.try_recv().unwrap().entity.header().time();

    // Then

    // We should only have produced two blocks.
    assert!(matches!(
        ctx.block_import.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // The fist block should be produced after the given block time.
    assert_eq!(first_block_time, start_time + block_time.as_secs());

    // Even though the real time clock rewinded, the second block is produced with a future timestamp
    // similarly to how it works when time is lagging.
    assert_eq!(second_block_time, start_time + block_time.as_secs() * 2);
}

#[tokio::test]
async fn interval_trigger_even_if_queued_tx_events() {
    let block_time = Duration::from_secs(2);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Interval { block_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    let block_creation_notifier = Arc::new(Notify::new());
    tokio::task::spawn({
        let notifier = ctx.new_txs_notifier.clone();
        async move {
            loop {
                time::sleep(Duration::from_nanos(10)).await;
                notifier.send_replace(());
            }
        }
    });
    let block_creation_waiter = block_creation_notifier.clone();
    tokio::task::spawn(async move {
        ctx.block_import.recv().await.unwrap();
        block_creation_notifier.notify_waiters();
    });
    block_creation_waiter.notified().await;
}

#[tokio::test]
async fn open_trigger__produce_blocks_in_time() {
    // Given
    let open_time = Duration::from_secs(10);
    let quarter_of_open_time = open_time / 4;
    let offset = Duration::from_secs(1);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Open { period: open_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    time::sleep(offset).await;

    for _ in 0..10 {
        // When
        ctx.advance_time_with_tokio();
        time::sleep(quarter_of_open_time).await;
        let first_quarter = ctx.block_import.try_recv();

        ctx.advance_time_with_tokio();
        time::sleep(quarter_of_open_time).await;
        let second_quarter = ctx.block_import.try_recv();

        ctx.advance_time_with_tokio();
        time::sleep(quarter_of_open_time).await;
        let third_quarter = ctx.block_import.try_recv();

        ctx.advance_time_with_tokio();
        time::sleep(quarter_of_open_time).await;
        let forth_quarter = ctx.block_import.try_recv();

        // Then
        assert!(first_quarter.is_err());
        assert!(second_quarter.is_err());
        assert!(third_quarter.is_err());
        assert!(forth_quarter.is_ok());
    }
}

#[tokio::test]
async fn open_trigger__produce_blocks_with_correct_time() {
    // Given
    let open_time = Duration::from_secs(10);
    let offset = Duration::from_secs(1);
    let mut ctx = DefaultContext::new(Config {
        trigger: Trigger::Open { period: open_time },
        signer: SignMode::Key(test_signing_key()),
        metrics: false,
        ..Default::default()
    })
    .await;
    let expected_first_block_time = ctx.now().0.checked_add(open_time.as_secs()).unwrap();
    let expected_second_block_time = expected_first_block_time
        .checked_add(open_time.as_secs())
        .unwrap();

    // When
    time::sleep(offset).await;
    ctx.advance_time_with_tokio();
    time::sleep(open_time).await;
    let first_block = ctx.block_import.try_recv();

    ctx.advance_time_with_tokio();
    time::sleep(open_time).await;
    let second_block = ctx.block_import.try_recv();

    // Then
    assert!(first_block.is_ok());
    assert!(second_block.is_ok());
    let first_block_time = first_block.unwrap().entity.header().time();
    let second_block_time = second_block.unwrap().entity.header().time();
    assert_eq!(first_block_time.0, expected_first_block_time);
    assert_eq!(second_block_time.0, expected_second_block_time);
}
