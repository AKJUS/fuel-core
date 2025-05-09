//! Test module for update_sender
//!
//! Contains tests for the UpdateSender and helper functions.
//! The test functions use arbitrary operations to perform
//! actions such as send, receive, subscribe and drop.

use fuel_core_types::{
    fuel_tx::Bytes32,
    services::transaction_status::TransactionStatus,
};
use proptest::prelude::*;
use test_strategy::{
    Arbitrary,
    proptest,
};
use tokio_stream::StreamExt;

use super::*;
use crate::{
    tx_status_stream::{
        State,
        TxStatusMessage,
        TxStatusStream,
        TxUpdate,
    },
    update_sender::{
        MpscChannel,
        SendError,
        UpdateSender,
    },
};
use std::{
    collections::HashMap,
    time::Duration,
};

const MAX_CHANNELS: usize = 2;
const MAX_IDS: u8 = 2;
const CAPACITY: usize = 4;

#[derive(Debug, Clone, Arbitrary)]
enum Op {
    Send(
        #[strategy(0..MAX_IDS)] u8,
        #[strategy(utils::tx_status_message_strategy())] TxStatusMessage,
    ),
    Recv(#[strategy(0..MAX_CHANNELS)] usize),
    Subscribe(#[strategy(0..MAX_IDS)] u8),
    DropRecv(#[strategy(0..MAX_CHANNELS)] usize),
}

#[derive(Debug, PartialEq, Eq, Clone, Arbitrary)]
pub(crate) enum StateTransitions {
    AddMsg(#[strategy(utils::tx_status_message_strategy())] TxStatusMessage),
    AddFailure,
    CloseRecv,
    Next,
}

pub(crate) fn apply_tx_state_transition(
    state: State,
    transition: StateTransitions,
) -> State {
    use State::*;
    use StateTransitions::*;
    match (state, transition) {
        (Empty, AddMsg(TxStatusMessage::Status(s))) => match s {
            TransactionStatus::Submitted(s) => Submitted(TransactionStatus::Submitted(s)),
            TransactionStatus::PreConfirmationSuccess(s) => {
                Preconfirmed(TransactionStatus::PreConfirmationSuccess(s))
            }
            TransactionStatus::PreConfirmationFailure(s) => {
                Preconfirmed(TransactionStatus::PreConfirmationFailure(s))
            }
            s => EarlySuccess(s),
        },
        (Empty, AddMsg(TxStatusMessage::FailedStatus)) => Failed,
        (Empty, AddFailure) => Failed,
        (Empty | Submitted(_) | Preconfirmed(_), Next) => Empty,
        (
            Submitted(_),
            AddMsg(TxStatusMessage::Status(TransactionStatus::Submitted(s))),
        ) => Submitted(TransactionStatus::Submitted(s)),
        (
            Submitted(_),
            AddMsg(TxStatusMessage::Status(TransactionStatus::PreConfirmationSuccess(s))),
        ) => Preconfirmed(TransactionStatus::PreConfirmationSuccess(s)),
        (
            Submitted(_),
            AddMsg(TxStatusMessage::Status(TransactionStatus::PreConfirmationFailure(s))),
        ) => Preconfirmed(TransactionStatus::PreConfirmationFailure(s)),
        (Submitted(s1), AddMsg(TxStatusMessage::Status(s2))) => Success(s1, s2),
        (Submitted(s1), AddMsg(TxStatusMessage::FailedStatus)) => LateFailed(s1),
        (Submitted(s), AddFailure) => LateFailed(s),
        (Preconfirmed(s1), AddMsg(TxStatusMessage::Status(s2))) => Success(s1, s2),
        (Preconfirmed(s1), AddMsg(TxStatusMessage::FailedStatus)) => LateFailed(s1),
        (Preconfirmed(s), AddFailure) => LateFailed(s),
        (_, CloseRecv) => Closed,
        (EarlySuccess(_) | Failed | SenderClosed(_), Next) => Closed,
        (LateFailed(_), Next) => Failed,
        (Success(_, s2), Next) => SenderClosed(s2),
        // Final states.
        (Closed, _) => Closed,
        (EarlySuccess(s), _) => EarlySuccess(s),
        (Success(s1, s2), _) => Success(s1, s2),
        (Failed, _) => Failed,
        (LateFailed(s), _) => LateFailed(s),
        (SenderClosed(s), _) => SenderClosed(s),
    }
}

pub(super) fn validate_send(
    tx: Result<(), SendError>,
    state: State,
    msg: TxStatusMessage,
) -> State {
    // Add the message to the stream.
    let state = apply_tx_state_transition(state, StateTransitions::AddMsg(msg));

    // Try to get the next message from the stream.
    let state = apply_tx_state_transition(state, StateTransitions::Next);

    // Try to send the message to the receiver.
    match tx {
        // If ok, then use this state.
        Ok(()) => state,
        // If the receiver is closed, then update the state.
        Err(SendError::Closed) => {
            apply_tx_state_transition(state, StateTransitions::CloseRecv)
        }
        // If the receiver is full, then update the state.
        Err(SendError::Full) => {
            apply_tx_state_transition(state, StateTransitions::AddFailure)
        }
    }
}

/// Proptest based test for update_sender
#[proptest]
fn test_update_sender(
    #[strategy(prop::collection::vec(Op::arbitrary(), 5..=6))] ops: Vec<Op>,
) {
    test_update_sender_inner(ops);
}

/// Regular test for update_sender
#[test]
fn test_update_sender_reg() {
    use Op::*;
    use TxStatusMessage::*;

    let ops = vec![
        Subscribe(0),
        Send(0, Status(TransactionStatus::Success(Default::default()))),
        Recv(0),
        Send(0, Status(TransactionStatus::Submitted(Default::default()))),
        Recv(0),
    ];
    test_update_sender_inner(ops);
}

/// Perform operations on `UpdateSender` and validate the results.
///
/// This function creates a simulated environment for testing the UpdateSender.
/// By providing a Vec of `Ops`, different situations can be created and tested.
///
/// # Arguments
///
/// * `ops` - A vector of `Op` enums that determine the operations to be performed on the UpdateSender.
#[allow(clippy::assigning_clones)] // Test code, we don't care about performance.
fn test_update_sender_inner(ops: Vec<Op>) {
    // Setup runtime
    thread_local! {
        static RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
    }

    // Initialize test variables
    let update = UpdateSender::new(CAPACITY, Duration::from_secs(5));
    let mut receivers: Vec<TxStatusStream> = Vec::new();
    let mut model_receivers: Vec<(u8, usize, [Option<TxStatusMessage>; 2])> = Vec::new();
    let mut sender_id = 0usize;
    let mut model: HashMap<u8, HashMap<usize, State>> = HashMap::new();

    // Process operations
    for op in ops {
        match op {
            // Sending a new update
            Op::Send(id, s) => {
                // Real
                update.send(TxUpdate {
                    tx_id: Bytes32::from([id; 32]),
                    message: s.clone(),
                });

                // Model
                remove_closed(&mut model);
                model_send(&mut model, &mut model_receivers, id, s);
            }
            // Receiving an update
            Op::Recv(i) => {
                // Real
                let mut real_msg = None;
                if let Some(rx) = receivers.get_mut(i) {
                    RT.with(|rt| {
                        rt.block_on(async {
                            if let Ok(Some(msg)) =
                                tokio::time::timeout(Duration::from_millis(10), rx.next())
                                    .await
                            {
                                real_msg = Some(msg);
                            }
                        })
                    });
                }
                // Model
                if let Some(rx) = model_receivers.get_mut(i) {
                    let msg = rx.2[1].take();
                    if msg.is_some() {
                        rx.2[1] = rx.2[0].clone();
                        rx.2[0] = None;
                    }

                    // Check the real and model messages are the same.
                    assert_eq!(
                        real_msg, msg,
                        "i: {}, real {:?}, model: {:?}",
                        i, real_msg, msg
                    );
                }
            }
            // Subscribing to updates
            Op::Subscribe(id) => {
                // Real
                if let Some(rx) =
                    update.try_subscribe::<MpscChannel>(Bytes32::from([id; 32]))
                {
                    receivers.push(rx);
                }

                // Model
                remove_closed(&mut model);
                model_subscribe(&mut model, &mut model_receivers, &mut sender_id, id);
            }
            // Dropping a receiver
            Op::DropRecv(i) => {
                // Real
                if i < receivers.len() {
                    let _ = receivers.remove(i);
                }
                // Model
                if i < model_receivers.len() {
                    model_receivers.remove(i);
                }
            }
        }
    }
}

/// Remove closed sender entries from the model.
///
/// # Arguments
///
/// * `model` - A mutable reference to the model of the senders.
fn remove_closed(model: &mut HashMap<u8, HashMap<usize, State>>) {
    model.retain(|_, senders| {
        senders.retain(|_, state| !matches!(state, State::Closed));
        !senders.is_empty()
    });
}

/// Model the subscribe functions behavior.
fn model_subscribe(
    model: &mut HashMap<u8, HashMap<usize, State>>,
    model_receivers: &mut Vec<(u8, usize, [Option<TxStatusMessage>; 2])>,
    sender_id: &mut usize,
    id: u8,
) {
    // Only add a new sender if the model is not full.
    if model.values().map(|v| v.len()).sum::<usize>() < CAPACITY {
        // Insert new senders with an empty state.
        let senders = model.entry(id).or_default();
        senders.insert(*sender_id, State::Empty);

        // Add a new receiver to the model.
        model_receivers.push((id, *sender_id, [None, None]));
        // Increment the sender id.
        *sender_id = sender_id.saturating_add(1);
    }
}

/// Model the sending behavior.
fn model_send(
    model: &mut HashMap<u8, HashMap<usize, State>>,
    model_receivers: &mut [(u8, usize, [Option<TxStatusMessage>; 2])],
    id: u8,
    s: TxStatusMessage,
) {
    // Only send if the model contains any senders for the given id.
    if let Some(senders) = model.get_mut(&id) {
        let mut to_remove = Vec::new();

        // Iterate over all senders and send the message.
        for (i, sender) in senders.iter_mut() {
            // Find the receiver buffer for the current sender.
            let buf = model_receivers.iter_mut().find_map(|(key, index, buf)| {
                (*key == id && *index == *i).then_some(buf)
            });

            // Map the receiver buffer to a result.
            let tx = match buf {
                Some(buf) => match buf {
                    [None, None] => {
                        buf[1] = Some(s.clone());
                        Ok(())
                    }
                    [None, Some(_)] => {
                        buf[0] = Some(s.clone());
                        Ok(())
                    }
                    [Some(_), Some(_)] => Err(SendError::Full),
                    // Can't ever have a message at the end of the buffer
                    // without a message at the start.
                    [Some(_), None] => unreachable!(),
                },
                None => Err(SendError::Closed),
            };

            // Validate the state transition.
            *sender = validate_send(tx, sender.clone(), s.clone());

            // Remove if closed.
            if matches!(sender, State::Closed) {
                to_remove.push(*i);
            }
        }

        // Remove closed senders after each send.
        for i in to_remove {
            senders.remove(&i);
        }
    }
}
