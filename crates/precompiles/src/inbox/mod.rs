//! Native `ZoneInbox` precompile.
//!
//! The Inbox advances the Zone's finalized Tempo checkpoint and consumes the canonical L1 deposit
//! queue. The implementation shares one execution-local [`L1State`] with `TempoState` and the
//! Zone EVM database adapter: sequencer admission is checked at the parent checkpoint, then
//! `finalizeTempo` selects the child anchor used by every deposit, policy, and portal read.

#[cfg(test)]
mod tests;

use alloc::vec::Vec;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::SolValue;
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    EncodePrecompileResult, charge_input_cost, dispatch,
    error::TempoPrecompileError,
    storage::{Handler, Mapping, StorageCtx},
    tip20::{ITIP20, TIP20Token},
    view,
};
use tempo_precompiles_macros::contract;
use tempo_zone_contracts::{
    DecryptionData, Deposit, DepositType, EnabledToken, EncryptedDeposit, QueuedDeposit,
    ZoneInbox as ZoneInboxAbi, ZoneInboxError, ZoneInboxEvent,
};
use zone_primitives::constants::{
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, PORTAL_ENCRYPTION_KEYS_SLOT, PORTAL_SEQUENCER_SLOT,
    TEMPO_STATE_ADDRESS, ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS,
};

use crate::{
    AesGcmDecrypt, ChaumPedersenVerify, ZonePrecompileError, ZoneResult,
    ecies::{ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE, hkdf_info, hkdf_sha256},
    storage::{L1State, L1StorageReader},
    tempo_state::TempoState,
    tip20_factory::{ZoneTokenFactory, enableTokenCall},
};

/// The two native Outbox operations required while processing deposits.
///
/// Keeping this interface narrow lets Inbox and Outbox be developed independently. The native
/// Outbox implementation supplies the production implementation; tests use a recording fake.
pub trait InboxOutbox: Clone + 'static {
    /// Enqueue a failed-deposit withdrawal back to the Tempo recipient.
    fn enqueue_deposit_bounce_back(
        &mut self,
        token: Address,
        amount: u128,
        bounceback_recipient: Address,
    ) -> ZoneResult<()>;

    /// Resolve and consume the private fallback recipient for a withdrawal bounce-back.
    fn consume_fallback_recipient(&mut self, fallback_nonce: u64) -> ZoneResult<Address>;
}

/// Zone-side bridge Inbox state and deposit-processing logic.
#[contract(addr = ZONE_INBOX_ADDRESS)]
pub struct ZoneInbox {
    /// Hash-chain head after the last processed L1 deposit.
    processed_deposit_queue_hash: B256,
    /// Monotonic number of deposits consumed from the L1 queue.
    processed_deposit_number: u64,
    /// Withdrawal bounce-back mints that failed and can be claimed later.
    refunds: Mapping<Address, Mapping<Address, u128>>,
}

/// A queue entry whose nested ABI payload has been validated before execution begins.
enum DecodedQueuedDeposit {
    Regular(Deposit),
    Encrypted(EncryptedDeposit),
}

impl DecodedQueuedDeposit {
    fn hash_with_tail(&self, tail: B256) -> tempo_precompiles::Result<B256> {
        let encoded = match self {
            Self::Regular(deposit) => {
                (DepositType::Regular, deposit.clone(), tail).abi_encode_params()
            }
            Self::Encrypted(deposit) => {
                (DepositType::Encrypted, deposit.clone(), tail).abi_encode_params()
            }
        };
        StorageCtx::default().keccak256(&encoded)
    }
}

