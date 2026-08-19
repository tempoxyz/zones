//! Decoded L2 Inbox/Outbox events with their canonical receipt provenance.

use alloy_consensus::{TxReceipt, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::SolEvent;
use tempo_precompiles::tip20::{ITIP20, TIP20Token};
use tempo_zone_contracts::{IZoneInbox, IZoneOutbox, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use crate::decode_event;

/// Exact Tempo/L1 block imported by `TempoAdvanced`.
#[derive(Debug)]
pub(crate) struct L1Anchor {
    pub(super) tempo_block_hash: B256,
    pub(super) tempo_block_number: u64,
}

impl L1Anchor {
    pub(crate) fn block_hash(&self) -> B256 {
        self.tempo_block_hash
    }

    pub(crate) fn block_number(&self) -> u64 {
        self.tempo_block_number
    }
}

/// Semantic value of one recognized Zone Inbox or Outbox event.
#[derive(Debug)]
pub(crate) enum L2BridgeEvent {
    /// Required link to the imported Tempo/L1 block.
    TempoAdvanced(L1Anchor),
    /// Successful or failed deposit execution.
    DepositOutcome {
        recipient: Option<Address>,
        token: Address,
        amount: u128,
        processed: bool,
    },
    /// Processed or pending withdrawal bounce-back.
    WithdrawalBounceBack {
        recipient: Address,
        token: Address,
        amount: u128,
        processed: bool,
    },
    RefundClaimed {
        recipient: Address,
        token: Address,
        amount: u128,
    },
    /// Canonical TIP-20 ownership movement used to identify affected accounts.
    Transfer {
        token: Address,
        from: Address,
        to: Address,
        amount: U256,
    },
    /// TIP-20 burn corroborating a transfer to the zero address.
    TokenBurn {
        token: Address,
        from: Address,
        amount: U256,
    },
    /// User withdrawal, preserving principal and fee separately.
    WithdrawalRequested {
        withdrawal_index: u64,
        sender: Address,
        token: Address,
        principal: u128,
        fee: u128,
    },
}

/// One decoded bridge event and its canonical receipt provenance.
#[derive(Debug)]
pub(crate) struct L2EventEvidence {
    pub(super) transaction_hash: B256,
    pub(super) transaction_index: u32,
    pub(super) event: L2BridgeEvent,
}

/// Ordered recognized events and the required L1 anchor position.
#[derive(Debug)]
pub(super) struct L2Events {
    pub(super) events: Vec<L2EventEvidence>,
    anchor_index: usize,
}

impl L2Events {
    /// Return the block's required `TempoAdvanced` anchor.
    pub(crate) fn l1_anchor(&self) -> Option<&L1Anchor> {
        match &self.events.get(self.anchor_index)?.event {
            L2BridgeEvent::TempoAdvanced(anchor) => Some(anchor),
            _ => None,
        }
    }

    /// Return TIP-20 transfers in canonical block-log order.
    pub(crate) fn token_transfers(
        &self,
    ) -> impl Iterator<Item = (Address, Address, Address, U256)> + '_ {
        self.events
            .iter()
            .filter_map(|evidence| match evidence.event {
                L2BridgeEvent::Transfer {
                    token,
                    from,
                    to,
                    amount,
                } => Some((token, from, to, amount)),
                _ => None,
            })
    }

    /// Require every Inbox mint to match its authenticated bridge event.
    fn authenticate_mints(&self) -> eyre::Result<()> {
        for receipt in self
            .events
            .chunk_by(|left, right| left.transaction_index == right.transaction_index)
        {
            authenticate_receipt_mints(receipt)?;
        }
        Ok(())
    }

    /// Require every user withdrawal to have its exact debit-and-burn sequence.
    fn authenticate_withdrawals(&self) -> eyre::Result<()> {
        for receipt in self
            .events
            .chunk_by(|left, right| left.transaction_index == right.transaction_index)
        {
            authenticate_receipt_withdrawals(receipt)?;
        }
        Ok(())
    }
}

/// Reconcile Inbox mints with their outcome in one receipt.
fn authenticate_receipt_mints(receipt: &[L2EventEvidence]) -> eyre::Result<()> {
    let Some(first) = receipt.first() else {
        return Ok(());
    };
    let mut index = 0;

    while let Some(evidence) = receipt.get(index) {
        let Some(observed) = Mint::from_transfer(&evidence.event) else {
            if Mint::from_outcome(&evidence.event).is_some() {
                eyre::bail!(
                    "transaction {} has an Inbox outcome without its mint",
                    first.transaction_hash,
                );
            }
            index += 1;
            continue;
        };

        index += 1;
        if receipt
            .get(index)
            .is_some_and(|evidence| observed.matches_forward(&evidence.event))
        {
            index += 1;
        }

        let Some(expected) = receipt
            .get(index)
            .and_then(|evidence| Mint::from_outcome(&evidence.event))
        else {
            eyre::bail!(
                "transaction {} has an unexplained mint of {} {} to {}",
                first.transaction_hash,
                observed.amount,
                observed.token,
                observed.recipient,
            );
        };

        eyre::ensure!(
            observed == expected,
            "transaction {} mint {:?} does not match Inbox outcome {:?}",
            first.transaction_hash,
            observed,
            expected,
        );
        index += 1;
    }
    Ok(())
}

/// TIP-20 mint expected from one successful Inbox action.
#[derive(Debug, PartialEq, Eq)]
struct Mint {
    token: Address,
    recipient: Address,
    amount: U256,
}

impl Mint {
    /// Decode a zero-address TIP-20 transfer as a mint.
    fn from_transfer(event: &L2BridgeEvent) -> Option<Self> {
        let L2BridgeEvent::Transfer {
            token,
            from,
            to,
            amount,
        } = event
        else {
            return None;
        };
        from.is_zero().then_some(Self {
            token: *token,
            recipient: *to,
            amount: *amount,
        })
    }

    /// Return whether an event forwards this virtual recipient to its master.
    fn matches_forward(&self, event: &L2BridgeEvent) -> bool {
        matches!(
            event,
            L2BridgeEvent::Transfer { token, from, to, amount }
                if *token == self.token
                    && *from == self.recipient
                    && !to.is_zero()
                    && *to != self.recipient
                    && *amount == self.amount
        )
    }

    /// Decode the Inbox outcome authenticating a mint.
    fn from_outcome(event: &L2BridgeEvent) -> Option<Self> {
        match event {
            L2BridgeEvent::DepositOutcome {
                recipient: Some(recipient),
                token,
                amount,
                processed: true,
                ..
            }
            | L2BridgeEvent::WithdrawalBounceBack {
                recipient,
                token,
                amount,
                processed: true,
            }
            | L2BridgeEvent::RefundClaimed {
                recipient,
                token,
                amount,
            } => Some(Self {
                token: *token,
                recipient: *recipient,
                amount: U256::from(*amount),
            }),
            _ => None,
        }
    }
}

/// Reconcile every withdrawal in one receipt with distinct preceding debit-and-burn groups.
fn authenticate_receipt_withdrawals(receipt: &[L2EventEvidence]) -> eyre::Result<()> {
    let Some(first) = receipt.first() else {
        return Ok(());
    };
    let burns = receipt
        .windows(3)
        .enumerate()
        .filter_map(|(start, events)| WithdrawalBurn::from_events(start + events.len(), events))
        .collect::<Vec<_>>();
    let mut consumed = vec![false; burns.len()];

    for (request_index, evidence) in receipt.iter().enumerate() {
        let L2BridgeEvent::WithdrawalRequested {
            sender,
            token,
            principal,
            fee,
            ..
        } = evidence.event
        else {
            continue;
        };
        if sender.is_zero() {
            continue;
        }

        let principal = U256::from(principal);
        let fee = U256::from(fee);
        let total = principal + fee;
        if let Some(index) = matching_burn(&burns, &consumed, request_index, token, sender, total) {
            consumed[index] = true;
            continue;
        }

        let principal_burn =
            matching_burn(&burns, &consumed, request_index, token, sender, principal);
        let fee_burn = if fee.is_zero() {
            None
        } else {
            matching_fee_burn(&burns, &consumed, request_index, token, fee, principal_burn)
        };
        if let (Some(principal_burn), Some(fee_burn)) = (principal_burn, fee_burn) {
            consumed[principal_burn] = true;
            consumed[fee_burn] = true;
            continue;
        }

        eyre::bail!(
            "withdrawal in transaction {} has no matching debit and burn of {total} {token}",
            evidence.transaction_hash
        );
    }

    if burns.iter().enumerate().any(|(index, _)| !consumed[index]) {
        eyre::bail!(
            "transaction {} has an unexplained withdrawal debit and burn",
            first.transaction_hash
        );
    }
    Ok(())
}

/// Find one unconsumed debit-and-burn group preceding a withdrawal request.
fn matching_burn(
    burns: &[WithdrawalBurn],
    consumed: &[bool],
    request_index: usize,
    token: Address,
    owner: Address,
    amount: U256,
) -> Option<usize> {
    burns.iter().enumerate().find_map(|(index, observed)| {
        (!consumed[index]
            && observed.end_index <= request_index
            && observed.token == token
            && observed.owner == owner
            && observed.amount == amount)
            .then_some(index)
    })
}

/// Find an unconsumed sponsored-fee burn preceding a withdrawal request.
fn matching_fee_burn(
    burns: &[WithdrawalBurn],
    consumed: &[bool],
    request_index: usize,
    token: Address,
    amount: U256,
    excluded: Option<usize>,
) -> Option<usize> {
    burns.iter().enumerate().find_map(|(index, observed)| {
        (Some(index) != excluded
            && !consumed[index]
            && observed.end_index <= request_index
            && observed.token == token
            && observed.amount == amount
            && !observed.owner.is_zero())
        .then_some(index)
    })
}

/// One debit, transfer-to-zero, and burn sequence within a receipt.
struct WithdrawalBurn {
    end_index: usize,
    token: Address,
    owner: Address,
    amount: U256,
}

impl WithdrawalBurn {
    /// Decode one adjacent transfer, zero transfer, and burn sequence.
    fn from_events(end_index: usize, events: &[L2EventEvidence]) -> Option<Self> {
        let [debit, transfer, burn] = events else {
            return None;
        };
        let L2BridgeEvent::Transfer {
            token: debit_token,
            from: owner,
            to,
            amount: debit_amount,
        } = debit.event
        else {
            return None;
        };
        let L2BridgeEvent::Transfer {
            token: transfer_token,
            from,
            to: recipient,
            amount: transfer_amount,
        } = transfer.event
        else {
            return None;
        };
        let L2BridgeEvent::TokenBurn {
            token: burn_token,
            from: burn_from,
            amount: burn_amount,
        } = burn.event
        else {
            return None;
        };
        (debit_token == transfer_token
            && debit_token == burn_token
            && to == ZONE_OUTBOX_ADDRESS
            && from == ZONE_OUTBOX_ADDRESS
            && recipient.is_zero()
            && burn_from == ZONE_OUTBOX_ADDRESS
            && debit_amount == transfer_amount
            && debit_amount == burn_amount)
            .then_some(Self {
                end_index,
                token: debit_token,
                owner,
                amount: debit_amount,
            })
    }
}

/// Builds ordered event evidence while receipts are visited once.
#[derive(Default)]
pub(super) struct EventCollector {
    events: Vec<L2EventEvidence>,
    anchor_index: Option<usize>,
}

impl EventCollector {
    /// Decode recognized logs from one canonical receipt.
    pub(super) fn extract_receipt<T, R>(
        &mut self,
        transaction_index: usize,
        transaction: &T,
        receipt: &R,
        block: u64,
    ) -> eyre::Result<()>
    where
        T: TxHashRef,
        R: TxReceipt<Log = Log>,
    {
        if !receipt.status() {
            return Ok(());
        }

        for log in receipt.logs() {
            let event = match log.address {
                ZONE_INBOX_ADDRESS => decode_inbox(log, block)?,
                ZONE_OUTBOX_ADDRESS => decode_outbox(log, block)?,
                _ => decode_token_event(log, block)?,
            };
            if let Some(event) = event {
                if matches!(event, L2BridgeEvent::TempoAdvanced(_)) {
                    eyre::ensure!(
                        self.anchor_index.is_none(),
                        "duplicate TempoAdvanced in block {block}"
                    );
                    self.anchor_index = Some(self.events.len());
                }
                self.events.push(L2EventEvidence {
                    transaction_hash: *transaction.tx_hash(),
                    transaction_index: transaction_index as u32,
                    event,
                });
            }
        }
        Ok(())
    }

    /// Require the L1 anchor and finish the ordered event bundle.
    pub(super) fn finish(self, block: u64) -> eyre::Result<L2Events> {
        let anchor_index = self
            .anchor_index
            .ok_or_else(|| eyre::eyre!("block {block} is missing TempoAdvanced"))?;
        let events = L2Events {
            events: self.events,
            anchor_index,
        };
        events.authenticate_mints()?;
        events.authenticate_withdrawals()?;
        Ok(events)
    }
}

/// Decode one recognized Zone Inbox log.
fn decode_inbox(log: &Log, block: u64) -> eyre::Result<Option<L2BridgeEvent>> {
    let topic = log
        .topics()
        .first()
        .ok_or_else(|| eyre::eyre!("topicless ZoneInbox log in block {block}"))?;
    Ok(Some(match *topic {
        IZoneInbox::TempoAdvanced::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::TempoAdvanced>(log, "TempoAdvanced", block)?;
            L2BridgeEvent::TempoAdvanced(L1Anchor {
                tempo_block_hash: event.tempoBlockHash,
                tempo_block_number: event.tempoBlockNumber,
            })
        }
        IZoneInbox::DepositProcessed::SIGNATURE_HASH => {
            let event =
                decode_event::<IZoneInbox::DepositProcessed>(log, "DepositProcessed", block)?;
            L2BridgeEvent::DepositOutcome {
                recipient: Some(event.to),
                token: event.token,
                amount: event.amount,
                processed: true,
            }
        }
        IZoneInbox::DepositFailed::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::DepositFailed>(log, "DepositFailed", block)?;
            L2BridgeEvent::DepositOutcome {
                recipient: None,
                token: event.token,
                amount: event.amount,
                processed: false,
            }
        }
        IZoneInbox::WithdrawalBounceBackProcessed::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::WithdrawalBounceBackProcessed>(
                log,
                "WithdrawalBounceBackProcessed",
                block,
            )?;
            L2BridgeEvent::WithdrawalBounceBack {
                recipient: event.zoneFallbackRecipient,
                token: event.token,
                amount: event.amount,
                processed: true,
            }
        }
        IZoneInbox::WithdrawalBounceBackPending::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::WithdrawalBounceBackPending>(
                log,
                "WithdrawalBounceBackPending",
                block,
            )?;
            L2BridgeEvent::WithdrawalBounceBack {
                recipient: event.zoneFallbackRecipient,
                token: event.token,
                amount: event.amount,
                processed: false,
            }
        }
        IZoneInbox::RefundClaimed::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::RefundClaimed>(log, "RefundClaimed", block)?;
            L2BridgeEvent::RefundClaimed {
                recipient: event.recipient,
                token: event.token,
                amount: event.amount,
            }
        }
        IZoneInbox::TokenEnabled::SIGNATURE_HASH => {
            decode_event::<IZoneInbox::TokenEnabled>(log, "TokenEnabled", block)?;
            return Ok(None);
        }
        IZoneInbox::DepositRejected::SIGNATURE_HASH => {
            eyre::bail!("unsupported DepositRejected in block {block}")
        }
        _ => eyre::bail!("unsupported ZoneInbox event {topic} in block {block}"),
    }))
}

