use std::collections::HashMap;

use fuel_core_types::fuel_tx::Bytes32;
use test_strategy::{
    Arbitrary,
    proptest,
};

use crate::{
    tests::utils::{
        MockCreateChannel,
        box_senders,
        senders_strategy_all_ok,
    },
    update_sender::{
        MockSendStatus,
        Sender,
        subscribe,
    },
};

#[derive(Debug, Arbitrary)]
struct Input {
    #[strategy(0..20u8)]
    tx_id: u8,
    #[strategy(senders_strategy_all_ok())]
    senders: HashMap<Bytes32, Vec<Sender<(), MockSendStatus>>>,
}

/// Simply test that for any of these inputs the subscribe function
/// adds one more subscriber.
#[proptest]
fn test_subscriber(input: Input) {
    let Input { tx_id, senders } = input;
    let mut senders = box_senders(senders);
    let len_before = senders.values().map(|v| v.len()).sum::<usize>();
    let _ = subscribe::<_, MockCreateChannel>(
        Bytes32::from([tx_id; 32]),
        &mut senders,
        Box::new(()),
    );
    let len_after = senders.values().map(|v| v.len()).sum::<usize>();
    assert_eq!(len_before.saturating_add(1), len_after);
}
