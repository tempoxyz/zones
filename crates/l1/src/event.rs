use super::*;

/// Events extracted from the ZonePortal in a single L1 block.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct L1PortalEvents {
    /// Deposit events (regular + encrypted).
    pub deposits: Vec<L1Deposit>,
    /// Tokens newly enabled for bridging in this block, with metadata.
    pub enabled_tokens: Vec<EnabledToken>,
}

/// A token newly enabled for bridging, with metadata for L2 creation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnabledToken {
    /// The L1 token address (TIP-20 with 0x20C0 prefix).
    pub token: Address,
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Token currency (e.g. "USD", "EUR").
    pub currency: String,
}

impl EnabledToken {
    /// Convert to the ABI type used in `advanceTempo` calldata.
    pub fn to_abi(&self) -> abi::EnabledToken {
        abi::EnabledToken {
            token: self.token,
            name: self.name.clone(),
            symbol: self.symbol.clone(),
            currency: self.currency.clone(),
        }
    }
}

impl L1PortalEvents {
    /// Event signature hashes that this container knows how to decode.
    const SIGNATURE_HASHES: [B256; 4] = [
        DepositMade::SIGNATURE_HASH,
        EncryptedDepositMade::SIGNATURE_HASH,
        WithdrawalBounceBack::SIGNATURE_HASH,
        TokenEnabled::SIGNATURE_HASH,
    ];

    /// Create portal events from deposits only.
    pub fn from_deposits(deposits: Vec<L1Deposit>) -> Self {
        Self {
            deposits,
            ..Default::default()
        }
    }

    /// Validate that `advanceTempo` processes every deposit and token enable observed in this
    /// block's verified L1 receipts, in canonical log order.
    ///
    /// The sequencer-controlled `rejected` flag is deliberately excluded from deposit identity.
    pub fn validate_advance_tempo_inputs(
        &self,
        deposits: &[abi::QueuedDeposit],
        enabled_tokens: &[abi::EnabledToken],
    ) -> eyre::Result<()> {
        eyre::ensure!(
            deposits.len() == self.deposits.len(),
            "advanceTempo deposit count does not match observed L1 events: expected {}, got {}",
            self.deposits.len(),
            deposits.len()
        );

        for (index, (expected, actual)) in self.deposits.iter().zip(deposits).enumerate() {
            let expected = expected.to_abi_queued_deposit();
            eyre::ensure!(
                actual.depositType == expected.depositType
                    && actual.depositData == expected.depositData,
                "advanceTempo deposit {index} does not match the observed L1 event"
            );
        }

        let expected_tokens: Vec<_> = self
            .enabled_tokens
            .iter()
            .map(EnabledToken::to_abi)
            .collect();
        eyre::ensure!(
            enabled_tokens == expected_tokens,
            "advanceTempo token enables do not match observed L1 events"
        );
        Ok(())
    }

    /// Decode a portal log and add the event to this container.
    ///
    /// Logs whose topic0 does not match a known portal event are skipped.
    /// Known events that fail to decode return an error.
    pub fn push_log(&mut self, log: &Log, block_number: u64) -> eyre::Result<()> {
        if !Self::is_known_event(log) {
            debug!(
                l1_block = block_number,
                topic0 = ?log.topic0(),
                "Skipping unknown portal event"
            );
            return Ok(());
        }
        match ZonePortalEvents::decode_log(&log.inner)?.data {
            ZonePortalEvents::DepositMade(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    sender = %event.sender,
                    to = %event.to,
                    amount = %event.netAmount,
                    "💰 Deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Regular(Deposit::from_event(event)));
            }
            ZonePortalEvents::EncryptedDepositMade(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    sender = %event.sender,
                    amount = %event.netAmount,
                    "🔒 Encrypted deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Encrypted(EncryptedDeposit::from_event(event)));
            }
            ZonePortalEvents::WithdrawalBounceBack(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    fallback_nonce = event.fallbackNonce,
                    amount = %event.amount,
                    "↩️ Bounce-back deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Regular(Deposit::from_bounce_back(
                        event,
                        log.address(),
                    )));
            }
            ZonePortalEvents::TokenEnabled(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    name = %event.name,
                    symbol = %event.symbol,
                    currency = %event.currency,
                    "🪙 Token enabled on L1"
                );
                self.enabled_tokens.push(EnabledToken {
                    token: event.token,
                    name: event.name,
                    symbol: event.symbol,
                    currency: event.currency,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn is_known_event(log: &Log) -> bool {
        log.topic0()
            .is_some_and(|t| Self::SIGNATURE_HASHES.contains(t))
    }
}
