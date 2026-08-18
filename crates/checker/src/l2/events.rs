//! Decoded L2 Inbox/Outbox events with their canonical receipt provenance.

use alloy_consensus::{TxReceipt, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::SolEvent;
use eyre::WrapErr as _;
use tempo_precompiles::tip20::{ITIP20, TIP20Token};
use tempo_zone_contracts::{IZoneInbox, IZoneOutbox, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use crate::decode_event;

/// Exact Tempo/L1 block imported by `TempoAdvanced`.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct L1Anchor {
    pub(super) tempo_block_hash: B256,
    pub(super) tempo_block_number: u64,
    pub(super) deposits_processed: u64,
    pub(super) processed_deposit_queue_hash: B256,
    pub(super) last_processed_deposit_number: u64,
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
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum L2BridgeEvent {
    /// Required link to the imported Tempo/L1 block.
    TempoAdvanced(L1Anchor),
    /// Successful or failed deposit execution.
    DepositOutcome {
        deposit_hash: B256,
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
    /// Refund consumed on the Zone.
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
    /// Token initialized for Zone bridging.
    TokenEnabled {
        token: Address,
        name: String,
        symbol: String,
        currency: String,
    },
    /// User withdrawal, preserving principal and fee separately.
    WithdrawalRequested {
        withdrawal_index: u64,
        sender: Address,
        token: Address,
        principal: u128,
        fee: u128,
        fallback_nonce: u64,
        is_deposit_bounce_back: bool,
    },
    /// Finalized withdrawal queue boundary.
    BatchFinalized {
        withdrawal_queue_hash: B256,
        withdrawal_batch_index: u64,
    },
}

/// One decoded bridge event and its canonical receipt provenance.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct L2EventEvidence {
    pub(super) transaction_hash: B256,
    pub(super) transaction_index: u32,
    pub(super) transaction_log_index: u32,
    pub(super) block_log_index: u32,
    pub(super) raw_log: Log,
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
    pub(crate) fn l1_anchor(&self) -> &L1Anchor {
        match &self.events[self.anchor_index].event {
            L2BridgeEvent::TempoAdvanced(anchor) => anchor,
            _ => unreachable!("anchor index is established during collection"),
        }
    }

    #[cfg(test)]
    pub(crate) fn anchor_evidence(&self) -> &L2EventEvidence {
        &self.events[self.anchor_index]
    }

    /// Return enabled token addresses in canonical event order.
    pub(crate) fn token_enabled_addresses(&self) -> Vec<Address> {
        self.token_enabled_events()
            .map(|evidence| match &evidence.event {
                L2BridgeEvent::TokenEnabled { token, .. } => *token,
                _ => unreachable!(),
            })
            .collect()
    }

    /// Return token specs from `TokenEnabled` events in canonical event order.
    pub(crate) fn token_enabled_specs(&self) -> Vec<crate::model::TokenSpec> {
        self.token_enabled_events()
            .map(|evidence| match &evidence.event {
                L2BridgeEvent::TokenEnabled {
                    token,
                    name,
                    symbol,
                    currency,
                } => crate::model::TokenSpec {
                    token: *token,
                    name: name.clone(),
                    symbol: symbol.clone(),
                    currency: currency.clone(),
                },
                _ => unreachable!(),
            })
            .collect()
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

    fn token_enabled_events(&self) -> impl Iterator<Item = &L2EventEvidence> {
        self.events
            .iter()
            .filter(|evidence| matches!(evidence.event, L2BridgeEvent::TokenEnabled { .. }))
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
                deposits_processed: u64::try_from(event.depositsProcessed)
                    .wrap_err("depositsProcessed overflows u64")?,
                processed_deposit_queue_hash: event.newProcessedDepositQueueHash,
                last_processed_deposit_number: event.lastProcessedDepositNumber,
            })
        }
        IZoneInbox::DepositProcessed::SIGNATURE_HASH => {
            let event =
                decode_event::<IZoneInbox::DepositProcessed>(log, "DepositProcessed", block)?;
            L2BridgeEvent::DepositOutcome {
                deposit_hash: event.depositHash,
                recipient: Some(event.to),
                token: event.token,
                amount: event.amount,
                processed: true,
            }
        }
        IZoneInbox::DepositFailed::SIGNATURE_HASH => {
            let event = decode_event::<IZoneInbox::DepositFailed>(log, "DepositFailed", block)?;
            L2BridgeEvent::DepositOutcome {
                deposit_hash: event.depositHash,
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
            let event = decode_event::<IZoneInbox::TokenEnabled>(log, "TokenEnabled", block)?;
            L2BridgeEvent::TokenEnabled {
                token: event.token,
                name: event.name,
                symbol: event.symbol,
                currency: event.currency,
            }
        }
        IZoneInbox::DepositRejected::SIGNATURE_HASH => {
            eyre::bail!("unsupported DepositRejected in block {block}")
        }
        _ => eyre::bail!("unsupported ZoneInbox event {topic} in block {block}"),
    }))
}