/// Decode accounting evidence emitted by a canonical TIP-20 precompile.
fn decode_token_event(log: &Log, block: u64) -> eyre::Result<Option<L2BridgeEvent>> {
    if TIP20Token::from_address(log.address).is_err() {
        return Ok(None);
    }
    match log.topics().first() {
        Some(topic) if *topic == ITIP20::Transfer::SIGNATURE_HASH => {
            let event = decode_event::<ITIP20::Transfer>(log, "Transfer", block)?;
            Ok(Some(L2BridgeEvent::Transfer {
                token: log.address,
                from: event.from,
                to: event.to,
                amount: event.amount,
            }))
        }
        Some(topic) if *topic == ITIP20::Burn::SIGNATURE_HASH => {
            let event = decode_event::<ITIP20::Burn>(log, "Burn", block)?;
            Ok(Some(L2BridgeEvent::TokenBurn {
                token: log.address,
                from: event.from,
                amount: event.amount,
            }))
        }
        _ => Ok(None),
    }
}

/// Decode one recognized Zone Outbox log.
fn decode_outbox(log: &Log, block: u64) -> eyre::Result<Option<L2BridgeEvent>> {
    let topic = log
        .topics()
        .first()
        .ok_or_else(|| eyre::eyre!("topicless ZoneOutbox log in block {block}"))?;
    Ok(Some(match *topic {
        IZoneOutbox::WithdrawalRequested::SIGNATURE_HASH => {
            let event = decode_event::<IZoneOutbox::WithdrawalRequested>(
                log,
                "WithdrawalRequested",
                block,
            )?;
            L2BridgeEvent::WithdrawalRequested {
                withdrawal_index: event.withdrawalIndex,
                sender: event.sender,
                token: event.token,
                principal: event.amount,
                fee: event.fee,
            }
        }
        IZoneOutbox::BatchFinalized::SIGNATURE_HASH => {
            decode_event::<IZoneOutbox::BatchFinalized>(log, "BatchFinalized", block)?;
            return Ok(None);
        }
        IZoneOutbox::TempoGasRateUpdated::SIGNATURE_HASH => {
            decode_event::<IZoneOutbox::TempoGasRateUpdated>(log, "TempoGasRateUpdated", block)?;
            return Ok(None);
        }
        IZoneOutbox::MaxWithdrawalsPerBlockUpdated::SIGNATURE_HASH => {
            decode_event::<IZoneOutbox::MaxWithdrawalsPerBlockUpdated>(
                log,
                "MaxWithdrawalsPerBlockUpdated",
                block,
            )?;
            return Ok(None);
        }
        _ => eyre::bail!("unsupported ZoneOutbox event {topic} in block {block}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction as _, TxLegacy, TxType};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Bytes, LogData, Signature, TxKind, U256, address};
    use alloy_sol_types::SolEvent;
    use reth_ethereum_primitives::Receipt;

    fn anchor_evidence(events: &L2Events) -> &L2EventEvidence {
        &events.events[events.anchor_index]
    }

    fn collect<T, R>(
        transactions: &[T],
        receipts: &[R],
        block: BlockNumHash,
    ) -> eyre::Result<L2Events>
    where
        T: TxHashRef,
        R: TxReceipt<Log = Log>,
    {
        crate::l2::collect_l2_block_evidence(transactions, receipts, block)
            .map(|evidence| evidence.events)
    }

    fn transaction() -> alloy_consensus::Signed<TxLegacy> {
        TxLegacy {
            to: TxKind::Call(ZONE_INBOX_ADDRESS),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
    }

    fn anchor_log(number: u64) -> Log {
        Log {
            address: ZONE_INBOX_ADDRESS,
            data: IZoneInbox::TempoAdvanced {
                tempoBlockHash: B256::repeat_byte(0xaa),
                tempoBlockNumber: number,
                depositsProcessed: U256::from(2),
                newProcessedDepositQueueHash: B256::repeat_byte(0xbb),
                lastProcessedDepositNumber: 3,
            }
            .encode_log_data(),
        }
    }

    fn receipt(logs: Vec<Log>) -> Receipt {
        Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs,
        }
    }

    fn event_log<E: SolEvent>(address: Address, event: E) -> Log {
        Log {
            address,
            data: event.encode_log_data(),
        }
    }

    #[test]
    fn decodes_all_event_variants_in_canonical_order() {
        let token_a = address!("20c0000000000000000000000000000000000000");
        let token_b = address!("20c0000000000000000000000000000000000001");
        let recipient_a = Address::repeat_byte(0xaa);
        let recipient_b = Address::repeat_byte(0xbb);
        let logs = vec![
            anchor_log(100),
            event_log(
                token_a,
                ITIP20::Transfer {
                    from: Address::ZERO,
                    to: recipient_a,
                    amount: U256::from(500),
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::DepositProcessed {
                    depositHash: B256::repeat_byte(0xd0),
                    sender: Address::ZERO,
                    to: recipient_a,
                    token: token_a,
                    amount: 500,
                    memo: B256::ZERO,
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::DepositFailed {
                    depositHash: B256::repeat_byte(0xd1),
                    sender: Address::ZERO,
                    token: token_b,
                    amount: 300,
                },
            ),
            event_log(
                token_a,
                ITIP20::Transfer {
                    from: Address::ZERO,
                    to: recipient_b,
                    amount: U256::from(777),
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::WithdrawalBounceBackProcessed {
                    zoneFallbackRecipient: recipient_b,
                    token: token_a,
                    amount: 777,
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::WithdrawalBounceBackPending {
                    zoneFallbackRecipient: Address::ZERO,
                    token: token_b,
                    amount: 888,
                },
            ),
            event_log(
                token_a,
                ITIP20::Transfer {
                    from: Address::ZERO,
                    to: recipient_b,
                    amount: U256::from(42),
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::RefundClaimed {
                    recipient: recipient_b,
                    token: token_a,
                    amount: 42,
                },
            ),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::TokenEnabled {
                    token: token_b,
                    name: "Token B".into(),
                    symbol: "TB".into(),
                    currency: "USD".into(),
                },
            ),
            event_log(
                ZONE_OUTBOX_ADDRESS,
                IZoneOutbox::WithdrawalRequested {
                    withdrawalIndex: 3,
                    sender: Address::ZERO,
                    token: token_a,
                    to: Address::ZERO,
                    amount: 1000,
                    fee: 50,
                    memo: B256::ZERO,
                    gasLimit: 0,
                    fallbackNonce: 8,
                    data: Default::default(),
                    revealTo: Default::default(),
                },
            ),
            event_log(
                token_b,
                ITIP20::Transfer {
                    from: Address::repeat_byte(0x44),
                    to: ZONE_OUTBOX_ADDRESS,
                    amount: U256::from(2075),
                },
            ),
            event_log(
                token_b,
                ITIP20::Transfer {
                    from: ZONE_OUTBOX_ADDRESS,
                    to: Address::ZERO,
                    amount: U256::from(2075),
                },
            ),
            event_log(
                token_b,
                ITIP20::Burn {
                    from: ZONE_OUTBOX_ADDRESS,
                    amount: U256::from(2075),
                },
            ),
            event_log(
                ZONE_OUTBOX_ADDRESS,
                IZoneOutbox::WithdrawalRequested {
                    withdrawalIndex: 4,
                    sender: Address::repeat_byte(0x44),
                    token: token_b,
                    to: Address::ZERO,
                    amount: 2000,
                    fee: 75,
                    memo: B256::ZERO,
                    gasLimit: 0,
                    fallbackNonce: 9,
                    data: Default::default(),
                    revealTo: Default::default(),
                },
            ),
            event_log(
                ZONE_OUTBOX_ADDRESS,
                IZoneOutbox::BatchFinalized {
                    withdrawalQueueHash: B256::repeat_byte(0xcc),
                    withdrawalBatchIndex: 7,
                },
            ),
        ];
        let events = collect(
            &[transaction()],
            &[receipt(logs)],
            BlockNumHash::new(4, B256::repeat_byte(4)),
        )
        .unwrap();

        assert_eq!(events.events.len(), 14);
        assert!(
            matches!(events.events[0].event, L2BridgeEvent::TempoAdvanced(ref anchor)
            if anchor.tempo_block_number == 100)
        );
        assert!(
            matches!(events.events[2].event, L2BridgeEvent::DepositOutcome {
            token, amount: 500, processed: true, ..
        } if token == token_a)
        );
        assert!(
            matches!(events.events[3].event, L2BridgeEvent::DepositOutcome {
            token, amount: 300, processed: false, ..
        } if token == token_b)
        );
        assert!(
            matches!(events.events[5].event, L2BridgeEvent::WithdrawalBounceBack {
            token, amount: 777, processed: true, ..
        } if token == token_a)
        );
        assert!(
            matches!(events.events[6].event, L2BridgeEvent::WithdrawalBounceBack {
            token, amount: 888, processed: false, ..
        } if token == token_b)
        );
        assert!(
            matches!(events.events[8].event, L2BridgeEvent::RefundClaimed {
            token, amount: 42, ..
        } if token == token_a)
        );
        assert!(
            matches!(events.events[9].event, L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 3, sender, token, principal: 1000, fee: 50,
        } if sender == Address::ZERO && token == token_a)
        );
        assert!(
            matches!(events.events[13].event, L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 4, sender, token, principal: 2000, fee: 75,
        } if sender == Address::repeat_byte(0x44) && token == token_b)
        );
    }

    #[test]
    fn rejects_missing_and_duplicate_anchor() {
        let block = BlockNumHash::new(4, B256::ZERO);
        let missing = collect(&[transaction()], &[receipt(vec![])], block).unwrap_err();
        assert!(missing.to_string().contains("missing TempoAdvanced"));

        let anchor = anchor_log(7);
        let duplicate = collect(
            &[transaction()],
            &[receipt(vec![anchor.clone(), anchor])],
            block,
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("duplicate TempoAdvanced"));
    }

    #[test]
    fn decodes_and_ignores_batch_finalized() {
        let batch = event_log(
            ZONE_OUTBOX_ADDRESS,
            IZoneOutbox::BatchFinalized {
                withdrawalQueueHash: B256::ZERO,
                withdrawalBatchIndex: 0,
            },
        );
        let events = collect(
            &[transaction()],
            &[receipt(vec![anchor_log(7), batch])],
            BlockNumHash::new(4, B256::ZERO),
        )
        .unwrap();
        assert_eq!(events.events.len(), 1);
    }

    #[test]
    fn rejects_malformed_known_event() {
        let malformed = Log {
            address: ZONE_INBOX_ADDRESS,
            data: LogData::new_unchecked(
                vec![IZoneInbox::TempoAdvanced::SIGNATURE_HASH],
                Bytes::from(vec![0xde, 0xad]),
            ),
        };
        let error = collect(
            &[transaction()],
            &[receipt(vec![malformed])],
            BlockNumHash::new(4, B256::ZERO),
        )
        .unwrap_err();
        assert!(error.to_string().contains("malformed TempoAdvanced"));
    }

    #[test]
    fn rejects_non_canonical_trailing_event_data() {
        let anchor = anchor_log(7);
        let mut data = anchor.data.data.to_vec();
        data.extend([0; 32]);
        let trailing = Log {
            address: anchor.address,
            data: LogData::new_unchecked(anchor.topics().to_vec(), Bytes::from(data)),
        };
        let error = collect(
            &[transaction()],
            &[receipt(vec![trailing])],
            BlockNumHash::new(4, B256::ZERO),
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-canonical TempoAdvanced"));
    }

    #[test]
    fn rejects_unknown_protocol_logs_and_ignores_other_emitters() {
        let anchor = anchor_log(7);
        for unsupported in [
            Log {
                address: ZONE_INBOX_ADDRESS,
                data: LogData::new_unchecked(vec![B256::repeat_byte(0x99)], Bytes::new()),
            },
            Log {
                address: ZONE_OUTBOX_ADDRESS,
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
        ] {
            assert!(
                collect(
                    &[transaction()],
                    &[receipt(vec![unsupported])],
                    BlockNumHash::new(4, B256::ZERO),
                )
                .is_err()
            );
        }
        let logs = vec![
            Log {
                address: Address::repeat_byte(0x88),
                data: anchor.data.clone(),
            },
            anchor,
        ];
        let events = collect(
            &[transaction()],
            &[receipt(logs)],
            BlockNumHash::new(4, B256::ZERO),
        )
        .unwrap();
        assert_eq!(events.events.len(), 1);
        assert!(matches!(
            events.events[0].event,
            L2BridgeEvent::TempoAdvanced(_)
        ));
    }

    #[test]
    fn rejects_transaction_receipt_count_mismatch() {
        let error = collect::<_, Receipt>(&[transaction()], &[], BlockNumHash::new(4, B256::ZERO))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has 1 transactions but 0 receipts")
        );
    }

    #[test]
    fn retains_receipt_provenance_across_all_logs() {
        let anchor = anchor_log(7);
        let noise = Log {
            address: Address::repeat_byte(9),
            data: anchor.data.clone(),
        };
        let receipt = Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs: vec![noise, anchor],
        };
        let tx = transaction();
        let events = collect(
            std::slice::from_ref(&tx),
            &[receipt],
            BlockNumHash::new(4, B256::repeat_byte(4)),
        )
        .unwrap();
        assert_eq!(events.events.len(), 1);
        let evidence = anchor_evidence(&events);
        assert_eq!(evidence.transaction_hash, *tx.tx_hash());
        assert_eq!(evidence.transaction_index, 0);
        assert_eq!(events.l1_anchor().unwrap().block_number(), 7);
    }

    #[test]
    fn failed_logs_are_not_evidence() {
        let failed = Receipt {
            tx_type: TxType::Legacy,
            success: false,
            cumulative_gas_used: 0,
            logs: vec![anchor_log(6)],
        };
        let successful = Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs: vec![anchor_log(7)],
        };
        let events = collect(
            &[transaction(), transaction()],
            &[failed, successful],
            BlockNumHash::new(4, B256::repeat_byte(4)),
        )
        .unwrap();
        assert_eq!(anchor_evidence(&events).transaction_index, 1);
    }

    fn evidence(transaction: u32, event: L2BridgeEvent) -> L2EventEvidence {
        L2EventEvidence {
            transaction_hash: B256::with_last_byte(transaction as u8),
            transaction_index: transaction,
            event,
        }
    }

    fn burn(transaction: u32, token: Address, owner: Address, amount: u64) -> Vec<L2EventEvidence> {
        let amount = U256::from(amount);
        vec![
            evidence(
                transaction,
                L2BridgeEvent::Transfer {
                    token,
                    from: owner,
                    to: ZONE_OUTBOX_ADDRESS,
                    amount,
                },
            ),
            evidence(
                transaction,
                L2BridgeEvent::Transfer {
                    token,
                    from: ZONE_OUTBOX_ADDRESS,
                    to: Address::ZERO,
                    amount,
                },
            ),
            evidence(
                transaction,
                L2BridgeEvent::TokenBurn {
                    token,
                    from: ZONE_OUTBOX_ADDRESS,
                    amount,
                },
            ),
        ]
    }

    fn withdrawal(
        transaction: u32,
        token: Address,
        sender: Address,
        principal: u128,
        fee: u128,
    ) -> L2EventEvidence {
        evidence(
            transaction,
            L2BridgeEvent::WithdrawalRequested {
                withdrawal_index: 0,
                sender,
                token,
                principal,
                fee,
            },
        )
    }

    fn authenticate_withdrawals(events: Vec<L2EventEvidence>) -> eyre::Result<()> {
        L2Events {
            events,
            anchor_index: 0,
        }
        .authenticate_withdrawals()
    }

    fn authenticate_mints(events: Vec<L2EventEvidence>) -> eyre::Result<()> {
        L2Events {
            events,
            anchor_index: 0,
        }
        .authenticate_mints()
    }

    #[test]
    fn authenticates_inbox_mints() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let outcome = evidence(
            1,
            L2BridgeEvent::DepositOutcome {
                recipient: Some(recipient),
                token,
                amount: 10,
                processed: true,
            },
        );
        let mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: recipient,
                amount: U256::from(10),
            },
        );

        authenticate_mints(vec![mint, outcome]).unwrap();
    }

    #[test]
    fn rejects_unmatched_inbox_mints() {
        let token = Address::repeat_byte(1);
        let outcome = evidence(
            1,
            L2BridgeEvent::DepositOutcome {
                recipient: Some(Address::repeat_byte(2)),
                token,
                amount: 10,
                processed: true,
            },
        );
        let wrong_mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: Address::repeat_byte(3),
                amount: U256::from(10),
            },
        );
        let unexpected_mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: Address::repeat_byte(4),
                amount: U256::from(10),
            },
        );

        assert!(authenticate_mints(vec![wrong_mint, outcome]).is_err());
        assert!(authenticate_mints(vec![unexpected_mint]).is_err());
    }

    #[test]
    fn authenticates_mints_to_virtual_recipients() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let master = Address::repeat_byte(3);
        let amount = U256::from(10);
        let mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: recipient,
                amount,
            },
        );
        let forward = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: recipient,
                to: master,
                amount,
            },
        );
        let outcome = evidence(
            1,
            L2BridgeEvent::DepositOutcome {
                recipient: Some(recipient),
                token,
                amount: 10,
                processed: true,
            },
        );

        authenticate_mints(vec![mint, forward, outcome]).unwrap();
    }

    #[test]
    fn rejects_inbox_mints_paired_across_receipts() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: recipient,
                amount: U256::from(10),
            },
        );
        let outcome = evidence(
            2,
            L2BridgeEvent::DepositOutcome {
                recipient: Some(recipient),
                token,
                amount: 10,
                processed: true,
            },
        );

        assert!(authenticate_mints(vec![mint, outcome]).is_err());
    }

    #[test]
    fn rejects_inbox_mints_with_an_intervening_recognized_event() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let mint = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: Address::ZERO,
                to: recipient,
                amount: U256::from(10),
            },
        );
        let transfer = evidence(
            1,
            L2BridgeEvent::Transfer {
                token,
                from: recipient,
                to: Address::repeat_byte(3),
                amount: U256::from(1),
            },
        );
        let outcome = evidence(
            1,
            L2BridgeEvent::DepositOutcome {
                recipient: Some(recipient),
                token,
                amount: 10,
                processed: true,
            },
        );

        assert!(authenticate_mints(vec![mint, transfer, outcome]).is_err());
    }

    #[test]
    fn authenticates_sender_paid_withdrawal() {
        let token = address!("20c0000000000000000000000000000000000000");
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 105);
        events.push(withdrawal(1, token, sender, 100, 5));
        authenticate_withdrawals(events).unwrap();
    }

    #[test]
    fn authenticates_sponsored_withdrawal() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let fee_payer = Address::repeat_byte(3);
        let mut events = burn(1, token, sender, 100);
        events.extend(burn(1, token, fee_payer, 5));
        events.push(withdrawal(1, token, sender, 100, 5));
        authenticate_withdrawals(events).unwrap();
    }

    #[test]
    fn rejects_wrong_withdrawal_burn_amount() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 101);
        events.push(withdrawal(1, token, sender, 100, 0));
        assert!(authenticate_withdrawals(events).is_err());
    }

    #[test]
    fn authenticates_multiple_withdrawals_in_one_transaction() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 100);
        events.push(withdrawal(1, token, sender, 100, 0));
        events.extend(burn(1, token, sender, 205));
        events.push(withdrawal(1, token, sender, 200, 5));
        authenticate_withdrawals(events).unwrap();
    }

    #[test]
    fn authenticates_withdrawal_after_unrelated_receipt_events() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 100);
        events.push(evidence(
            1,
            L2BridgeEvent::RefundClaimed {
                recipient: sender,
                token,
                amount: 1,
            },
        ));
        events.push(withdrawal(1, token, sender, 100, 0));

        authenticate_withdrawals(events).unwrap();
    }

    #[test]
    fn rejects_reusing_one_burn_for_multiple_withdrawals() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 100);
        events.push(withdrawal(1, token, sender, 100, 0));
        events.push(withdrawal(1, token, sender, 100, 0));

        assert!(authenticate_withdrawals(events).is_err());
    }

    #[test]
    fn rejects_unexplained_withdrawal_burn() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);

        assert!(authenticate_withdrawals(burn(1, token, sender, 100)).is_err());
    }

    #[test]
    fn rejects_missing_withdrawal_burn_event() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 100);
        events.remove(2);
        events.push(withdrawal(1, token, sender, 100, 0));
        assert!(authenticate_withdrawals(events).is_err());
    }

    #[test]
    fn rejects_cross_transaction_withdrawal_evidence() {
        let token = Address::repeat_byte(1);
        let sender = Address::repeat_byte(2);
        let mut events = burn(1, token, sender, 100);
        events.push(withdrawal(2, token, sender, 100, 0));
        assert!(authenticate_withdrawals(events).is_err());
    }

    #[test]
    fn deposit_bounce_back_does_not_require_a_burn() {
        let event = evidence(
            1,
            L2BridgeEvent::WithdrawalRequested {
                withdrawal_index: 0,
                sender: Address::ZERO,
                token: Address::repeat_byte(1),
                principal: 100,
                fee: 0,
            },
        );
        authenticate_withdrawals(vec![event]).unwrap();
    }
}
