//! Independent safety model for instant Zone-to-Zone transfers.
//!
//! This is deliberately not an E2E substitute. It defines the acceptance oracle used by the
//! implementation-facing tests: destination terminal outcomes serialize, token effects are
//! literal debits and credits, and source disposition follows only a certified terminal outcome.

use std::collections::{HashSet, VecDeque};

const PRINCIPAL: i64 = 10;
const INITIAL_SENDER_BALANCE: i64 = 20;
const INITIAL_POOL_BALANCE: i64 = PRINCIPAL;
const EXHAUSTIVE_DEPTH: usize = 12;
const LONG_SCHEDULE_LEN: usize = 10_000;
const LONG_SCHEDULE_SEEDS: [u64; 16] = [
    0x0000_0000_0000_0001,
    0x0123_4567_89ab_cdef,
    0x0ddc_0ffe_e15e_d00d,
    0x1020_3040_5060_7080,
    0x1bad_b002_cafe_f00d,
    0x3141_5926_5358_9793,
    0x4242_4242_4242_4242,
    0x5eed_0000_0000_0001,
    0x7fff_ffff_ffff_ffff,
    0x8000_0000_0000_0001,
    0x9e37_79b9_7f4a_7c15,
    0xa5a5_5a5a_a5a5_5a5a,
    0xc001_d00d_dead_beef,
    0xd1ce_b00c_5eed_fade,
    0xf00d_f00d_f00d_f00d,
    0xffff_ffff_ffff_ffff,
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DestinationOutcome {
    Unknown,
    Paid,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SourceState {
    Unlocked,
    Locked,
    PaidAwaitingRelease,
    RejectedAwaitingRefund,
    Released,
    Refunded,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Transfer {
    source: SourceState,
    destination: DestinationOutcome,
    sender: i64,
    escrow: i64,
    operator_inventory: i64,
    recipient: i64,
    payment_count: u8,
    refund_count: u8,
    release_count: u8,
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            source: SourceState::Unlocked,
            destination: DestinationOutcome::Unknown,
            sender: INITIAL_SENDER_BALANCE,
            escrow: 0,
            operator_inventory: 0,
            recipient: 0,
            payment_count: 0,
            refund_count: 0,
            release_count: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Model {
    transfers: [Transfer; 2],
    pool: i64,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            transfers: [Transfer::default(); 2],
            pool: INITIAL_POOL_BALANCE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Operation {
    Lock(usize),
    Pay(usize),
    Reject(usize),
    Cancel(usize),
    RecordPaid(usize),
    RecordRejected(usize),
    Dispose(usize),
    RetryTerminal(usize),
    TokenMovementFails(usize),
    CrashSource,
    CrashDestination,
    RestartSource,
    RestartDestination,
}

const OPERATIONS: [Operation; 22] = [
    Operation::Lock(0),
    Operation::Lock(1),
    Operation::Pay(0),
    Operation::Pay(1),
    Operation::Reject(0),
    Operation::Reject(1),
    Operation::Cancel(0),
    Operation::Cancel(1),
    Operation::RecordPaid(0),
    Operation::RecordPaid(1),
    Operation::RecordRejected(0),
    Operation::RecordRejected(1),
    Operation::Dispose(0),
    Operation::Dispose(1),
    Operation::RetryTerminal(0),
    Operation::RetryTerminal(1),
    Operation::TokenMovementFails(0),
    Operation::TokenMovementFails(1),
    Operation::CrashSource,
    Operation::CrashDestination,
    Operation::RestartSource,
    Operation::RestartDestination,
];

#[derive(Clone, Debug)]
struct Reachable {
    model: Model,
    trace: Vec<Operation>,
}

impl Model {
    fn apply(mut self, operation: Operation) -> Self {
        match operation {
            Operation::Lock(id) => {
                let transfer = &mut self.transfers[id];
                if transfer.source == SourceState::Unlocked {
                    transfer.sender -= PRINCIPAL;
                    transfer.escrow += PRINCIPAL;
                    transfer.source = SourceState::Locked;
                }
            }
            Operation::Pay(id) => {
                let transfer = &mut self.transfers[id];
                if transfer.source == SourceState::Locked
                    && transfer.destination == DestinationOutcome::Unknown
                    && self.pool >= PRINCIPAL
                {
                    self.pool -= PRINCIPAL;
                    transfer.recipient += PRINCIPAL;
                    transfer.payment_count += 1;
                    transfer.destination = DestinationOutcome::Paid;
                }
            }
            Operation::Reject(id) | Operation::Cancel(id) => {
                let transfer = &mut self.transfers[id];
                if transfer.source == SourceState::Locked
                    && transfer.destination == DestinationOutcome::Unknown
                {
                    transfer.destination = DestinationOutcome::Rejected;
                }
            }
            Operation::RecordPaid(id) => {
                let transfer = &mut self.transfers[id];
                if transfer.source == SourceState::Locked
                    && transfer.destination == DestinationOutcome::Paid
                {
                    transfer.source = SourceState::PaidAwaitingRelease;
                }
            }
            Operation::RecordRejected(id) => {
                let transfer = &mut self.transfers[id];
                if transfer.source == SourceState::Locked
                    && transfer.destination == DestinationOutcome::Rejected
                {
                    transfer.source = SourceState::RejectedAwaitingRefund;
                }
            }
            Operation::Dispose(id) => {
                let transfer = &mut self.transfers[id];
                match transfer.source {
                    SourceState::PaidAwaitingRelease => {
                        transfer.escrow -= PRINCIPAL;
                        transfer.operator_inventory += PRINCIPAL;
                        transfer.release_count += 1;
                        transfer.source = SourceState::Released;
                    }
                    SourceState::RejectedAwaitingRefund => {
                        transfer.escrow -= PRINCIPAL;
                        transfer.sender += PRINCIPAL;
                        transfer.refund_count += 1;
                        transfer.source = SourceState::Refunded;
                    }
                    _ => {}
                }
            }
            Operation::RetryTerminal(id) => {
                let transfer = &mut self.transfers[id];
                match (transfer.destination, transfer.source) {
                    (DestinationOutcome::Paid, SourceState::Locked) => {
                        transfer.source = SourceState::PaidAwaitingRelease;
                    }
                    (DestinationOutcome::Rejected, SourceState::Locked) => {
                        transfer.source = SourceState::RejectedAwaitingRefund;
                    }
                    _ => {}
                }
            }
            Operation::TokenMovementFails(id) => {
                // A journaled token failure has no marker or balance effect. The ID is used so
                // schedules independently exercise both transfers.
                let _ = self.transfers[id];
            }
            Operation::CrashSource
            | Operation::CrashDestination
            | Operation::RestartSource
            | Operation::RestartDestination => {
                // The model contains only committed durable state. Crashes and restarts cannot
                // mutate it; implementation crash tests compare recovered state to this oracle.
            }
        }
        self
    }

    fn assert_literal_ledger(self, trace: &[Operation], seed: Option<u64>) {
        let destination_total = self.pool
            + self
                .transfers
                .iter()
                .map(|transfer| transfer.recipient)
                .sum::<i64>();
        assert_eq!(
            destination_total, INITIAL_POOL_BALANCE,
            "destination token conservation failed; seed={seed:?}; trace={trace:?}; state={self:?}"
        );
        assert!(
            self.pool >= 0,
            "pool overdrawn; seed={seed:?}; trace={trace:?}; state={self:?}"
        );

        for (id, transfer) in self.transfers.iter().enumerate() {
            let source_total = transfer.sender + transfer.escrow + transfer.operator_inventory;
            assert_eq!(
                source_total, INITIAL_SENDER_BALANCE,
                "source token conservation failed for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );
            assert!(
                transfer.sender >= 0
                    && transfer.escrow >= 0
                    && transfer.operator_inventory >= 0
                    && transfer.recipient >= 0,
                "negative literal balance for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );
            assert!(
                transfer.payment_count <= 1,
                "duplicate payment for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );
            assert!(
                transfer.refund_count <= 1,
                "duplicate refund for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );
            assert!(
                transfer.release_count <= 1,
                "duplicate release for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );
            assert!(
                !(transfer.payment_count > 0 && transfer.refund_count > 0),
                "payment and refund both occurred for transfer {id}; seed={seed:?}; trace={trace:?}; state={self:?}"
            );

            match transfer.source {
                SourceState::Unlocked => {
                    assert_eq!(transfer.sender, INITIAL_SENDER_BALANCE);
                    assert_eq!(transfer.escrow, 0);
                }
                SourceState::Locked
                | SourceState::PaidAwaitingRelease
                | SourceState::RejectedAwaitingRefund => {
                    assert_eq!(transfer.sender, INITIAL_SENDER_BALANCE - PRINCIPAL);
                    assert_eq!(transfer.escrow, PRINCIPAL);
                }
                SourceState::Released => {
                    assert_eq!(transfer.escrow, 0);
                    assert_eq!(transfer.operator_inventory, PRINCIPAL);
                    assert_eq!(transfer.destination, DestinationOutcome::Paid);
                }
                SourceState::Refunded => {
                    assert_eq!(transfer.sender, INITIAL_SENDER_BALANCE);
                    assert_eq!(transfer.escrow, 0);
                    assert_eq!(transfer.destination, DestinationOutcome::Rejected);
                }
            }
        }
    }
}

#[test]
fn exhaustive_atomic_outcome_schedules_through_twelve_operations() {
    let initial = Reachable {
        model: Model::default(),
        trace: Vec::new(),
    };
    let mut queue = VecDeque::from([initial.clone()]);
    let mut visited = HashSet::from([(initial.model, 0_usize)]);
    let mut state_count = 0_usize;

    while let Some(reachable) = queue.pop_front() {
        reachable
            .model
            .assert_literal_ledger(&reachable.trace, None);
        state_count += 1;
        if reachable.trace.len() == EXHAUSTIVE_DEPTH {
            continue;
        }

        for operation in OPERATIONS {
            let model = reachable.model.apply(operation);
            let depth = reachable.trace.len() + 1;
            if visited.insert((model, depth)) {
                let mut trace = reachable.trace.clone();
                trace.push(operation);
                queue.push_back(Reachable { model, trace });
            }
        }
    }

    assert!(
        state_count > 400,
        "exhaustive search unexpectedly visited only {state_count} state/depth pairs"
    );
}

#[test]
fn seeded_long_atomic_outcome_schedules_preserve_literal_balances() {
    for seed in LONG_SCHEDULE_SEEDS {
        let mut random = XorShift64::new(seed);
        let mut model = Model::default();
        let mut trace = VecDeque::with_capacity(EXHAUSTIVE_DEPTH);

        for _ in 0..LONG_SCHEDULE_LEN {
            let operation = OPERATIONS[random.next_index(OPERATIONS.len())];
            if trace.len() == EXHAUSTIVE_DEPTH {
                trace.pop_front();
            }
            trace.push_back(operation);
            model = model.apply(operation);
            model.assert_literal_ledger(trace.make_contiguous(), Some(seed));
        }
    }
}

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_index(&mut self, upper: usize) -> usize {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value as usize % upper
    }
}
