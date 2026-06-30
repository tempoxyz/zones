//! `SwapAndDepositRouter` — deployed on Tempo L1.

use crate::EncryptedDepositPayload;
use alloc::vec::Vec;
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolValue;

crate::sol! {
    #[derive(Debug)]
    contract SwapAndDepositRouter {
        function onWithdrawalReceived(
            bytes32 senderTag,
            address tokenIn,
            uint128 amount,
            bytes calldata data
        ) external returns (bytes4);
    }
}

/// Plaintext callback payload for `SwapAndDepositRouter.onWithdrawalReceived`.
///
/// This payload tells the router to optionally swap the withdrawn token on L1
/// and then perform a regular `ZonePortal.deposit(...)`.
#[derive(Debug, Clone)]
pub struct SwapAndDepositRouterPlaintextCallback {
    /// Token that should be deposited after the optional L1 swap.
    pub token_out: Address,
    /// Target zone portal that receives the downstream deposit.
    pub target_portal: Address,
    /// Zone recipient for the downstream plaintext deposit.
    pub recipient: Address,
    /// Tempo refund recipient if the downstream zone deposit later bounces.
    pub bounceback_recipient: Address,
    /// Memo recorded on the downstream plaintext deposit.
    pub memo: B256,
    /// Minimum acceptable output from the optional swap.
    ///
    /// Ignored when `tokenIn == token_out` and the router can deposit directly.
    pub min_amount_out: u128,
}

impl SwapAndDepositRouterPlaintextCallback {
    /// ABI-encode the router callback data expected by the Solidity router.
    pub fn abi_encode(&self) -> Vec<u8> {
        (
            false,
            self.token_out,
            self.target_portal,
            self.recipient,
            self.bounceback_recipient,
            self.memo,
            self.min_amount_out,
        )
            .abi_encode_params()
    }
}

/// Encrypted callback payload for `SwapAndDepositRouter.onWithdrawalReceived`.
///
/// This payload tells the router to optionally swap the withdrawn token on L1
/// and then call `ZonePortal.depositEncrypted(...)` with an ECIES-encrypted
/// `(recipient, memo)` payload.
#[derive(Debug, Clone)]
pub struct SwapAndDepositRouterEncryptedCallback {
    /// Token that should be deposited after the optional L1 swap.
    pub token_out: Address,
    /// Target zone portal that receives the downstream encrypted deposit.
    pub target_portal: Address,
    /// Portal encryption key index used to build [`Self::encrypted`].
    pub key_index: U256,
    /// ECIES-encrypted `(recipient, memo)` payload for `depositEncrypted`.
    pub encrypted: EncryptedDepositPayload,
    /// Tempo refund recipient if the downstream encrypted deposit later bounces.
    pub bounceback_recipient: Address,
    /// Minimum acceptable output from the optional swap.
    ///
    /// Ignored when `tokenIn == token_out` and the router can deposit directly.
    pub min_amount_out: u128,
}

impl SwapAndDepositRouterEncryptedCallback {
    /// ABI-encode the router callback data expected by the Solidity router.
    pub fn abi_encode(&self) -> Vec<u8> {
        (
            true,
            self.token_out,
            self.target_portal,
            self.key_index,
            self.encrypted.clone(),
            self.bounceback_recipient,
            self.min_amount_out,
        )
            .abi_encode_params()
    }
}
