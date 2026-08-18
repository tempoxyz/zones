//! Decoded L1 Portal events with their canonical receipt provenance.

use alloy_network::ReceiptResponse as _;
use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::SolEvent;
use tempo_alloy::rpc::TempoTransactionReceipt;
use tempo_zone_contracts::ZonePortal;

use crate::decode_event;

/// Semantic value of one recognized Zone Portal event.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum L1PortalEvent {
    /// A user deposit escrowed on L1 — new external backing entering the bridge.
    DepositMade {
        token: Address,
        net_amount: u128,
        fee: u128,
        tempo_refund_recipient: Address,
        deposit_number: u64,
        deposit_queue_hash: B256,
    },
    /// A TIP-20 token newly enabled for bridging, with metadata.
    TokenEnabled {
        token: Address,
        name: String,
        symbol: String,
        currency: String,
    },
    /// A finalized withdrawal batch submitted to L1.
    BatchSubmitted {
        withdrawal_batch_index: u64,
        withdrawal_queue_index: U256,
        withdrawal_queue_hash: B256,
        next_block_hash: B256,
        last_processed_deposit_number: u64,
    },
    /// A withdrawal paid out on L1.
    WithdrawalProcessed {
        to: Address,
        sender_tag: B256,
        token: Address,
        amount: u128,
        callback_success: bool,
    },
    /// A withdrawal bounce-back — recycles existing Portal backing, not a new
    /// external deposit.  Kept distinct from [`Self::DepositMade`].
    WithdrawalBounceBack {
        token: Address,
        amount: u128,
        fallback_nonce: u64,
        deposit_number: u64,
        deposit_queue_hash: B256,
    },
    /// A deposit bounce-back processed on L1 (fee deducted, refund sent).
    DepositBounceBack {
        tempo_refund_recipient: Address,
        token: Address,
        amount: u128,
        bounceback_fee: u128,
    },
    /// A deposit bounce-back still pending on L1.
    DepositBounceBackPending {
        tempo_refund_recipient: Address,
        token: Address,
        amount: u128,
        bounceback_fee: u128,
    },
    /// A Zone refund claimed on L1.
    RefundClaimed {
        recipient: Address,
        token: Address,
        amount: u128,
    },
}

/// One decoded Portal event and its canonical receipt provenance.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct L1EventEvidence {
    pub(crate) transaction_hash: B256,
    pub(crate) transaction_index: u32,
    pub(crate) transaction_log_index: u32,
    pub(crate) block_log_index: u32,
    pub(crate) raw_log: Log,
    pub(crate) event: L1PortalEvent,
}

/// Ordered recognized Portal events for one L1 block.
#[derive(Debug)]
pub(super) struct L1Events {
    pub(super) portal: Address,
    pub(super) events: Vec<L1EventEvidence>,
}

