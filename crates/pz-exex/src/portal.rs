//! ZonePortal contract interface for deposit extraction.
//!
//! The ZonePortal is deployed on L1 and allows users to deposit ETH
//! that is credited on the L2 Privacy Zone.

use alloy_primitives::{Address, U256, address};
use alloy_sol_types::sol;

/// Default ZonePortal contract address on L1.
///
/// TODO: Make this configurable per chain spec.
pub const ZONE_PORTAL_ADDRESS: Address = address!("0x0000000000000000000000000000000000004242");

sol! {
    /// Event emitted by the ZonePortal when a deposit is enqueued.
    ///
    /// ```solidity
    /// event DepositEnqueued(
    ///     address indexed sender,
    ///     address indexed to,
    ///     uint256 amount,
    ///     bytes data
    /// );
    /// ```
    #[derive(Debug)]
    event DepositEnqueued(
        address indexed sender,
        address indexed to,
        uint256 amount,
        bytes data
    );
}

/// Parsed deposit from L1.
#[derive(Debug, Clone)]
pub struct Deposit {
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on L2.
    pub to: Address,
    /// Amount deposited (in wei).
    pub amount: U256,
    /// Optional calldata for contract interactions.
    pub data: alloy_primitives::Bytes,
}

impl From<DepositEnqueued> for Deposit {
    fn from(event: DepositEnqueued) -> Self {
        Self {
            sender: event.sender,
            to: event.to,
            amount: event.amount,
            data: event.data,
        }
    }
}
