//! Stable durable-journal operation tags.
//!
//! Tags are append-only. Reassigning one would reinterpret already-fsynced journals and
//! snapshots, so new operations must take a previously unused value.

pub(crate) const OUTGOING: u8 = 0;
pub(crate) const INCOMING: u8 = 1;
pub(crate) const TRANSITION: u8 = 2;
pub(crate) const CURSOR: u8 = 3;
pub(crate) const SIGNING: u8 = 4;
pub(crate) const TOMBSTONE: u8 = 5;
pub(crate) const REPLENISHMENT_JOB: u8 = 6;
pub(crate) const REPLICATED_BLOCK: u8 = 7;
pub(crate) const CANCELLATION: u8 = 8;
pub(crate) const CANCELLATION_COMPLETED: u8 = 9;
pub(crate) const ECONOMIC_ACTION: u8 = 10;
pub(crate) const ECONOMIC_SUBMISSION: u8 = 11;
pub(crate) const ECONOMIC_COMPLETED: u8 = 12;
pub(crate) const ECONOMIC_NEXT_ATTEMPT: u8 = 13;