impl L1Events {
    /// Return token specs from `TokenEnabled` events in canonical event order.
    pub(crate) fn token_enabled_specs(&self) -> Vec<crate::model::TokenSpec> {
        self.events
            .iter()
            .filter_map(|evidence| match &evidence.event {
                L1PortalEvent::TokenEnabled {
                    token,
                    name,
                    symbol,
                    currency,
                } => Some(crate::model::TokenSpec {
                    token: *token,
                    name: name.clone(),
                    symbol: symbol.clone(),
                    currency: currency.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Decode one Portal event and fail closed on unknown protocol topics.
///
/// Only logs emitted from `portal` are passed to this function. A known event
/// that fails ABI decoding returns a contextual error.
fn decode_portal_event(log: &Log, block: u64) -> eyre::Result<Option<L1PortalEvent>> {
    let topic = log
        .topics()
        .first()
        .ok_or_else(|| eyre::eyre!("topicless Portal log in block {block}"))?;
    macro_rules! ignored {
        ($event:ty, $name:literal) => {{
            decode_event::<$event>(log, $name, block)?;
            return Ok(None);
        }};
    }
    Ok(Some(match *topic {
        ZonePortal::DepositMade::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::DepositMade>(log, "DepositMade", block)?;
            L1PortalEvent::DepositMade {
                token: e.token,
                net_amount: e.netAmount,
                fee: e.fee,
                tempo_refund_recipient: e.tempoRefundRecipient,
                deposit_number: e.depositNumber,
                deposit_queue_hash: e.newCurrentDepositQueueHash,
            }
        }
        ZonePortal::TokenEnabled::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::TokenEnabled>(log, "TokenEnabled", block)?;
            L1PortalEvent::TokenEnabled {
                token: e.token,
                name: e.name,
                symbol: e.symbol,
                currency: e.currency,
            }
        }
        ZonePortal::BatchSubmitted::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::BatchSubmitted>(log, "BatchSubmitted", block)?;
            L1PortalEvent::BatchSubmitted {
                withdrawal_batch_index: e.withdrawalBatchIndex,
                withdrawal_queue_index: e.withdrawalQueueIndex,
                withdrawal_queue_hash: e.withdrawalQueueHash,
                next_block_hash: e.nextBlockHash,
                last_processed_deposit_number: e.lastProcessedDepositNumber,
            }
        }
        ZonePortal::WithdrawalProcessed::SIGNATURE_HASH => {
            let e =
                decode_event::<ZonePortal::WithdrawalProcessed>(log, "WithdrawalProcessed", block)?;
            L1PortalEvent::WithdrawalProcessed {
                to: e.to,
                sender_tag: e.senderTag,
                token: e.token,
                amount: e.amount,
                callback_success: e.callbackSuccess,
            }
        }
        ZonePortal::WithdrawalBounceBack::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::WithdrawalBounceBack>(
                log,
                "WithdrawalBounceBack",
                block,
            )?;
            L1PortalEvent::WithdrawalBounceBack {
                token: e.token,
                amount: e.amount,
                fallback_nonce: e.fallbackNonce,
                deposit_number: e.depositNumber,
                deposit_queue_hash: e.newCurrentDepositQueueHash,
            }
        }
        ZonePortal::DepositBounceBack::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::DepositBounceBack>(log, "DepositBounceBack", block)?;
            L1PortalEvent::DepositBounceBack {
                tempo_refund_recipient: e.tempoRefundRecipient,
                token: e.token,
                amount: e.amount,
                bounceback_fee: e.bouncebackFee,
            }
        }
        ZonePortal::DepositBounceBackPending::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::DepositBounceBackPending>(
                log,
                "DepositBounceBackPending",
                block,
            )?;
            L1PortalEvent::DepositBounceBackPending {
                tempo_refund_recipient: e.tempoRefundRecipient,
                token: e.token,
                amount: e.amount,
                bounceback_fee: e.bouncebackFee,
            }
        }
        ZonePortal::RefundClaimed::SIGNATURE_HASH => {
            let e = decode_event::<ZonePortal::RefundClaimed>(log, "RefundClaimed", block)?;
            L1PortalEvent::RefundClaimed {
                recipient: e.recipient,
                token: e.token,
                amount: e.amount,
            }
        }
        ZonePortal::DepositsPaused::SIGNATURE_HASH => {
            ignored!(ZonePortal::DepositsPaused, "DepositsPaused")
        }
        ZonePortal::DepositsResumed::SIGNATURE_HASH => {
            ignored!(ZonePortal::DepositsResumed, "DepositsResumed")
        }
        ZonePortal::PortalPaused::SIGNATURE_HASH => {
            ignored!(ZonePortal::PortalPaused, "PortalPaused")
        }
        ZonePortal::PortalResumed::SIGNATURE_HASH => {
            ignored!(ZonePortal::PortalResumed, "PortalResumed")
        }
        ZonePortal::AbdicationScheduled::SIGNATURE_HASH => {
            ignored!(ZonePortal::AbdicationScheduled, "AbdicationScheduled")
        }
        ZonePortal::RpcUrlUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::RpcUrlUpdated, "RpcUrlUpdated")
        }
        ZonePortal::SequencerEncryptionKeyUpdated::SIGNATURE_HASH => ignored!(
            ZonePortal::SequencerEncryptionKeyUpdated,
            "SequencerEncryptionKeyUpdated"
        ),
        ZonePortal::ZoneGasRateUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::ZoneGasRateUpdated, "ZoneGasRateUpdated")
        }
        ZonePortal::MaxTempoGasRateUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::MaxTempoGasRateUpdated, "MaxTempoGasRateUpdated")
        }
        ZonePortal::BouncebackGasUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::BouncebackGasUpdated, "BouncebackGasUpdated")
        }
        ZonePortal::AdminTransferStarted::SIGNATURE_HASH => {
            ignored!(ZonePortal::AdminTransferStarted, "AdminTransferStarted")
        }
        ZonePortal::AdminTransferred::SIGNATURE_HASH => {
            ignored!(ZonePortal::AdminTransferred, "AdminTransferred")
        }
        ZonePortal::RoleUpdated::SIGNATURE_HASH => ignored!(ZonePortal::RoleUpdated, "RoleUpdated"),
        ZonePortal::EnforcementModesUpdated::SIGNATURE_HASH => ignored!(
            ZonePortal::EnforcementModesUpdated,
            "EnforcementModesUpdated"
        ),
        ZonePortal::LeaderUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::LeaderUpdated, "LeaderUpdated")
        }
        ZonePortal::SequencerSetUpdated::SIGNATURE_HASH => {
            ignored!(ZonePortal::SequencerSetUpdated, "SequencerSetUpdated")
        }
        _ => eyre::bail!("unsupported Portal event {topic} in block {block}"),
    }))
}

