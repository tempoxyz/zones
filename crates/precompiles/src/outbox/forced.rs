//! Inbox-only building block. Authorization and replay processing are connected in PR3.
use super::*;
use crate::error::ZonePrecompileError;
use tempo_precompiles::{tip20::Recipient, tip403_registry::TIP403Registry};

/// Private authenticated input; never emitted as a public L1 attribution.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForcedWithdrawalRequest {
    /// Hash of the canonical decrypted payload including its verified root signature.
    pub private_request_hash: B256,
    pub token: Address,
    pub account: Address,
    pub recipient: Address,
    pub amount: u128,
}

/// Only enumerated policy failures may become a terminal rejection in the inbox.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForcedWithdrawalError {
    PolicyRejected,
    Fatal(ZonePrecompileError),
}

impl From<ZonePrecompileError> for ForcedWithdrawalError {
    fn from(error: ZonePrecompileError) -> Self {
        Self::Fatal(error)
    }
}
impl From<TempoPrecompileError> for ForcedWithdrawalError {
    fn from(error: TempoPrecompileError) -> Self {
        Self::Fatal(error.into())
    }
}
impl ForcedWithdrawalError {
    fn token_policy(error: TempoPrecompileError) -> Self {
        match error {
            TempoPrecompileError::TIP20(
                TIP20Error::PolicyForbids(_) | TIP20Error::ContractPaused(_),
            ) => Self::PolicyRejected,
            error => error.into(),
        }
    }
}

impl ZoneOutbox {
    /// Debit an already root-authorized full balance and enqueue a plain withdrawal atomically.
    /// No ABI selector exposes this operation. PR3 calls it after authorization and replay checks.
    #[allow(dead_code)]
    pub(crate) fn request_forced_withdrawal<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        request: ForcedWithdrawalRequest,
    ) -> Result<Withdrawal, ForcedWithdrawalError> {
        if caller != ZONE_INBOX_ADDRESS {
            return Err(ZonePrecompileError::from(ZoneOutboxError::only_zone_inbox()).into());
        }
        let checkpoint = self.storage.checkpoint();
        let ForcedWithdrawalRequest {
            private_request_hash,
            token,
            account,
            recipient,
            amount,
        } = request;
        if account.is_zero() || recipient.is_zero() || amount == 0 {
            return Err(ZonePrecompileError::MalformedCalldata.into());
        }
        if !l1
            .read_portal(|portal| &portal.token_configs[token].enabled)
            .map_err(ZonePrecompileError::from)?
        {
            return Err(ForcedWithdrawalError::PolicyRejected);
        }
        // Admission already checked portal pause. Only recipient policy is checked again here.
        self.validate_recipient_policy(l1, recipient, 0)
            .map_err(|error| match error {
                ZonePrecompileError::Portal(
                    ZonePortalError::AccountNotAllowed(_)
                    | ZonePortalError::InvalidCallbackTarget(_),
                ) => ForcedWithdrawalError::PolicyRejected,
                error => error.into(),
            })?;
        // L1 admission bounds this mandatory inbox workload. Do not apply or consume the
        // ordinary user-withdrawal cap: it could prevent an admitted anchor from executing.
        let mut token = TIP20Token::from_address(token)?;
        if !token.is_initialized()? {
            return Err(TempoPrecompileError::from(TIP20Error::uninitialized()).into());
        }
        let amount256 = U256::from(amount);
        if token.balance_of(ITIP20::balanceOfCall { account })? != amount256 {
            // Balance was checked by the inbox immediately before this call. Drift is fatal.
            return Err(TempoPrecompileError::under_overflow().into());
        }
        token
            .check_not_paused()
            .map_err(ForcedWithdrawalError::token_policy)?;
        token
            .ensure_transfer_authorized(account, self.address)
            .map_err(ForcedWithdrawalError::token_policy)?;
        if TIP403Registry::new()
            .validate_receive_policy(request.token, account, self.address)?
            .is_some()
        {
            // Reject instead of allowing a successful transfer to divert principal to a receipt.
            return Err(ForcedWithdrawalError::PolicyRejected);
        }
        // The destination is our fixed precompile address, not a caller-selected virtual alias.
        // Root authorization replaces allowance/transaction-key spending authorization only.
        // Native transfer and burn still run balance, supply, reward and event hooks.
        token
            ._transfer(account, &Recipient::direct(self.address), amount256)
            .map_err(ForcedWithdrawalError::token_policy)?;
        token
            .burn(self.address, ITIP20::burnCall { amount: amount256 })
            .map_err(ForcedWithdrawalError::token_policy)?;

        let nonce = self
            .last_fallback_nonce
            .read()?
            .checked_add(1)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        let index = self.next_withdrawal_index.read()?;
        self.last_fallback_nonce.write(nonce)?;
        self.fallback_recipients[nonce].write(account)?;
        let pending = PendingWithdrawal {
            token: request.token,
            sender: account,
            tx_hash: private_request_hash,
            to: recipient,
            amount,
            fallback_nonce: nonce,
            ..Default::default()
        };
        let withdrawal = pending.clone().into_withdrawal(Bytes::new())?;
        self.pending_withdrawals.push(pending)?;
        self.next_withdrawal_index.write(
            index
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        self.emit_event(ZoneOutboxEvent::forced_withdrawal_requested(
            index,
            request.token,
            withdrawal.senderTag,
            recipient,
            amount,
            nonce,
        ))?;
        checkpoint.commit();
        Ok(withdrawal)
    }
}