impl ZoneInbox {
    /// Initialize the precompile account marker without changing protocol storage.
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
    }

    /// Create the direct-call-only native Inbox precompile.
    pub fn create<P, O>(l1: L1State<P>, outbox: O, env: &crate::ZonePrecompileEnv) -> DynPrecompile
    where
        P: L1StorageReader,
        O: InboxOutbox,
    {
        crate::execution::create_precompile(
            "ZoneInbox",
            env,
            crate::execution::NoCallRules,
            move |data, caller| Self::new().call_with_l1_state(&l1, outbox.clone(), data, caller),
        )
    }

    /// Dispatch an Inbox ABI call using execution-local L1 state.
    pub(crate) fn call_with_l1_state<P, O>(
        &mut self,
        l1: &L1State<P>,
        mut outbox: O,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult
    where
        P: L1StorageReader,
        O: InboxOutbox,
    {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                ZoneInboxAbi::ZoneInboxCalls {
                    processedDepositQueueHash(call) => {
                        view(call, |_| self.processed_deposit_queue_hash.read())
                    },
                    processedDepositNumber(call) => {
                        view(call, |_| self.processed_deposit_number.read())
                    },
                    tempoPortal(call) => view(call, |_| Ok(l1.portal_address())),
                    tempoState(call) => view(call, |_| Ok(TEMPO_STATE_ADDRESS)),
                    config(call) => view(call, |_| Ok(ZONE_CONFIG_ADDRESS)),
                    refunds(call) => view(call, |call| {
                        self.refunds[call.token][call.owner].read()
                    }),
                    claimRefund(call) => crate::dispatch::mutate(call, msg_sender, |caller, call| {
                        self.claim_refund(caller, call.token)
                    }),
                    advanceTempo(call) => {
                        if self.storage.is_static() {
                            Ok(self.storage.revert_output(Bytes::new()))
                        } else {
                            self.advance_tempo(l1, &mut outbox, msg_sender, call)
                                .encode_precompile_result(0, 0, |()| Bytes::new())
                        }
                    },
                }
            },
        )
    }

    fn advance_tempo<P, O>(
        &mut self,
        l1: &L1State<P>,
        outbox: &mut O,
        caller: Address,
        call: ZoneInboxAbi::advanceTempoCall,
    ) -> ZoneResult<()>
    where
        P: L1StorageReader,
        O: InboxOutbox,
    {
        // Match Solidity ABI decoding: malformed nested payloads revert before any state or L1 read.
        let deposits = Self::decode_deposits(call.deposits)?;

        let mut tempo_state = TempoState::new();
        let previous_block_number = tempo_state.tempo_block_number()?;

        if !caller.is_zero() {
            let sequencer_word = l1.read_before_advance(
                l1.portal_address(),
                PORTAL_SEQUENCER_SLOT,
                previous_block_number,
            )?;
            let sequencer = Address::from_slice(&sequencer_word.as_slice()[12..]);
            if caller != sequencer {
                return Err(ZoneInboxError::only_sequencer().into());
            }
        }

        tempo_state.finalize_checkpoint(l1, call.header)?;
        self.enable_tokens(call.enabledTokens)?;

        let tempo_block_number = tempo_state.tempo_block_number()?;
        let tempo_block_hash = tempo_state.tempo_block_hash()?;
        let mut current_hash = self.processed_deposit_queue_hash.read()?;
        let mut decryptions = call.decryptions.into_iter();
        let deposit_count = deposits.len();

        for queued in deposits {
            current_hash = queued.hash_with_tail(current_hash)?;

            match queued {
                DecodedQueuedDeposit::Regular(deposit) => {
                    self.process_deposit(outbox, current_hash, deposit)
                }
                DecodedQueuedDeposit::Encrypted(deposit) => {
                    let Some(decryption) = decryptions.next() else {
                        return Err(ZoneInboxError::missing_decryption_data().into());
                    };
                    let key = self.read_encryption_key(l1, tempo_block_number, deposit.keyIndex)?;
                    self.process_deposit_encrypted(
                        outbox,
                        l1.portal_address(),
                        current_hash,
                        deposit,
                        decryption,
                        key,
                    )
                }
            }?;
        }

        if decryptions.next().is_some() {
            return Err(ZoneInboxError::extra_decryption_data().into());
        }

        let tempo_current_hash = l1.read_l1_storage(
            l1.portal_address(),
            PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            tempo_block_number,
        )?;
        if current_hash != tempo_current_hash {
            return Err(ZoneInboxError::invalid_deposit_queue_hash().into());
        }

        self.processed_deposit_queue_hash.write(current_hash)?;
        let previous_number = self.processed_deposit_number.read()?;
        let added =
            u64::try_from(deposit_count).map_err(|_| TempoPrecompileError::under_overflow())?;
        let processed_number = previous_number
            .checked_add(added)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        self.processed_deposit_number.write(processed_number)?;
        self.emit_event(ZoneInboxEvent::tempo_advanced(
            tempo_block_hash,
            tempo_block_number,
            U256::from(deposit_count),
            current_hash,
            processed_number,
        ))?;

        Ok(())
    }

    fn decode_deposits(deposits: Vec<QueuedDeposit>) -> ZoneResult<Vec<DecodedQueuedDeposit>> {
        deposits
            .into_iter()
            .map(|queued| {
                let decoded = match queued.depositType {
                    DepositType::Regular => {
                        Deposit::abi_decode(&queued.depositData).map(DecodedQueuedDeposit::Regular)
                    }
                    DepositType::Encrypted => EncryptedDeposit::abi_decode(&queued.depositData)
                        .map(DecodedQueuedDeposit::Encrypted),
                    _ => return Err(ZonePrecompileError::MalformedCalldata),
                };
                decoded.map_err(|_| ZonePrecompileError::MalformedCalldata)
            })
            .collect()
    }

    fn enable_tokens(&mut self, tokens: Vec<EnabledToken>) -> ZoneResult<()> {
        for token in tokens {
            ZoneTokenFactory::new().enable_token(enableTokenCall {
                token: token.token,
                name: token.name.clone(),
                symbol: token.symbol.clone(),
                currency: token.currency.clone(),
            })?;
            self.emit_event(ZoneInboxEvent::token_enabled(
                token.token,
                token.name,
                token.symbol,
                token.currency,
            ))?;
        }
        Ok(())
    }

    fn process_deposit<O: InboxOutbox>(
        &mut self,
        outbox: &mut O,
        current_hash: B256,
        deposit: Deposit,
    ) -> ZoneResult<()> {
        if deposit.bouncebackRecipient.is_zero() {
            return self.process_withdrawal_bounce_back(outbox, deposit);
        }

        if self.try_mint(deposit.token, deposit.to, deposit.amount)? {
            self.emit_event(ZoneInboxEvent::deposit_processed(
                current_hash,
                deposit.sender,
                deposit.to,
                deposit.token,
                deposit.amount,
                deposit.memo,
            ))?;
        } else {
            outbox.enqueue_deposit_bounce_back(
                deposit.token,
                deposit.amount,
                deposit.bouncebackRecipient,
            )?;
            self.emit_event(ZoneInboxEvent::deposit_failed(
                current_hash,
                deposit.sender,
                deposit.to,
                deposit.token,
                deposit.amount,
                deposit.bouncebackRecipient,
            ))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_deposit_encrypted<O: InboxOutbox>(
        &mut self,
        outbox: &mut O,
        portal: Address,
        current_hash: B256,
        deposit: EncryptedDeposit,
        decryption: DecryptionData,
        (sequencer_x, sequencer_y_parity): (B256, u8),
    ) -> ZoneResult<()> {
        ChaumPedersenVerify::charge_gas()?;
        let proof_valid = ChaumPedersenVerify::verify(
            &deposit.encrypted.ephemeralPubkeyX.0,
            deposit.encrypted.ephemeralPubkeyYParity,
            &decryption.sharedSecret.0,
            decryption.sharedSecretYParity,
            &sequencer_x.0,
            sequencer_y_parity,
            &decryption.cpProof.s.0,
            &decryption.cpProof.c.0,
        );

        let decrypted = if proof_valid {
            let info = hkdf_info(
                &portal,
                &deposit.keyIndex,
                &deposit.encrypted.ephemeralPubkeyX,
            );
            let key = hkdf_sha256(&decryption.sharedSecret.0, b"ecies-aes-key", &info);
            AesGcmDecrypt::charge_gas(deposit.encrypted.ciphertext.len(), 0)?;
            let (plaintext, valid) = AesGcmDecrypt::decrypt(
                &key,
                &deposit.encrypted.nonce.0,
                &deposit.encrypted.ciphertext,
                &[],
                &deposit.encrypted.tag.0,
            );
            valid
                .then_some(plaintext)
                .filter(|plaintext| plaintext.len() == ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE)
        } else {
            None
        };

        let Some(plaintext) = decrypted else {
            return self.fail_encrypted_deposit(outbox, current_hash, deposit);
        };
        let to = Address::from_slice(&plaintext[..20]);
        let memo = B256::from_slice(&plaintext[20..52]);

        if self.try_mint(deposit.token, to, deposit.amount)? {
            self.emit_event(ZoneInboxEvent::encrypted_deposit_processed(
                current_hash,
                deposit.sender,
                to,
                deposit.token,
                deposit.amount,
                memo,
            ))?;
            Ok(())
        } else {
            self.fail_encrypted_deposit(outbox, current_hash, deposit)
        }
    }

    fn read_encryption_key<P: L1StorageReader>(
        &self,
        l1: &L1State<P>,
        tempo_block_number: u64,
        key_index: U256,
    ) -> ZoneResult<(B256, u8)> {
        let base = U256::from_be_bytes(keccak256(PORTAL_ENCRYPTION_KEYS_SLOT.as_slice()).0);
        let offset = key_index
            .checked_mul(U256::from(2))
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        let slot_x = base
            .checked_add(offset)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        let slot_meta = slot_x
            .checked_add(U256::ONE)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        let portal = l1.portal_address();
        let x = l1.read_l1_storage(portal, B256::from(slot_x.to_be_bytes()), tempo_block_number)?;
        if x.is_zero() {
            return Err(ZoneInboxError::invalid_shared_secret_proof().into());
        }
        let meta = l1.read_l1_storage(
            portal,
            B256::from(slot_meta.to_be_bytes()),
            tempo_block_number,
        )?;
        Ok((x, meta.as_slice()[31]))
    }

    fn fail_encrypted_deposit<O: InboxOutbox>(
        &mut self,
        outbox: &mut O,
        current_hash: B256,
        deposit: EncryptedDeposit,
    ) -> ZoneResult<()> {
        outbox.enqueue_deposit_bounce_back(
            deposit.token,
            deposit.amount,
            deposit.bouncebackRecipient,
        )?;
        self.emit_event(ZoneInboxEvent::encrypted_deposit_failed(
            current_hash,
            deposit.sender,
            deposit.token,
            deposit.amount,
        ))?;
        Ok(())
    }

    fn process_withdrawal_bounce_back<O: InboxOutbox>(
        &mut self,
        outbox: &mut O,
        deposit: Deposit,
    ) -> ZoneResult<()> {
        let fallback_nonce = u64::from_be_bytes(
            deposit.to.as_slice()[12..]
                .try_into()
                .expect("address suffix is eight bytes"),
        );
        let recipient = outbox.consume_fallback_recipient(fallback_nonce)?;
        if self.try_mint(deposit.token, recipient, deposit.amount)? {
            self.emit_event(ZoneInboxEvent::withdrawal_bounce_back_processed(
                recipient,
                deposit.token,
                deposit.amount,
            ))?;
        } else {
            let previous = self.refunds[deposit.token][recipient].read()?;
            let Some(refund) = previous.checked_add(deposit.amount) else {
                return Err(TempoPrecompileError::under_overflow().into());
            };
            self.refunds[deposit.token][recipient].write(refund)?;
            self.emit_event(ZoneInboxEvent::withdrawal_bounce_back_pending(
                recipient,
                deposit.token,
                deposit.amount,
            ))?;
        }
        Ok(())
    }

    /// Mint with Solidity `try/catch` semantics: ordinary reverts are caught while fatal and
    /// out-of-gas failures abort the outer Inbox call.
    fn try_mint(&mut self, token: Address, to: Address, amount: u128) -> ZoneResult<bool> {
        let checkpoint = self.storage.checkpoint();
        let result = TIP20Token::from_address(token).and_then(|mut token| {
            token.mint(
                ZONE_INBOX_ADDRESS,
                ITIP20::mintCall {
                    to,
                    amount: U256::from(amount),
                },
            )
        });
        match result {
            Ok(()) => {
                checkpoint.commit();
                Ok(true)
            }
            Err(TempoPrecompileError::Fatal(message)) => {
                drop(checkpoint);
                Err(TempoPrecompileError::Fatal(message).into())
            }
            Err(TempoPrecompileError::OutOfGas) => {
                drop(checkpoint);
                Err(TempoPrecompileError::OutOfGas.into())
            }
            Err(_) => {
                drop(checkpoint);
                Ok(false)
            }
        }
    }

    fn claim_refund(&mut self, caller: Address, token: Address) -> ZoneResult<u128> {
        let amount = self.refunds[token][caller].read()?;
        self.refunds[token][caller].write(0)?;
        TIP20Token::from_address(token)?.mint(
            ZONE_INBOX_ADDRESS,
            ITIP20::mintCall {
                to: caller,
                amount: U256::from(amount),
            },
        )?;
        self.emit_event(ZoneInboxEvent::refund_claimed(caller, token, amount))?;
        Ok(amount)
    }
}