/// Builds ordered event evidence while receipts are visited once.
#[derive(Default)]
pub(super) struct EventCollector {
    portal: Address,
    events: Vec<L1EventEvidence>,
    block_log_index: u32,
}

impl EventCollector {
    /// Create a collector that only decodes logs from `portal`.
    pub(super) fn new(portal: Address) -> Self {
        Self {
            portal,
            ..Default::default()
        }
    }

    /// Decode recognized Portal logs from one canonical receipt.
    ///
    /// Failed receipts are skipped but their logs still count toward the
    /// block-global log index. Only logs from `portal` are decoded. Unknown
    /// Portal topics are ignored. A known event that fails ABI decoding
    /// returns a contextual error.
    pub(super) fn extract_receipt(
        &mut self,
        transaction_index: usize,
        transaction_hash: B256,
        receipt: &TempoTransactionReceipt,
        block: u64,
    ) -> eyre::Result<()> {
        if !receipt.status() {
            self.block_log_index += receipt.logs().len() as u32;
            return Ok(());
        }
        for (transaction_log_index, log) in receipt.logs().iter().enumerate() {
            if log.address() == self.portal
                && let Some(event) = decode_portal_event(&log.inner, block)?
            {
                self.events.push(L1EventEvidence {
                    transaction_hash,
                    transaction_index: transaction_index as u32,
                    transaction_log_index: transaction_log_index as u32,
                    block_log_index: self.block_log_index,
                    raw_log: log.inner.clone(),
                    event,
                });
            }
            self.block_log_index += 1;
        }
        Ok(())
    }

