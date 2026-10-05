use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Outcome {
    Unknown,
    Paid,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Transfer {
    locked: bool,
    outcome: Outcome,
    disposed: bool,
    recipient: u8,
    refund: u8,
    operator: u8,
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            locked: false,
            outcome: Outcome::Unknown,
            disposed: false,
            recipient: 0,
            refund: 0,
            operator: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Ledger {
    sender: u8,
    escrow: u8,
    pool: u8,
    transfers: [Transfer; 2],
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            sender: 22,
            escrow: 0,
            pool: 10,
            transfers: [Transfer::default(); 2],
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Lock(usize),
    Pay(usize),
    Reject(usize),
    Cancel(usize),
    Deliver(usize),
    Replay(usize),
    CrashRestart,
}

const OPERATIONS: [Operation; 13] = [
    Operation::Lock(0),
    Operation::Lock(1),
    Operation::Pay(0),
    Operation::Pay(1),
    Operation::Reject(0),
    Operation::Reject(1),
    Operation::Cancel(0),
    Operation::Cancel(1),
    Operation::Deliver(0),
    Operation::Deliver(1),
    Operation::Replay(0),
    Operation::Replay(1),
    Operation::CrashRestart,
];

impl Ledger {
    fn apply(&mut self, operation: Operation) {
        match operation {
            Operation::Lock(index) if !self.transfers[index].locked && self.sender >= 11 => {
                self.sender -= 11;
                self.escrow += 11;
                self.transfers[index].locked = true;
            }
            Operation::Pay(index)
                if self.transfers[index].locked
                    && self.transfers[index].outcome == Outcome::Unknown
                    && self.pool >= 10 =>
            {
                self.pool -= 10;
                self.transfers[index].recipient += 10;
                self.transfers[index].outcome = Outcome::Paid;
            }
            Operation::Reject(index) | Operation::Cancel(index)
                if self.transfers[index].locked
                    && self.transfers[index].outcome == Outcome::Unknown =>
            {
                self.transfers[index].outcome = Outcome::Rejected;
            }
            Operation::Deliver(index) | Operation::Replay(index)
                if self.transfers[index].locked && !self.transfers[index].disposed =>
            {
                match self.transfers[index].outcome {
                    Outcome::Paid => self.transfers[index].operator += 11,
                    Outcome::Rejected => self.transfers[index].refund += 11,
                    Outcome::Unknown => return,
                }
                self.escrow -= 11;
                self.transfers[index].disposed = true;
            }
            Operation::CrashRestart
            | Operation::Lock(_)
            | Operation::Pay(_)
            | Operation::Reject(_)
            | Operation::Cancel(_)
            | Operation::Deliver(_)
            | Operation::Replay(_) => {}
        }
    }

    fn assert_invariants(&self, schedule: &[Operation]) {
        let recipient_total: u8 = self.transfers.iter().map(|value| value.recipient).sum();
        let refund_total: u8 = self.transfers.iter().map(|value| value.refund).sum();
        let operator_total: u8 = self.transfers.iter().map(|value| value.operator).sum();
        assert_eq!(
            self.sender + self.escrow + refund_total + operator_total,
            22,
            "source conservation failed; shortest schedule candidate: {schedule:?}"
        );
        assert_eq!(
            self.pool + recipient_total,
            10,
            "destination conservation failed; shortest schedule candidate: {schedule:?}"
        );
        assert!(
            self.transfers
                .iter()
                .filter(|value| value.recipient == 10)
                .count()
                <= 1,
            "pool paid more than its funded capacity; schedule: {schedule:?}"
        );
        for transfer in self.transfers {
            assert!(
                transfer.recipient <= 10,
                "duplicate payment; schedule: {schedule:?}"
            );
            assert!(
                transfer.refund <= 11,
                "duplicate refund; schedule: {schedule:?}"
            );
            assert!(
                transfer.operator <= 11,
                "duplicate release; schedule: {schedule:?}"
            );
            assert!(
                transfer.recipient == 0 || transfer.refund == 0,
                "payment and refund both occurred; schedule: {schedule:?}"
            );
            if transfer.outcome == Outcome::Paid {
                assert_eq!(
                    transfer.recipient, 10,
                    "committed recipient credit disappeared; schedule: {schedule:?}"
                );
            }
        }
    }
}

#[test]
fn all_distinct_schedules_through_twelve_operations_preserve_atomic_outcomes() {
    let initial = Ledger::default();
    let mut frontier = vec![(initial.clone(), Vec::new())];
    initial.assert_invariants(&[]);

    for _depth in 0..12 {
        let mut next = Vec::new();
        let mut seen = HashSet::new();
        for (state, schedule) in frontier {
            for operation in OPERATIONS {
                let mut candidate = state.clone();
                candidate.apply(operation);
                let mut candidate_schedule = schedule.clone();
                candidate_schedule.push(operation);
                candidate.assert_invariants(&candidate_schedule);
                if seen.insert(candidate.clone()) {
                    next.push((candidate, candidate_schedule));
                }
            }
        }
        frontier = next;
    }
}

#[test]
fn seeded_long_chaos_schedules_preserve_atomic_outcomes() {
    for seed in [1_u64, 0x5eed, 0xdead_beef, 0xffff_ffff_ffff_ffc5] {
        eprintln!("fast-transfer chaos seed: {seed:#x}");
        let mut random = seed;
        let mut ledger = Ledger::default();
        let mut recent = Vec::new();
        for _step in 0..10_000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let operation = OPERATIONS[(random as usize) % OPERATIONS.len()];
            ledger.apply(operation);
            recent.push(operation);
            if recent.len() > 24 {
                recent.remove(0);
            }
            ledger.assert_invariants(&recent);
        }
    }
}