/// Decode a canonical TIP-20 transfer emitted by a token precompile.
fn decode_transfer(log: &Log, block: u64) -> eyre::Result<Option<L2BridgeEvent>> {
    if log.topics().first() != Some(&ITIP20::Transfer::SIGNATURE_HASH)
        || TIP20Token::from_address(log.address).is_err()
    {
        return Ok(None);
    }
    let event = decode_event::<ITIP20::Transfer>(log, "Transfer", block)?;
    Ok(Some(L2BridgeEvent::Transfer {
        token: log.address,
        from: event.from,
        to: event.to,
        amount: event.amount,
    }))
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
                fallback_nonce: event.fallbackNonce,
                is_deposit_bounce_back: event.sender.is_zero(),
            }
        }
        IZoneOutbox::BatchFinalized::SIGNATURE_HASH => {
            let event = decode_event::<IZoneOutbox::BatchFinalized>(log, "BatchFinalized", block)?;
            L2BridgeEvent::BatchFinalized {
                withdrawal_queue_hash: event.withdrawalQueueHash,
                withdrawal_batch_index: event.withdrawalBatchIndex,
            }
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

/// Builds ordered event evidence while receipts are visited once.
#[derive(Default)]
pub(super) struct EventCollector {
    events: Vec<L2EventEvidence>,
    anchor_index: Option<usize>,
    batch_seen: bool,
    block_log_index: u32,
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
            self.block_log_index += receipt.logs().len() as u32;
            return Ok(());
        }

        for (transaction_log_index, log) in receipt.logs().iter().enumerate() {
            let event = match log.address {
                ZONE_INBOX_ADDRESS => decode_inbox(log, block)?,
                ZONE_OUTBOX_ADDRESS => decode_outbox(log, block)?,
                _ => decode_transfer(log, block)?,
            };
            if let Some(event) = event {
                if matches!(event, L2BridgeEvent::TempoAdvanced(_)) {
                    eyre::ensure!(
                        self.anchor_index.is_none(),
                        "duplicate TempoAdvanced in block {block}"
                    );
                    self.anchor_index = Some(self.events.len());
                }
                if matches!(event, L2BridgeEvent::BatchFinalized { .. }) {
                    eyre::ensure!(
                        !self.batch_seen,
                        "duplicate BatchFinalized in block {block}"
                    );
                    self.batch_seen = true;
                }
                self.events.push(L2EventEvidence {
                    transaction_hash: *transaction.tx_hash(),
                    transaction_index: transaction_index as u32,
                    transaction_log_index: transaction_log_index as u32,
                    block_log_index: self.block_log_index,
                    raw_log: log.clone(),
                    event,
                });
            }
            self.block_log_index += 1;
        }
        Ok(())
    }

    /// Require the L1 anchor and finish the ordered event bundle.
    pub(super) fn finish(self, block: u64) -> eyre::Result<L2Events> {
        let anchor_index = self
            .anchor_index
            .ok_or_else(|| eyre::eyre!("block {block} is missing TempoAdvanced"))?;
        Ok(L2Events {
            events: self.events,
            anchor_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction as _, TxLegacy, TxType};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Bytes, LogData, Signature, TxKind, U256};
    use alloy_sol_types::SolEvent;
    use reth_ethereum_primitives::Receipt;

    fn collect<T, R>(
        transactions: &[T],
        receipts: &[R],
        block: BlockNumHash,
    ) -> eyre::Result<L2Events>
    where
        T: TxHashRef,
        R: TxReceipt<Log = Log>,
    {
        eyre::ensure!(
            transactions.len() == receipts.len(),
            "block {} has {} transactions but {} receipts",
            block.number,
            transactions.len(),
            receipts.len()
        );
        let mut collector = EventCollector::default();
        for (index, (transaction, receipt)) in transactions.iter().zip(receipts).enumerate() {
            collector.extract_receipt(index, transaction, receipt, block.number)?;
        }
        collector.finish(block.number)
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
        let token_a = Address::repeat_byte(0xa1);
        let token_b = Address::repeat_byte(0xb2);
        let logs = vec![
            anchor_log(100),
            event_log(
                ZONE_INBOX_ADDRESS,
                IZoneInbox::DepositProcessed {
                    depositHash: B256::repeat_byte(0xd0),
                    sender: Address::ZERO,
                    to: Address::ZERO,
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
                ZONE_INBOX_ADDRESS,
                IZoneInbox::WithdrawalBounceBackProcessed {
                    zoneFallbackRecipient: Address::ZERO,
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
                ZONE_INBOX_ADDRESS,
                IZoneInbox::RefundClaimed {
                    recipient: Address::ZERO,
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

        assert_eq!(events.events.len(), 10);
        assert!(
            matches!(events.events[0].event, L2BridgeEvent::TempoAdvanced(ref anchor)
            if anchor.tempo_block_number == 100 && anchor.deposits_processed == 2)
        );
        assert!(
            matches!(events.events[1].event, L2BridgeEvent::DepositOutcome {
            deposit_hash, token, amount: 500, processed: true, ..
        } if deposit_hash == B256::repeat_byte(0xd0) && token == token_a)
        );
        assert!(
            matches!(events.events[2].event, L2BridgeEvent::DepositOutcome {
            deposit_hash, token, amount: 300, processed: false, ..
        } if deposit_hash == B256::repeat_byte(0xd1) && token == token_b)
        );
        assert!(
            matches!(events.events[3].event, L2BridgeEvent::WithdrawalBounceBack {
            token, amount: 777, processed: true, ..
        } if token == token_a)
        );
        assert!(
            matches!(events.events[4].event, L2BridgeEvent::WithdrawalBounceBack {
            token, amount: 888, processed: false, ..
        } if token == token_b)
        );
        assert!(
            matches!(events.events[5].event, L2BridgeEvent::RefundClaimed {
            token, amount: 42, ..
        } if token == token_a)
        );
        assert!(
            matches!(&events.events[6].event, L2BridgeEvent::TokenEnabled {
            token, name, symbol, currency
        } if *token == token_b && name == "Token B" && symbol == "TB" && currency == "USD")
        );
        assert!(
            matches!(events.events[7].event, L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 3, sender, token, principal: 1000, fee: 50,
            fallback_nonce: 8, is_deposit_bounce_back: true
        } if sender == Address::ZERO && token == token_a)
        );
        assert!(
            matches!(events.events[8].event, L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 4, sender, token, principal: 2000, fee: 75,
            fallback_nonce: 9, is_deposit_bounce_back: false
        } if sender == Address::repeat_byte(0x44) && token == token_b)
        );
        assert!(
            matches!(events.events[9].event, L2BridgeEvent::BatchFinalized {
            withdrawal_queue_hash, withdrawal_batch_index: 7
        } if withdrawal_queue_hash == B256::repeat_byte(0xcc))
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
    fn rejects_duplicate_batch_finalized() {
        let batch = event_log(
            ZONE_OUTBOX_ADDRESS,
            IZoneOutbox::BatchFinalized {
                withdrawalQueueHash: B256::ZERO,
                withdrawalBatchIndex: 0,
            },
        );
        let error = collect(
            &[transaction()],
            &[receipt(vec![anchor_log(7), batch.clone(), batch])],
            BlockNumHash::new(4, B256::ZERO),
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate BatchFinalized"));
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
    fn retains_order_and_provenance_across_all_logs() {
        let raw_anchor = anchor_log(7);
        let noise = Log {
            address: Address::repeat_byte(9),
            data: raw_anchor.data.clone(),
        };
        let receipt = Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs: vec![noise, raw_anchor.clone()],
        };
        let tx = transaction();
        let events = collect(
            std::slice::from_ref(&tx),
            &[receipt],
            BlockNumHash::new(4, B256::repeat_byte(4)),
        )
        .unwrap();
        assert_eq!(events.events.len(), 1);
        let evidence = events.anchor_evidence();
        assert_eq!(evidence.transaction_hash, *tx.tx_hash());
        assert_eq!(evidence.transaction_index, 0);
        assert_eq!(evidence.transaction_log_index, 1);
        assert_eq!(evidence.block_log_index, 1);
        assert_eq!(evidence.raw_log, raw_anchor);
        assert_eq!(events.l1_anchor().block_number(), 7);
    }

    #[test]
    fn failed_logs_are_not_evidence_but_count_in_block_index() {
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
        assert_eq!(events.anchor_evidence().block_log_index, 1);
        assert_eq!(events.anchor_evidence().transaction_index, 1);
    }
}