    /// Finish the ordered event bundle.
    pub(super) fn finish(self) -> L1Events {
        L1Events {
            portal: self.portal,
            events: self.events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{Bloom, U256, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use tempo_alloy::rpc::TempoTransactionReceipt;
    use tempo_primitives::{TempoReceipt, TempoTxType};

    const PORTAL: Address = address!("0x0000000000000000000000000000000000000abc");
    const BLOCK: u64 = 100;

    fn receipt(
        success: bool,
        hash: B256,
        logs: Vec<alloy_rpc_types_eth::Log>,
    ) -> TempoTransactionReceipt {
        TempoTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom::new(
                    TempoReceipt {
                        tx_type: TempoTxType::Legacy,
                        success,
                        cumulative_gas_used: 0,
                        logs,
                    },
                    Bloom::ZERO,
                ),
                transaction_hash: hash,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(BLOCK),
                gas_used: 0,
                effective_gas_price: 0,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: Some(Address::ZERO),
                contract_address: None,
            },
            fee_token: None,
            fee_payer: Address::ZERO,
        }
    }

    fn log(data: alloy_primitives::LogData) -> alloy_rpc_types_eth::Log {
        alloy_rpc_types_eth::Log {
            inner: Log {
                address: PORTAL,
                data,
            },
            ..Default::default()
        }
    }
    fn deposit() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::DepositMade {
            newCurrentDepositQueueHash: B256::repeat_byte(1),
            sender: Address::ZERO,
            token: Address::repeat_byte(3),
            netAmount: 500,
            fee: 10,
            keyIndex: U256::ZERO,
            ephemeralPubkeyX: B256::ZERO,
            ephemeralPubkeyYParity: 0,
            ciphertext: Default::default(),
            nonce: [0; 12].into(),
            tag: [0; 16].into(),
            tempoRefundRecipient: Address::repeat_byte(4),
            depositNumber: 7,
        }
        .encode_log_data())
    }
    fn token() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::TokenEnabled {
            token: Address::repeat_byte(5),
            name: "Test".into(),
            symbol: "TST".into(),
            currency: "USD".into(),
        }
        .encode_log_data())
    }
    fn batch(index: U256) -> alloy_rpc_types_eth::Log {
        log(ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: 3,
            withdrawalQueueIndex: index,
            nextProcessedDepositQueueHash: B256::ZERO,
            nextBlockHash: B256::repeat_byte(6),
            withdrawalQueueHash: B256::repeat_byte(7),
            lastProcessedDepositNumber: 9,
        }
        .encode_log_data())
    }
    fn withdrawal() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::WithdrawalProcessed {
            to: Address::repeat_byte(8),
            senderTag: B256::repeat_byte(9),
            token: Address::repeat_byte(10),
            amount: 1000,
            callbackSuccess: true,
        }
        .encode_log_data())
    }
    fn withdrawal_bounce() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::WithdrawalBounceBack {
            newCurrentDepositQueueHash: B256::repeat_byte(11),
            fallbackNonce: 42,
            token: Address::repeat_byte(12),
            amount: 777,
            depositNumber: 5,
        }
        .encode_log_data())
    }
    fn deposit_bounce() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::DepositBounceBack {
            tempoRefundRecipient: Address::repeat_byte(13),
            token: Address::repeat_byte(14),
            amount: 300,
            bouncebackFee: 5,
        }
        .encode_log_data())
    }
    fn pending() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::DepositBounceBackPending {
            tempoRefundRecipient: Address::repeat_byte(15),
            token: Address::repeat_byte(16),
            amount: 200,
            bouncebackFee: 3,
        }
        .encode_log_data())
    }
    fn refund() -> alloy_rpc_types_eth::Log {
        log(ZonePortal::RefundClaimed {
            recipient: Address::repeat_byte(17),
            token: Address::repeat_byte(18),
            amount: 42,
        }
        .encode_log_data())
    }

    fn collect(receipts: &[TempoTransactionReceipt]) -> eyre::Result<L1Events> {
        let mut collector = EventCollector::new(PORTAL);
        for (index, receipt) in receipts.iter().enumerate() {
            collector.extract_receipt(index, receipt.transaction_hash(), receipt, BLOCK)?;
        }
        Ok(collector.finish())
    }

    #[test]
    fn extract_all_portal_event_variants_in_order() {
        let events = collect(&[receipt(
            true,
            B256::ZERO,
            vec![
                deposit(),
                token(),
                batch(U256::ONE),
                withdrawal(),
                withdrawal_bounce(),
                deposit_bounce(),
                pending(),
                refund(),
            ],
        )])
        .unwrap();
        assert!(matches!(
            events.events[0].event,
            L1PortalEvent::DepositMade {
                deposit_number: 7,
                ..
            }
        ));
        assert!(matches!(
            events.events[1].event,
            L1PortalEvent::TokenEnabled { .. }
        ));
        assert!(matches!(
            events.events[2].event,
            L1PortalEvent::BatchSubmitted { .. }
        ));
        assert!(matches!(
            events.events[3].event,
            L1PortalEvent::WithdrawalProcessed { .. }
        ));
        assert!(matches!(
            events.events[4].event,
            L1PortalEvent::WithdrawalBounceBack { .. }
        ));
        assert!(matches!(
            events.events[5].event,
            L1PortalEvent::DepositBounceBack { .. }
        ));
        assert!(matches!(
            events.events[6].event,
            L1PortalEvent::DepositBounceBackPending { .. }
        ));
        assert!(matches!(
            events.events[7].event,
            L1PortalEvent::RefundClaimed { amount: 42, .. }
        ));
    }

    #[test]
    fn canonical_order_across_transactions() {
        let events = collect(&[
            receipt(true, B256::ZERO, vec![deposit()]),
            receipt(true, B256::ZERO, vec![withdrawal(), token()]),
        ])
        .unwrap();
        assert!(matches!(
            events.events[0].event,
            L1PortalEvent::DepositMade { .. }
        ));
        assert!(matches!(
            events.events[1].event,
            L1PortalEvent::WithdrawalProcessed { .. }
        ));
        assert!(matches!(
            events.events[2].event,
            L1PortalEvent::TokenEnabled { .. }
        ));
    }

    #[test]
    fn withdrawal_bounce_back_distinct_from_deposit_made() {
        let events = collect(&[receipt(
            true,
            B256::ZERO,
            vec![deposit(), withdrawal_bounce()],
        )])
        .unwrap();
        assert!(matches!(
            events.events[0].event,
            L1PortalEvent::DepositMade { .. }
        ));
        assert!(matches!(
            events.events[1].event,
            L1PortalEvent::WithdrawalBounceBack { .. }
        ));
    }

    #[test]
    fn rejects_unknown_topics_and_ignores_non_protocol_logs() {
        let unknown = log(alloy_primitives::LogData::new_unchecked(
            vec![B256::repeat_byte(0xff)],
            Default::default(),
        ));
        assert!(collect(&[receipt(true, B256::ZERO, vec![unknown])]).is_err());
        let mut wrong = deposit();
        wrong.inner.address = Address::repeat_byte(99);
        let events = collect(&[
            receipt(true, B256::ZERO, vec![wrong]),
            receipt(false, B256::ZERO, vec![deposit()]),
        ])
        .unwrap();
        assert!(events.events.is_empty());
    }

    #[test]
    fn reject_malformed_known_event() {
        let bad = log(alloy_primitives::LogData::new_unchecked(
            vec![ZonePortal::DepositMade::SIGNATURE_HASH],
            vec![0xde, 0xad].into(),
        ));
        assert!(
            collect(&[receipt(true, B256::ZERO, vec![bad])])
                .unwrap_err()
                .to_string()
                .contains("malformed DepositMade")
        );
    }

    #[test]
    fn reject_non_canonical_trailing_data() {
        let original = deposit();
        let mut bytes = original.inner.data.data.to_vec();
        bytes.extend([0; 32]);
        let bad = log(alloy_primitives::LogData::new_unchecked(
            original.inner.topics().to_vec(),
            bytes.into(),
        ));
        assert!(
            collect(&[receipt(true, B256::ZERO, vec![bad])])
                .unwrap_err()
                .to_string()
                .contains("non-canonical DepositMade")
        );
    }

    #[test]
    fn batch_submitted_preserves_no_queue_index() {
        let events = collect(&[receipt(true, B256::ZERO, vec![batch(U256::MAX)])]).unwrap();
        assert!(matches!(
            events.events[0].event,
            L1PortalEvent::BatchSubmitted {
                withdrawal_queue_index: U256::MAX,
                ..
            }
        ));
    }

    #[test]
    fn token_enabled_retains_metadata_and_provenance() {
        let receipt_hash = B256::repeat_byte(0x42);
        let canonical_hash = B256::repeat_byte(0x43);
        let receipt = receipt(true, receipt_hash, vec![token()]);
        let mut collector = EventCollector::new(PORTAL);
        collector
            .extract_receipt(0, canonical_hash, &receipt, BLOCK)
            .unwrap();
        let events = collector.finish();
        let evidence = &events.events[0];
        assert_eq!(evidence.transaction_hash, canonical_hash);
        assert_eq!(evidence.transaction_index, 0);
        assert_eq!(evidence.transaction_log_index, 0);
        assert_eq!(evidence.block_log_index, 0);
        assert_eq!(evidence.raw_log.address, PORTAL);
        assert!(
            matches!(&evidence.event, L1PortalEvent::TokenEnabled { token, name, symbol, currency } if *token == Address::repeat_byte(5) && name == "Test" && symbol == "TST" && currency == "USD")
        );
    }

    #[test]
    fn log_indices_are_correct() {
        let mut unknown = log(alloy_primitives::LogData::new_unchecked(
            vec![B256::repeat_byte(0xff)],
            Default::default(),
        ));
        unknown.inner.address = Address::repeat_byte(99);
        let events = collect(&[
            receipt(false, B256::ZERO, vec![unknown.clone()]),
            receipt(true, B256::ZERO, vec![unknown, deposit(), token()]),
        ])
        .unwrap();
        assert_eq!(
            (
                events.events[0].transaction_index,
                events.events[0].transaction_log_index,
                events.events[0].block_log_index
            ),
            (1, 1, 2)
        );
        assert_eq!(
            (
                events.events[1].transaction_index,
                events.events[1].transaction_log_index,
                events.events[1].block_log_index
            ),
            (1, 2, 3)
        );
    }
}
