//! Provider-backed C6 operator replenishment.
//!
//! This module owns economic-action construction and strict mapping of canonical provider
//! observations into the durable replenishment driver. Providers must authenticate observations
//! at finalized L1 or committed Zone heads; submission acceptance is never success.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_network::{ReceiptResponse as _, TransactionBuilder as _, TransactionResponse as _};
use alloy_primitives::{Address, B256, Bytes, FixedBytes, Sealable as _, TxKind, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, TransactionRequest};
use alloy_sol_types::{SolCall, SolEvent};
use serde::{Deserialize, Serialize};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionRequest};
use tempo_contracts::precompiles::{ITIP20, ITIP403Registry, TIP403_REGISTRY_ADDRESS};
use tempo_zone_contracts::{
    BatchSubmitted, DepositPayload, FAST_TRANSFER_ADDRESS, IFastTransfer, IZoneOutbox,
    LegacyBatchSubmitted, ZonePortal,
};
use zone_fast_transfer::{
    DepositRecord, LegObservation, ReplenishmentBridge, ReplenishmentJob, ReplenishmentWorkerError,
    WithdrawalRecord, WorkerFuture,
    replenishment::{TokenAmount, TransactionCost},
};
use zone_precompiles::ecies::encrypt_deposit;
use zone_primitives::constants::ZONE_OUTBOX_ADDRESS;

const MAX_REPLENISHMENT_CONTRIBUTIONS: usize = 256;
const MAX_RECONCILIATION_LOGS: usize = 4096;

/// Heap-owned provider future; implementations own real signer/read providers.
pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ReplenishmentProviderError>> + Send + 'a>>;

/// Owned live providers runtime assembles for one route. The source and Tempo providers include
/// the operator signers; the destination provider is a committed-state reader.
#[derive(Clone)]
pub struct AlloyReplenishmentProviderHandles {
    pub source_zone: DynProvider<TempoNetwork>,
    pub tempo_l1_treasury: DynProvider<TempoNetwork>,
    pub destination_zone: DynProvider<TempoNetwork>,
    pub source_signing: ReplenishmentSigningConfig,
    pub treasury_signing: ReplenishmentSigningConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplenishmentSigningConfig {
    pub sender: Address,
    pub effective_fee_payer: Address,
    pub fee_token: Address,
}

impl AlloyReplenishmentProviderHandles {
    pub fn new(
        source_zone: DynProvider<TempoNetwork>,
        tempo_l1_treasury: DynProvider<TempoNetwork>,
        destination_zone: DynProvider<TempoNetwork>,
        source_signing: ReplenishmentSigningConfig,
        treasury_signing: ReplenishmentSigningConfig,
    ) -> Self {
        Self {
            source_zone,
            tempo_l1_treasury,
            destination_zone,
            source_signing,
            treasury_signing,
        }
    }
}

/// Candidate route. It cannot be used by a bridge until the provider validates it at finalized
/// L1 and returns a [`FinalizedRouteSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplenishmentRouteConfig {
    pub l1_chain_id: u64,
    pub source_chain_id: u64,
    pub destination_chain_id: u64,
    pub protocol_version: u32,
    pub source_portal: Address,
    pub destination_portal: Address,
    pub source_fast_transfer: Address,
    pub destination_fast_transfer: Address,
    pub l1_token: Address,
    pub source_token: Address,
    pub destination_token: Address,
    pub source_inventory: Address,
    pub source_fallback: Address,
    pub source_fee_payer: Address,
    pub treasury_fee_payer: Address,
    pub source_fee_token: Address,
    pub treasury_fee_token: Address,
    pub minimum_source_fee_reserve: U256,
    pub minimum_treasury_fee_reserve: U256,
    pub maximum_replenishment_amount: U256,
    pub maximum_source_withdrawal_fee: U256,
    pub maximum_destination_deposit_fee: U256,
    pub treasury: Address,
    pub destination_pool_operator: Address,
}

impl ReplenishmentRouteConfig {
    fn validate_local(&self) -> Result<(), ReplenishmentProviderError> {
        let addresses = [
            self.source_portal,
            self.destination_portal,
            self.source_fast_transfer,
            self.destination_fast_transfer,
            self.l1_token,
            self.source_token,
            self.destination_token,
            self.source_inventory,
            self.source_fallback,
            self.source_fee_payer,
            self.treasury_fee_payer,
            self.source_fee_token,
            self.treasury_fee_token,
            self.treasury,
            self.destination_pool_operator,
        ];
        if self.l1_chain_id == 0
            || self.source_chain_id == 0
            || self.destination_chain_id == 0
            || self.protocol_version == 0
            || self.maximum_replenishment_amount.is_zero()
            || addresses.iter().any(|address| address.is_zero())
            || self.source_portal == self.destination_portal
            || self.source_chain_id == self.destination_chain_id
            || self.source_token != self.l1_token
            || self.destination_token != self.l1_token
            || self.source_inventory != self.source_fallback
            || self.source_fast_transfer != FAST_TRANSFER_ADDRESS
            || self.destination_fast_transfer != FAST_TRANSFER_ADDRESS
        {
            return Err(ReplenishmentProviderError::InvalidRoute);
        }
        Ok(())
    }
}

/// Authoritative route facts read together at one finalized Tempo L1 block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedRouteSnapshot {
    pub l1_block_number: u64,
    pub l1_block_hash: B256,
    pub source_block_number: u64,
    pub destination_block_number: u64,
    pub l1_chain_id: u64,
    pub source_chain_id: u64,
    pub destination_chain_id: u64,
    pub source_portal: Address,
    pub destination_portal: Address,
    pub l1_token: Address,
    pub source_token: Address,
    pub destination_token: Address,
    pub source_inventory: Address,
    pub source_fallback: Address,
    pub treasury: Address,
    pub destination_pool_operator: Address,
    pub source_fast_epoch: u64,
    pub destination_fast_epoch: u64,
    pub source_protocol_version: u32,
    pub destination_protocol_version: u32,
    pub source_epoch_open: bool,
    pub destination_epoch_open: bool,
    pub l1_token_decimals: u8,
    pub source_token_decimals: u8,
    pub destination_token_decimals: u8,
    pub destination_key_index: U256,
    pub destination_key_x: B256,
    pub destination_key_y_parity: u8,
    pub source_inventory_allowed: bool,
    pub source_fallback_allowed: bool,
    pub source_fee_payer_allowed: bool,
    pub treasury_allowed_on_source: bool,
    pub treasury_allowed_on_destination: bool,
    pub source_token_enabled: bool,
    pub destination_token_enabled: bool,
    pub destination_deposits_active: bool,
    pub source_outbox_allowance: U256,
    pub source_fee_payer_outbox_allowance: U256,
    pub destination_portal_allowance: U256,
    pub source_fee_reserve: U256,
    pub treasury_fee_reserve: U256,
    pub destination_pool_initialized: bool,
    pub destination_replenishment_route_configured: bool,
}

impl FinalizedRouteSnapshot {
    fn validate_against(
        &self,
        route: &ReplenishmentRouteConfig,
    ) -> Result<(), ReplenishmentProviderError> {
        let combined_source_allowance = route
            .maximum_replenishment_amount
            .checked_add(route.maximum_source_withdrawal_fee)
            .ok_or(ReplenishmentProviderError::InvalidRoute)?;
        if self.l1_block_hash.is_zero()
            || self.l1_chain_id != route.l1_chain_id
            || self.source_chain_id != route.source_chain_id
            || self.destination_chain_id != route.destination_chain_id
            || self.source_portal != route.source_portal
            || self.destination_portal != route.destination_portal
            || self.l1_token != route.l1_token
            || self.source_token != route.source_token
            || self.destination_token != route.destination_token
            || self.source_inventory != route.source_inventory
            || self.source_fallback != route.source_fallback
            || self.treasury != route.treasury
            || self.destination_pool_operator != route.destination_pool_operator
            || self.source_fast_epoch == 0
            || self.destination_fast_epoch == 0
            || self.source_protocol_version != route.protocol_version
            || self.destination_protocol_version != route.protocol_version
            || !self.source_epoch_open
            || !self.destination_epoch_open
            || self.l1_token_decimals != self.source_token_decimals
            || self.l1_token_decimals != self.destination_token_decimals
            || self.destination_key_x.is_zero()
            || !matches!(self.destination_key_y_parity, 2 | 3)
            || !self.source_inventory_allowed
            || !self.source_fallback_allowed
            || !self.source_fee_payer_allowed
            || !self.treasury_allowed_on_source
            || !self.treasury_allowed_on_destination
            || !self.source_token_enabled
            || !self.destination_token_enabled
            || !self.destination_deposits_active
            || self.source_outbox_allowance < route.maximum_replenishment_amount
            || self.source_fee_payer_outbox_allowance < route.maximum_source_withdrawal_fee
            || (route.source_inventory == route.source_fee_payer
                && self.source_outbox_allowance < combined_source_allowance)
            || self.destination_portal_allowance < route.maximum_replenishment_amount
            || self.source_fee_reserve < route.minimum_source_fee_reserve
            || self.treasury_fee_reserve < route.minimum_treasury_fee_reserve
            || !self.destination_pool_initialized
            || !self.destination_replenishment_route_configured
        {
            return Err(ReplenishmentProviderError::InvalidFinalizedRegistry);
        }
        Ok(())
    }
}

/// Complete source action retained before any RPC submission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedWithdrawalAction {
    pub job_id: B256,
    pub transfer_ids: Vec<B256>,
    pub token: Address,
    pub treasury: Address,
    pub gross_amount: U256,
    pub not_before_source_block: u64,
    pub not_before_l1_block: u64,
    pub signer_nonce: u64,
    pub fee_payer: Address,
    pub fee_token: Address,
    pub calldata: Bytes,
    pub native_job_intent_hash: B256,
    pub intent_hash: B256,
}

/// Complete randomized encrypted-deposit action retained before any RPC submission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedDepositAction {
    pub job_id: B256,
    pub portal: Address,
    pub token: Address,
    pub amount: u128,
    pub not_before_l1_block: u64,
    pub not_before_destination_block: u64,
    pub key_index: U256,
    pub encrypted: PersistedDepositPayload,
    pub pool_recipient: Address,
    pub refund_recipient: Address,
    pub signer_nonce: u64,
    pub fee_payer: Address,
    pub fee_token: Address,
    pub calldata: Bytes,
    pub intent_hash: B256,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedRefundClaimAction {
    pub job_id: B256,
    pub portal: Address,
    pub token: Address,
    pub expected_amount: u128,
    pub not_before_l1_block: u64,
    pub signer_nonce: u64,
    pub fee_payer: Address,
    pub fee_token: Address,
    pub calldata: Bytes,
    pub intent_hash: B256,
}

/// Persisted prepared action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreparedReplenishmentAction {
    Withdrawal(PreparedWithdrawalAction),
    Deposit(PreparedDepositAction),
    RefundClaim(PreparedRefundClaimAction),
}

/// Serializable form of the exact randomized Portal deposit payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedDepositPayload {
    pub ephemeral_pubkey_x: B256,
    pub ephemeral_pubkey_y_parity: u8,
    pub ciphertext: Bytes,
    pub nonce: FixedBytes<12>,
    pub tag: FixedBytes<16>,
}

impl PersistedDepositPayload {
    fn as_abi(&self) -> DepositPayload {
        DepositPayload {
            ephemeralPubkeyX: self.ephemeral_pubkey_x,
            ephemeralPubkeyYParity: self.ephemeral_pubkey_y_parity,
            ciphertext: self.ciphertext.clone(),
            nonce: self.nonce,
            tag: self.tag,
        }
    }
}

/// Durable action bytes independent of the job image codec.
pub trait PreparedActionStore: Send + Sync {
    /// Fsync an immutable action. Repeating identical bytes is a no-op; the same intent hash with
    /// different bytes is an error.
    fn put(&self, action: PreparedReplenishmentAction) -> Result<(), ReplenishmentProviderError>;

    /// Load the exact bytes required to reconstruct a same-nonce replacement after restart.
    fn get(
        &self,
        intent_hash: B256,
    ) -> Result<PreparedReplenishmentAction, ReplenishmentProviderError>;
}

/// Fsynced immutable action files keyed by transaction-intent hash.
pub struct FilePreparedActionStore {
    directory: PathBuf,
    writer: Mutex<()>,
}

impl FilePreparedActionStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, ReplenishmentProviderError> {
        fs::create_dir_all(directory.as_ref()).map_err(storage_error)?;
        sync_directory(directory.as_ref())?;
        Ok(Self {
            directory: directory.as_ref().to_path_buf(),
            writer: Mutex::new(()),
        })
    }

    fn path(&self, intent_hash: B256) -> PathBuf {
        self.directory.join(format!("{intent_hash}.json"))
    }
}

impl PreparedActionStore for FilePreparedActionStore {
    fn put(&self, action: PreparedReplenishmentAction) -> Result<(), ReplenishmentProviderError> {
        let _guard = self
            .writer
            .lock()
            .map_err(|_| ReplenishmentProviderError::Storage("action store poisoned".into()))?;
        let intent_hash = action.intent_hash();
        let bytes = serde_json::to_vec(&action).map_err(storage_error)?;
        let path = self.path(intent_hash);
        if path.exists() {
            return require_same_action(&path, &action);
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(storage_error)?
            .as_nanos();
        let temporary = self.directory.join(format!(
            ".{intent_hash}.{}.{}.tmp",
            std::process::id(),
            unique
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(storage_error)?;
        file.write_all(&bytes).map_err(storage_error)?;
        file.sync_all().map_err(storage_error)?;
        drop(file);

        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                fs::remove_file(&temporary).map_err(storage_error)?;
                sync_directory(&self.directory)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary).map_err(storage_error)?;
                require_same_action(&path, &action)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(storage_error(error))
            }
        }
    }

    fn get(
        &self,
        intent_hash: B256,
    ) -> Result<PreparedReplenishmentAction, ReplenishmentProviderError> {
        let bytes = fs::read(self.path(intent_hash)).map_err(storage_error)?;
        let action =
            serde_json::from_slice::<PreparedReplenishmentAction>(&bytes).map_err(storage_error)?;
        if action.intent_hash() != intent_hash {
            return Err(ReplenishmentProviderError::PreparedActionMismatch);
        }
        Ok(action)
    }
}

impl PreparedReplenishmentAction {
    fn intent_hash(&self) -> B256 {
        match self {
            Self::Withdrawal(action) => action.intent_hash,
            Self::Deposit(action) => action.intent_hash,
            Self::RefundClaim(action) => action.intent_hash,
        }
    }
}

/// Durable reservations for replacement-safe signer and Outbox nonces.
pub trait ReplenishmentNoncePlanner: Send + Sync {
    /// Durably reserve or return the existing source signer nonce for this permanent job.
    fn source_signer_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError>;
    /// Durably reserve or return the existing Tempo treasury signer nonce.
    fn treasury_signer_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError>;
    /// Durably reserve the distinct Tempo treasury nonce used only if a failed deposit has to
    /// claim an aggregated Portal refund.
    fn treasury_refund_claim_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError>;
    /// Read an existing claim reservation without creating a nonce gap for direct refunds.
    fn treasury_refund_claim_nonce_if_reserved(
        &self,
        job_id: B256,
    ) -> Result<Option<u64>, ReplenishmentProviderError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct NoncePlannerState {
    source_signer: Address,
    treasury_signer: Address,
    next_source_nonce: u64,
    next_treasury_nonce: u64,
    source_jobs: BTreeMap<B256, u64>,
    treasury_deposit_jobs: BTreeMap<B256, u64>,
    treasury_refund_jobs: BTreeMap<B256, u64>,
}

/// Fsynced nonce reservations. A new file fetches each provider's committed account nonce once;
/// every subsequent open and every retry is served exclusively from the durable reservations.
pub struct FileReplenishmentNoncePlanner {
    path: PathBuf,
    state: Mutex<NoncePlannerState>,
}

impl FileReplenishmentNoncePlanner {
    pub async fn open(
        path: impl AsRef<Path>,
        source_provider: &DynProvider<TempoNetwork>,
        source_signer: Address,
        treasury_provider: &DynProvider<TempoNetwork>,
        treasury_signer: Address,
    ) -> Result<Self, ReplenishmentProviderError> {
        if source_signer.is_zero() || treasury_signer.is_zero() {
            return Err(ReplenishmentProviderError::InvalidRoute);
        }
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(storage_error)?;
        }
        if path.exists() {
            let state: NoncePlannerState =
                serde_json::from_slice(&fs::read(&path).map_err(storage_error)?)
                    .map_err(storage_error)?;
            if state.source_signer != source_signer || state.treasury_signer != treasury_signer {
                return Err(ReplenishmentProviderError::PreparedActionMismatch);
            }
            validate_nonce_state(&state)?;
            return Ok(Self {
                path,
                state: Mutex::new(state),
            });
        }

        // These are deliberately the only provider nonce reads in initialization. Persisting the
        // complete two-lane state before returning prevents a restart from refetching one lane.
        let source_nonce = source_provider
            .get_transaction_count(source_signer)
            .await
            .map_err(provider_error)?;
        let treasury_nonce = treasury_provider
            .get_transaction_count(treasury_signer)
            .await
            .map_err(provider_error)?;
        let state = NoncePlannerState {
            source_signer,
            treasury_signer,
            next_source_nonce: source_nonce,
            next_treasury_nonce: treasury_nonce,
            source_jobs: BTreeMap::new(),
            treasury_deposit_jobs: BTreeMap::new(),
            treasury_refund_jobs: BTreeMap::new(),
        };
        persist_nonce_state(&path, &state, true)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn reserve(&self, lane: NonceLane, job_id: B256) -> Result<u64, ReplenishmentProviderError> {
        if job_id.is_zero() {
            return Err(ReplenishmentProviderError::PreparedActionMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReplenishmentProviderError::Storage("nonce planner poisoned".into()))?;
        let existing = match lane {
            NonceLane::Source => state.source_jobs.get(&job_id),
            NonceLane::TreasuryDeposit => state.treasury_deposit_jobs.get(&job_id),
            NonceLane::TreasuryRefund => state.treasury_refund_jobs.get(&job_id),
        };
        if let Some(nonce) = existing {
            return Ok(*nonce);
        }
        let next = match lane {
            NonceLane::Source => &mut state.next_source_nonce,
            NonceLane::TreasuryDeposit | NonceLane::TreasuryRefund => {
                &mut state.next_treasury_nonce
            }
        };
        let nonce = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| ReplenishmentProviderError::Storage("nonce overflow".into()))?;
        match lane {
            NonceLane::Source => state.source_jobs.insert(job_id, nonce),
            NonceLane::TreasuryDeposit => state.treasury_deposit_jobs.insert(job_id, nonce),
            NonceLane::TreasuryRefund => state.treasury_refund_jobs.insert(job_id, nonce),
        };
        persist_nonce_state(&self.path, &state, false)?;
        Ok(nonce)
    }
}

#[derive(Clone, Copy)]
enum NonceLane {
    Source,
    TreasuryDeposit,
    TreasuryRefund,
}

impl ReplenishmentNoncePlanner for FileReplenishmentNoncePlanner {
    fn source_signer_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError> {
        self.reserve(NonceLane::Source, job_id)
    }

    fn treasury_signer_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError> {
        self.reserve(NonceLane::TreasuryDeposit, job_id)
    }

    fn treasury_refund_claim_nonce(&self, job_id: B256) -> Result<u64, ReplenishmentProviderError> {
        self.reserve(NonceLane::TreasuryRefund, job_id)
    }

    fn treasury_refund_claim_nonce_if_reserved(
        &self,
        job_id: B256,
    ) -> Result<Option<u64>, ReplenishmentProviderError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ReplenishmentProviderError::Storage("nonce planner poisoned".into()))?;
        Ok(state.treasury_refund_jobs.get(&job_id).copied())
    }
}

/// Canonical source-leg result. Implementations derive this from the atomic native receipt,
/// accepted-batch event, sequential Portal withdrawal outcome, and real token effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalWithdrawalObservation {
    Pending {
        submission_hash: Option<B256>,
        canonical_transaction_hash: Option<B256>,
        fallback_nonce: Option<u64>,
        withdrawal_index: Option<u64>,
        sender_tag: Option<B256>,
        source_transaction_cost: Option<TransactionCost>,
        withdrawal_fee: Option<TokenAmount>,
        queue_index: Option<u64>,
        accepted_batch_hash: Option<B256>,
    },
    TreasuryCredited {
        transaction_hash: B256,
        queue_index: u64,
        accepted_batch_hash: B256,
        amount: U256,
        fallback_nonce: u64,
        withdrawal_index: u64,
        sender_tag: B256,
        source_transaction_cost: TransactionCost,
        withdrawal_fee: TokenAmount,
        l1_transaction_cost: TransactionCost,
    },
    Bounced {
        transaction_hash: B256,
        queue_index: u64,
        accepted_batch_hash: B256,
        fallback_nonce: u64,
        withdrawal_index: u64,
        sender_tag: B256,
        source_transaction_cost: TransactionCost,
        withdrawal_fee: TokenAmount,
        l1_transaction_cost: TransactionCost,
    },
}

/// Canonical destination-leg result. `PoolCredited` requires the committed Inbox receipt and
/// native `ReplenishmentCredited` state transition, not only `DepositMade` on L1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalDepositObservation {
    Pending {
        submission_hash: Option<B256>,
        canonical_transaction_hash: Option<B256>,
        transaction_cost: Option<TransactionCost>,
        deposit_fee: Option<TokenAmount>,
        deposit_number: Option<u64>,
    },
    PoolCredited {
        transaction_hash: B256,
        deposit_number: u64,
        amount: U256,
        transaction_cost: TransactionCost,
        deposit_fee: TokenAmount,
    },
    RefundPending {
        transaction_hash: B256,
        deposit_number: u64,
        transaction_cost: TransactionCost,
        deposit_fee: TokenAmount,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalRefundObservation {
    Pending,
    Refunded(U256),
    ClaimRequired(U256),
}

/// Owned provider handles used by the bridge. A production implementation holds the source Zone
/// signer/provider, Tempo treasury signer/provider, and destination committed-state provider.
pub trait ReplenishmentProvider: Send + Sync {
    /// Read all returned facts from one finalized L1 anchor plus committed Zone state. This must
    /// verify roles, policies, allowances, fee reserves, token mappings/decimals, pool ownership,
    /// current open epochs, and the destination Portal encryption key.
    fn validate_route<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
    ) -> ProviderFuture<'a, FinalizedRouteSnapshot>;

    /// Reconcile known same-nonce hashes before broadcasting. A timeout returns `Pending`; any
    /// replacement reuses `action.signer_nonce` and byte-identical economic calldata. Before
    /// following the L1 leg, verify the atomic receipt's job ID, derived gross amount, fallback
    /// nonce, withdrawal index, token, treasury, and contribution IDs against `action`.
    fn submit_or_observe_withdrawal<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedWithdrawalAction,
    ) -> ProviderFuture<'a, CanonicalWithdrawalObservation>;

    /// Return an amount only from the committed native `InventoryRestored` transition.
    fn observe_inventory_restoration<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedWithdrawalAction,
    ) -> ProviderFuture<'a, Option<U256>>;

    /// Reconcile the L1 deposit queue and destination committed native pool receipt. L1 RPC
    /// acceptance or `DepositMade` alone returns `Pending`.
    fn submit_or_observe_deposit<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedDepositAction,
    ) -> ProviderFuture<'a, CanonicalDepositObservation>;

    /// Reconcile direct refund or claimable refund and claim transaction before returning funds.
    fn observe_treasury_refund<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedDepositAction,
    ) -> ProviderFuture<'a, CanonicalRefundObservation>;

    fn submit_or_observe_refund_claim<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedRefundClaimAction,
    ) -> ProviderFuture<'a, Option<U256>>;
}

impl ReplenishmentProvider for AlloyReplenishmentProviderHandles {
    fn validate_route<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
    ) -> ProviderFuture<'a, FinalizedRouteSnapshot> {
        Box::pin(async move {
            route.validate_local()?;
            if self.source_signing
                != (ReplenishmentSigningConfig {
                    sender: route.source_inventory,
                    effective_fee_payer: route.source_fee_payer,
                    fee_token: route.source_fee_token,
                })
                || self.treasury_signing
                    != (ReplenishmentSigningConfig {
                        sender: route.treasury,
                        effective_fee_payer: route.treasury_fee_payer,
                        fee_token: route.treasury_fee_token,
                    })
            {
                return Err(ReplenishmentProviderError::MissingSignerConfiguration);
            }
            let l1_header = self
                .tempo_l1_treasury
                .get_header_by_number(BlockNumberOrTag::Finalized)
                .await
                .map_err(provider_error)?
                .ok_or(ReplenishmentProviderError::FinalizedStateUnavailable)?;
            let l1_block_number = l1_header.number();
            let l1_block_hash = l1_header.hash_slow();
            let l1_block = BlockId::hash_canonical(l1_block_hash);
            let source_block_number = self
                .source_zone
                .get_block_number()
                .await
                .map_err(provider_error)?;
            let destination_block_number = self
                .destination_zone
                .get_block_number()
                .await
                .map_err(provider_error)?;
            let source_block = BlockId::number(source_block_number);
            let destination_block = BlockId::number(destination_block_number);

            let l1_chain_id = self
                .tempo_l1_treasury
                .get_chain_id()
                .await
                .map_err(provider_error)?;
            let source_chain_id = self
                .source_zone
                .get_chain_id()
                .await
                .map_err(provider_error)?;
            let destination_chain_id = self
                .destination_zone
                .get_chain_id()
                .await
                .map_err(provider_error)?;
            if l1_chain_id != route.l1_chain_id
                || source_chain_id != route.source_chain_id
                || destination_chain_id != route.destination_chain_id
            {
                return Err(ReplenishmentProviderError::InvalidFinalizedRegistry);
            }

            let source_portal = ZonePortal::new(route.source_portal, &self.tempo_l1_treasury);
            let destination_portal =
                ZonePortal::new(route.destination_portal, &self.tempo_l1_treasury);
            let source_fast_epoch = source_portal
                .fastEpoch()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_fast_epoch = destination_portal
                .fastEpoch()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_epoch = source_portal
                .fastEpochConfig(source_fast_epoch)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_fast_active = source_portal
                .fastEpochActive()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_fast_active = destination_portal
                .fastEpochActive()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_portal_paused = source_portal
                .paused()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_portal_paused = destination_portal
                .paused()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_epoch = destination_portal
                .fastEpochConfig(destination_fast_epoch)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_token_config = source_portal
                .tokenConfig(route.l1_token)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_token_config = destination_portal
                .tokenConfig(route.l1_token)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_key = destination_portal
                .encryptionKeyAtBlock(l1_block_number)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let key_valid = destination_portal
                .isEncryptionKeyValid(destination_key.keyIndex)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;

            let source_access_enforced = source_portal
                .isAccessEnforced()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_access_enforced = destination_portal
                .isAccessEnforced()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_inventory_allowed = account_allowed(
                &self.tempo_l1_treasury,
                route.source_portal,
                source_access_enforced,
                route.source_inventory,
                l1_block,
            )
            .await?;
            let source_fallback_allowed = account_allowed(
                &self.tempo_l1_treasury,
                route.source_portal,
                source_access_enforced,
                route.source_fallback,
                l1_block,
            )
            .await?;
            let source_fee_payer_allowed = account_allowed(
                &self.tempo_l1_treasury,
                route.source_portal,
                source_access_enforced,
                route.source_fee_payer,
                l1_block,
            )
            .await?;
            let treasury_allowed_on_source = account_allowed(
                &self.tempo_l1_treasury,
                route.source_portal,
                source_access_enforced,
                route.treasury,
                l1_block,
            )
            .await?;
            let treasury_allowed_on_destination = account_allowed(
                &self.tempo_l1_treasury,
                route.destination_portal,
                destination_access_enforced,
                route.treasury,
                l1_block,
            )
            .await?;

            let l1_token = ITIP20::new(route.l1_token, &self.tempo_l1_treasury);
            let source_token = ITIP20::new(route.source_token, &self.source_zone);
            let destination_token = ITIP20::new(route.destination_token, &self.destination_zone);
            let l1_token_decimals = l1_token
                .decimals()
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_token_decimals = source_token
                .decimals()
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_token_decimals = destination_token
                .decimals()
                .block(destination_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_token_paused = source_token
                .paused()
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_policy = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.source_zone)
                .tokenTransferPolicyId(route.source_token)
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_policy =
                ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.destination_zone)
                    .tokenTransferPolicyId(route.destination_token)
                    .block(destination_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let l1_policy = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.tempo_l1_treasury)
                .tokenTransferPolicyId(route.l1_token)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_policy_sender_allowed = !source_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.source_zone)
                    .isAuthorizedSender(source_policy.policyId, route.source_inventory)
                    .block(source_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let source_policy_fee_payer_allowed = !source_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.source_zone)
                    .isAuthorizedSender(source_policy.policyId, route.source_fee_payer)
                    .block(source_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let source_policy_fallback_allowed = !source_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.source_zone)
                    .isAuthorizedMintRecipient(source_policy.policyId, route.source_fallback)
                    .block(source_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let source_fallback_receive_allowed =
                ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.source_zone)
                    .validateReceivePolicy(
                        route.source_token,
                        zone_primitives::constants::ZONE_INBOX_ADDRESS,
                        route.source_fallback,
                    )
                    .block(source_block)
                    .call()
                    .await
                    .map_err(provider_error)?
                    .authorized;
            let destination_policy_mint_allowed = !destination_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.destination_zone)
                    .isAuthorizedMintRecipient(
                        destination_policy.policyId,
                        route.destination_fast_transfer,
                    )
                    .block(destination_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let treasury_policy_allowed = !l1_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.tempo_l1_treasury)
                    .isAuthorizedRecipient(l1_policy.policyId, route.treasury)
                    .block(l1_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let treasury_policy_sender_allowed = !l1_policy.isSet
                || ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.tempo_l1_treasury)
                    .isAuthorizedSender(l1_policy.policyId, route.treasury)
                    .block(l1_block)
                    .call()
                    .await
                    .map_err(provider_error)?;
            let treasury_receive_allowed =
                ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.tempo_l1_treasury)
                    .validateReceivePolicy(route.l1_token, route.source_portal, route.treasury)
                    .block(l1_block)
                    .call()
                    .await
                    .map_err(provider_error)?
                    .authorized;
            let treasury_refund_receive_allowed =
                ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.tempo_l1_treasury)
                    .validateReceivePolicy(route.l1_token, route.destination_portal, route.treasury)
                    .block(l1_block)
                    .call()
                    .await
                    .map_err(provider_error)?
                    .authorized;
            let destination_token_paused = destination_token
                .paused()
                .block(destination_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_outbox_allowance = source_token
                .allowance(route.source_inventory, ZONE_OUTBOX_ADDRESS)
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let source_fee_payer_outbox_allowance = source_token
                .allowance(route.source_fee_payer, ZONE_OUTBOX_ADDRESS)
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let destination_portal_allowance = l1_token
                .allowance(route.treasury, route.destination_portal)
                .block(l1_block)
                .call()
                .await
                .map_err(provider_error)?;

            let source_fee_reserve = ITIP20::new(route.source_fee_token, &self.source_zone)
                .balanceOf(route.source_fee_payer)
                .block(source_block)
                .call()
                .await
                .map_err(provider_error)?;
            let treasury_fee_reserve =
                ITIP20::new(route.treasury_fee_token, &self.tempo_l1_treasury)
                    .balanceOf(route.treasury_fee_payer)
                    .block(l1_block)
                    .call()
                    .await
                    .map_err(provider_error)?;

            let destination_fast =
                IFastTransfer::new(route.destination_fast_transfer, &self.destination_zone);
            let pool = destination_fast
                .poolState(route.destination_token)
                .block(destination_block)
                .call()
                .await
                .map_err(provider_error)?;
            let configured_operator = destination_fast
                .replenishmentRoute(route.destination_token, route.treasury)
                .block(destination_block)
                .call()
                .await
                .map_err(provider_error)?;

            Ok(FinalizedRouteSnapshot {
                l1_block_number,
                l1_block_hash,
                source_block_number,
                destination_block_number,
                l1_chain_id,
                source_chain_id,
                destination_chain_id,
                source_portal: route.source_portal,
                destination_portal: route.destination_portal,
                l1_token: route.l1_token,
                source_token: route.source_token,
                destination_token: route.destination_token,
                source_inventory: route.source_inventory,
                source_fallback: route.source_fallback,
                treasury: route.treasury,
                destination_pool_operator: route.destination_pool_operator,
                source_fast_epoch,
                destination_fast_epoch,
                source_protocol_version: source_epoch.protocolVersion,
                destination_protocol_version: destination_epoch.protocolVersion,
                source_epoch_open: source_fast_active
                    && !source_epoch.closed
                    && !source_epoch.retired
                    && !source_portal_paused,
                destination_epoch_open: destination_fast_active
                    && !destination_epoch.closed
                    && !destination_epoch.retired
                    && !destination_portal_paused,
                l1_token_decimals,
                source_token_decimals,
                destination_token_decimals,
                destination_key_index: destination_key.keyIndex,
                destination_key_x: destination_key.x,
                destination_key_y_parity: destination_key.yParity,
                source_inventory_allowed,
                source_fallback_allowed: source_fallback_allowed
                    && source_policy_fallback_allowed
                    && source_fallback_receive_allowed,
                source_fee_payer_allowed,
                treasury_allowed_on_source,
                treasury_allowed_on_destination,
                source_token_enabled: source_token_config.enabled
                    && !source_token_paused
                    && source_policy_sender_allowed
                    && source_policy_fee_payer_allowed,
                destination_token_enabled: destination_token_config.enabled
                    && !destination_token_paused
                    && destination_policy_mint_allowed
                    && treasury_policy_allowed
                    && treasury_policy_sender_allowed
                    && treasury_receive_allowed
                    && treasury_refund_receive_allowed,
                destination_deposits_active: destination_token_config.depositsActive
                    && key_valid.valid,
                source_outbox_allowance,
                source_fee_payer_outbox_allowance,
                destination_portal_allowance,
                source_fee_reserve,
                treasury_fee_reserve,
                destination_pool_initialized: pool.operator == route.destination_pool_operator,
                destination_replenishment_route_configured: configured_operator
                    == route.destination_pool_operator,
            })
        })
    }

    fn submit_or_observe_withdrawal<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedWithdrawalAction,
    ) -> ProviderFuture<'a, CanonicalWithdrawalObservation> {
        Box::pin(async move { self.reconcile_or_submit_withdrawal(route, action).await })
    }

    fn observe_inventory_restoration<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedWithdrawalAction,
    ) -> ProviderFuture<'a, Option<U256>> {
        Box::pin(async move {
            let job = IFastTransfer::new(route.source_fast_transfer, &self.source_zone)
                .inventoryJob(action.job_id)
                .call()
                .await
                .map_err(provider_error)?;
            validate_inventory_job(route, action, &job)?;
            Ok(job.restored.then_some(U256::from(job.amount)))
        })
    }

    fn submit_or_observe_deposit<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedDepositAction,
    ) -> ProviderFuture<'a, CanonicalDepositObservation> {
        Box::pin(async move { self.reconcile_or_submit_deposit(route, action).await })
    }

    fn observe_treasury_refund<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedDepositAction,
    ) -> ProviderFuture<'a, CanonicalRefundObservation> {
        Box::pin(async move { self.observe_refund(route, action).await })
    }

    fn submit_or_observe_refund_claim<'a>(
        &'a self,
        route: &'a ReplenishmentRouteConfig,
        action: &'a PreparedRefundClaimAction,
    ) -> ProviderFuture<'a, Option<U256>> {
        Box::pin(async move { self.reconcile_or_submit_refund_claim(route, action).await })
    }
}

impl AlloyReplenishmentProviderHandles {
    async fn reconcile_or_submit_withdrawal(
        &self,
        route: &ReplenishmentRouteConfig,
        action: &PreparedWithdrawalAction,
    ) -> Result<CanonicalWithdrawalObservation, ReplenishmentProviderError> {
        validate_withdrawal_action(route, action)?;
        let fast = IFastTransfer::new(route.source_fast_transfer, &self.source_zone);
        let job = fast
            .inventoryJob(action.job_id)
            .call()
            .await
            .map_err(provider_error)?;
        if job.intentHash.is_zero() {
            let candidate_hash = submit_action(
                &self.source_zone,
                route.source_inventory,
                route.source_fast_transfer,
                action.signer_nonce,
                action.calldata.clone(),
                route.source_fee_token,
            )
            .await;
            return Ok(CanonicalWithdrawalObservation::Pending {
                submission_hash: candidate_hash,
                canonical_transaction_hash: None,
                fallback_nonce: None,
                withdrawal_index: None,
                sender_tag: None,
                source_transaction_cost: None,
                withdrawal_fee: None,
                queue_index: None,
                accepted_batch_hash: None,
            });
        }
        validate_inventory_job(route, action, &job)?;

        let allocation_logs = self
            .source_zone
            .get_logs(
                &Filter::new()
                    .address(route.source_fast_transfer)
                    .event_signature(IFastTransfer::InventoryAllocated::SIGNATURE_HASH)
                    .topic1(action.job_id)
                    .from_block(action.not_before_source_block),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&allocation_logs)?;
        let mut allocation = None;
        for log in allocation_logs {
            let event = IFastTransfer::InventoryAllocated::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.intentHash == action.native_job_intent_hash
                && event.operator == route.source_inventory
                && event.token == action.token
                && event.treasury == action.treasury
                && U256::from(event.amount) == action.gross_amount
                && event.fallbackNonce == job.fallbackNonce
                && event.withdrawalIndex == job.withdrawalIndex
            {
                allocation = Some((log, event));
                break;
            }
        }
        let Some((allocation_log, _)) = allocation else {
            return Err(ReplenishmentProviderError::InvalidObservation);
        };
        let (source_transaction_hash, source_transaction_cost) = canonical_action_transaction(
            &self.source_zone,
            &allocation_log,
            CanonicalActionIntent {
                from: route.source_inventory,
                fee_payer: route.source_fee_payer,
                fee_token: route.source_fee_token,
                to: route.source_fast_transfer,
                nonce: action.signer_nonce,
                calldata: &action.calldata,
            },
        )
        .await?;
        let withdrawal_fee = validate_withdrawal_request(
            &self.source_zone,
            route,
            action,
            job.fallbackNonce,
            job.withdrawalIndex,
            allocation_log
                .block_number
                .ok_or(ReplenishmentProviderError::InvalidObservation)?,
        )
        .await?;
        let sender_tag = withdrawal_sender_tag(
            route.source_inventory,
            source_transaction_hash,
            job.fallbackNonce,
        );

        let allocation_block = allocation_log
            .block_number
            .ok_or(ReplenishmentProviderError::InvalidObservation)?;
        let allocation_log_index = allocation_log.log_index.unwrap_or_default();
        let finalized_logs = self
            .source_zone
            .get_logs(
                &Filter::new()
                    .address(ZONE_OUTBOX_ADDRESS)
                    .event_signature(IZoneOutbox::BatchFinalized::SIGNATURE_HASH)
                    .from_block(allocation_block)
                    .to_block(allocation_block),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&finalized_logs)?;
        let mut batch_index = None;
        for log in finalized_logs {
            if log.block_number == Some(allocation_block)
                && log.log_index.unwrap_or_default() <= allocation_log_index
            {
                continue;
            }
            let event = IZoneOutbox::BatchFinalized::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            batch_index = Some(event.withdrawalBatchIndex);
            break;
        }
        let Some(batch_index) = batch_index else {
            return Ok(CanonicalWithdrawalObservation::Pending {
                submission_hash: None,
                canonical_transaction_hash: Some(source_transaction_hash),
                fallback_nonce: Some(job.fallbackNonce),
                withdrawal_index: Some(job.withdrawalIndex),
                sender_tag: Some(sender_tag),
                source_transaction_cost: Some(source_transaction_cost),
                withdrawal_fee: Some(withdrawal_fee),
                queue_index: None,
                accepted_batch_hash: None,
            });
        };

        let l1_finalized = self
            .tempo_l1_treasury
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(provider_error)?
            .ok_or(ReplenishmentProviderError::FinalizedStateUnavailable)?;
        let accepted_logs = self
            .tempo_l1_treasury
            .get_logs(
                &Filter::new()
                    .address(route.source_portal)
                    .event_signature(vec![
                        BatchSubmitted::SIGNATURE_HASH,
                        LegacyBatchSubmitted::SIGNATURE_HASH,
                    ])
                    .topic1(B256::from(U256::from(batch_index)))
                    .from_block(action.not_before_l1_block)
                    .to_block(l1_finalized.number()),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&accepted_logs)?;
        let mut accepted = None;
        for log in accepted_logs {
            let (observed_batch, queue_index) = decode_accepted_batch(&log)?;
            if observed_batch == batch_index {
                let transaction_hash =
                    canonical_log_transaction(&self.tempo_l1_treasury, &log).await?;
                accepted = Some((
                    transaction_hash,
                    queue_index,
                    log.block_number
                        .ok_or(ReplenishmentProviderError::InvalidObservation)?,
                ));
                break;
            }
        }
        let Some((accepted_batch_hash, queue_index, accepted_block_number)) = accepted else {
            return Ok(CanonicalWithdrawalObservation::Pending {
                submission_hash: None,
                canonical_transaction_hash: Some(source_transaction_hash),
                fallback_nonce: Some(job.fallbackNonce),
                withdrawal_index: Some(job.withdrawalIndex),
                sender_tag: Some(sender_tag),
                source_transaction_cost: Some(source_transaction_cost),
                withdrawal_fee: Some(withdrawal_fee),
                queue_index: None,
                accepted_batch_hash: None,
            });
        };

        let bounce_logs = self
            .tempo_l1_treasury
            .get_logs(
                &Filter::new()
                    .address(route.source_portal)
                    .event_signature(ZonePortal::WithdrawalBounceBack::SIGNATURE_HASH)
                    .topic1(B256::from(U256::from(job.fallbackNonce)))
                    .from_block(accepted_block_number)
                    .to_block(l1_finalized.number()),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&bounce_logs)?;
        for log in bounce_logs {
            let event = ZonePortal::WithdrawalBounceBack::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.token == route.l1_token && U256::from(event.amount) == action.gross_amount {
                let (transaction_hash, l1_transaction_cost) =
                    canonical_log_transaction_cost(&self.tempo_l1_treasury, &log).await?;
                return Ok(CanonicalWithdrawalObservation::Bounced {
                    transaction_hash: source_transaction_hash,
                    queue_index,
                    accepted_batch_hash,
                    fallback_nonce: job.fallbackNonce,
                    withdrawal_index: job.withdrawalIndex,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    l1_transaction_cost: TransactionCost {
                        transaction_hash,
                        ..l1_transaction_cost
                    },
                });
            }
        }

        let processed_logs = self
            .tempo_l1_treasury
            .get_logs(
                &Filter::new()
                    .address(route.source_portal)
                    .event_signature(ZonePortal::WithdrawalProcessed::SIGNATURE_HASH)
                    .topic1(route.treasury.into_word())
                    .topic2(sender_tag)
                    .from_block(accepted_block_number)
                    .to_block(l1_finalized.number()),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&processed_logs)?;
        for log in processed_logs {
            let event = ZonePortal::WithdrawalProcessed::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.token == route.l1_token
                && U256::from(event.amount) == action.gross_amount
                && event.callbackSuccess
            {
                let (transaction_hash, l1_transaction_cost) =
                    canonical_log_transaction_cost(&self.tempo_l1_treasury, &log).await?;
                require_canonical_token_transfer(
                    &self.tempo_l1_treasury,
                    transaction_hash,
                    route.l1_token,
                    route.source_portal,
                    route.treasury,
                    action.gross_amount,
                )
                .await?;
                return Ok(CanonicalWithdrawalObservation::TreasuryCredited {
                    transaction_hash: source_transaction_hash,
                    queue_index,
                    accepted_batch_hash,
                    amount: action.gross_amount,
                    fallback_nonce: job.fallbackNonce,
                    withdrawal_index: job.withdrawalIndex,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    l1_transaction_cost: TransactionCost {
                        transaction_hash,
                        ..l1_transaction_cost
                    },
                });
            }
        }

        Ok(CanonicalWithdrawalObservation::Pending {
            submission_hash: None,
            canonical_transaction_hash: Some(source_transaction_hash),
            fallback_nonce: Some(job.fallbackNonce),
            withdrawal_index: Some(job.withdrawalIndex),
            sender_tag: Some(sender_tag),
            source_transaction_cost: Some(source_transaction_cost),
            withdrawal_fee: Some(withdrawal_fee),
            queue_index: Some(queue_index),
            accepted_batch_hash: Some(accepted_batch_hash),
        })
    }

    async fn reconcile_or_submit_deposit(
        &self,
        route: &ReplenishmentRouteConfig,
        action: &PreparedDepositAction,
    ) -> Result<CanonicalDepositObservation, ReplenishmentProviderError> {
        validate_deposit_action(route, action)?;
        let Some(deposit) = self.find_deposit(route, action).await? else {
            let candidate_hash = submit_action(
                &self.tempo_l1_treasury,
                route.treasury,
                action.portal,
                action.signer_nonce,
                action.calldata.clone(),
                route.treasury_fee_token,
            )
            .await;
            return Ok(CanonicalDepositObservation::Pending {
                submission_hash: candidate_hash,
                canonical_transaction_hash: None,
                transaction_cost: None,
                deposit_fee: None,
                deposit_number: None,
            });
        };

        let destination_fast =
            IFastTransfer::new(route.destination_fast_transfer, &self.destination_zone);
        let credit = destination_fast
            .replenishmentCredit(action.job_id)
            .call()
            .await
            .map_err(provider_error)?;
        if credit != 0 {
            if credit != deposit.net_amount {
                return Err(ReplenishmentProviderError::InvalidObservation);
            }
            let credit_logs = self
                .destination_zone
                .get_logs(
                    &Filter::new()
                        .address(route.destination_fast_transfer)
                        .event_signature(IFastTransfer::ReplenishmentCredited::SIGNATURE_HASH)
                        .topic1(action.job_id)
                        .from_block(action.not_before_destination_block),
                )
                .await
                .map_err(provider_error)?;
            require_bounded_logs(&credit_logs)?;
            let mut committed_credit = false;
            for log in credit_logs {
                let event = IFastTransfer::ReplenishmentCredited::decode_log(&log.inner)
                    .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                    .data;
                if event.token == route.destination_token
                    && event.operator == route.destination_pool_operator
                    && event.amount == credit
                {
                    canonical_log_transaction(&self.destination_zone, &log).await?;
                    committed_credit = true;
                    break;
                }
            }
            if !committed_credit {
                return Err(ReplenishmentProviderError::InvalidObservation);
            }
            return Ok(CanonicalDepositObservation::PoolCredited {
                transaction_hash: deposit.transaction_hash,
                deposit_number: deposit.deposit_number,
                amount: U256::from(credit),
                transaction_cost: deposit.transaction_cost,
                deposit_fee: TokenAmount {
                    token: action.token,
                    amount: U256::from(deposit.deposit_fee),
                },
            });
        }

        let failed_logs = self
            .destination_zone
            .get_logs(
                &Filter::new()
                    .address(zone_primitives::constants::ZONE_INBOX_ADDRESS)
                    .event_signature(vec![
                        tempo_zone_contracts::IZoneInbox::DepositFailed::SIGNATURE_HASH,
                        tempo_zone_contracts::IZoneInbox::DepositRejected::SIGNATURE_HASH,
                    ])
                    .topic1(deposit.deposit_hash)
                    .from_block(action.not_before_destination_block),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&failed_logs)?;
        if failed_logs.is_empty() {
            return Ok(CanonicalDepositObservation::Pending {
                submission_hash: None,
                canonical_transaction_hash: Some(deposit.transaction_hash),
                transaction_cost: Some(deposit.transaction_cost),
                deposit_fee: Some(TokenAmount {
                    token: action.token,
                    amount: U256::from(deposit.deposit_fee),
                }),
                deposit_number: Some(deposit.deposit_number),
            });
        }
        for log in &failed_logs {
            canonical_log_transaction(&self.destination_zone, log).await?;
        }
        Ok(CanonicalDepositObservation::RefundPending {
            transaction_hash: deposit.transaction_hash,
            deposit_number: deposit.deposit_number,
            transaction_cost: deposit.transaction_cost,
            deposit_fee: TokenAmount {
                token: action.token,
                amount: U256::from(deposit.deposit_fee),
            },
        })
    }

    async fn observe_refund(
        &self,
        route: &ReplenishmentRouteConfig,
        action: &PreparedDepositAction,
    ) -> Result<CanonicalRefundObservation, ReplenishmentProviderError> {
        validate_deposit_action(route, action)?;
        let Some(deposit) = self.find_deposit(route, action).await? else {
            return Ok(CanonicalRefundObservation::Pending);
        };
        let finalized = self
            .tempo_l1_treasury
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(provider_error)?
            .ok_or(ReplenishmentProviderError::FinalizedStateUnavailable)?;
        let filter = Filter::new()
            .address(route.destination_portal)
            .topic1(route.treasury.into_word())
            .from_block(deposit.block_number)
            .to_block(finalized.number());
        let direct_logs = self
            .tempo_l1_treasury
            .get_logs(
                &filter
                    .clone()
                    .event_signature(ZonePortal::DepositBounceBack::SIGNATURE_HASH),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&direct_logs)?;
        let mut matching_direct_refund = None;
        for log in direct_logs {
            let event = ZonePortal::DepositBounceBack::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.token == route.l1_token
                && refund_matches_deposit(deposit.net_amount, event.amount, event.bouncebackFee)
            {
                let transaction_hash =
                    canonical_log_transaction(&self.tempo_l1_treasury, &log).await?;
                require_canonical_token_transfer(
                    &self.tempo_l1_treasury,
                    transaction_hash,
                    route.l1_token,
                    route.destination_portal,
                    route.treasury,
                    U256::from(event.amount),
                )
                .await?;
                retain_unique_refund(&mut matching_direct_refund, event.amount)?;
            }
        }
        if let Some(amount) = matching_direct_refund {
            return Ok(CanonicalRefundObservation::Refunded(U256::from(amount)));
        }
        let portal = ZonePortal::new(route.destination_portal, &self.tempo_l1_treasury);
        let claimable = portal
            .refunds(route.l1_token, route.treasury)
            .block(BlockId::hash_canonical(finalized.hash_slow()))
            .call()
            .await
            .map_err(provider_error)?;
        if claimable == 0 {
            return Ok(CanonicalRefundObservation::Pending);
        }
        let pending_logs = self
            .tempo_l1_treasury
            .get_logs(&filter.event_signature(ZonePortal::DepositBounceBackPending::SIGNATURE_HASH))
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&pending_logs)?;
        let mut matching_pending = None;
        for log in pending_logs {
            let event = ZonePortal::DepositBounceBackPending::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.token == route.l1_token
                && refund_matches_deposit(deposit.net_amount, event.amount, event.bouncebackFee)
            {
                canonical_log_transaction(&self.tempo_l1_treasury, &log).await?;
                retain_unique_refund(&mut matching_pending, event.amount)?;
            }
        }
        let Some(matching_pending) = matching_pending else {
            return Err(ReplenishmentProviderError::AmbiguousJobEvidence);
        };
        // The slot is aggregated. It is assignable only when the one exact failed-deposit amount
        // above equals the entire finalized slot; otherwise another job's liability is present.
        if claimable != matching_pending {
            return Err(ReplenishmentProviderError::AmbiguousAggregatedRefund);
        }
        Ok(CanonicalRefundObservation::ClaimRequired(U256::from(
            claimable,
        )))
    }

    async fn reconcile_or_submit_refund_claim(
        &self,
        route: &ReplenishmentRouteConfig,
        action: &PreparedRefundClaimAction,
    ) -> Result<Option<U256>, ReplenishmentProviderError> {
        validate_refund_claim_action(route, action)?;
        let finalized = self
            .tempo_l1_treasury
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(provider_error)?
            .ok_or(ReplenishmentProviderError::FinalizedStateUnavailable)?;
        let logs = self
            .tempo_l1_treasury
            .get_logs(
                &Filter::new()
                    .address(action.portal)
                    .event_signature(ZonePortal::RefundClaimed::SIGNATURE_HASH)
                    .topic1(route.treasury.into_word())
                    .topic2(action.token.into_word())
                    .from_block(action.not_before_l1_block)
                    .to_block(finalized.number()),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&logs)?;
        for log in logs {
            let event = ZonePortal::RefundClaimed::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            if event.amount == action.expected_amount {
                let (transaction_hash, _) = canonical_action_transaction(
                    &self.tempo_l1_treasury,
                    &log,
                    CanonicalActionIntent {
                        from: route.treasury,
                        fee_payer: route.treasury_fee_payer,
                        fee_token: route.treasury_fee_token,
                        to: action.portal,
                        nonce: action.signer_nonce,
                        calldata: &action.calldata,
                    },
                )
                .await?;
                require_canonical_token_transfer(
                    &self.tempo_l1_treasury,
                    transaction_hash,
                    action.token,
                    action.portal,
                    route.treasury,
                    U256::from(event.amount),
                )
                .await?;
                return Ok(Some(U256::from(event.amount)));
            }
        }
        let portal = ZonePortal::new(action.portal, &self.tempo_l1_treasury);
        let claimable = portal
            .refunds(action.token, route.treasury)
            .block(BlockId::hash_canonical(finalized.hash_slow()))
            .call()
            .await
            .map_err(provider_error)?;
        if claimable == action.expected_amount {
            let _ = submit_action(
                &self.tempo_l1_treasury,
                route.treasury,
                action.portal,
                action.signer_nonce,
                action.calldata.clone(),
                route.treasury_fee_token,
            )
            .await;
        } else if claimable != 0 {
            return Err(ReplenishmentProviderError::AmbiguousAggregatedRefund);
        }
        Ok(None)
    }

    async fn find_deposit(
        &self,
        route: &ReplenishmentRouteConfig,
        action: &PreparedDepositAction,
    ) -> Result<Option<ObservedDeposit>, ReplenishmentProviderError> {
        let finalized = self
            .tempo_l1_treasury
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(provider_error)?
            .ok_or(ReplenishmentProviderError::FinalizedStateUnavailable)?;
        let logs = self
            .tempo_l1_treasury
            .get_logs(
                &Filter::new()
                    .address(action.portal)
                    .event_signature(ZonePortal::DepositMade::SIGNATURE_HASH)
                    .topic2(route.treasury.into_word())
                    .from_block(action.not_before_l1_block)
                    .to_block(finalized.number()),
            )
            .await
            .map_err(provider_error)?;
        require_bounded_logs(&logs)?;
        for log in logs {
            let event = ZonePortal::DepositMade::decode_log(&log.inner)
                .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
                .data;
            let total = event
                .netAmount
                .checked_add(event.fee)
                .ok_or(ReplenishmentProviderError::InvalidObservation)?;
            if event.token == action.token
                && total == action.amount
                && U256::from(event.fee) <= route.maximum_destination_deposit_fee
                && event.keyIndex == action.key_index
                && event.ephemeralPubkeyX == action.encrypted.ephemeral_pubkey_x
                && event.ephemeralPubkeyYParity == action.encrypted.ephemeral_pubkey_y_parity
                && event.ciphertext == action.encrypted.ciphertext
                && event.nonce == action.encrypted.nonce
                && event.tag == action.encrypted.tag
                && event.tempoRefundRecipient == action.refund_recipient
            {
                let (transaction_hash, transaction_cost) = canonical_action_transaction(
                    &self.tempo_l1_treasury,
                    &log,
                    CanonicalActionIntent {
                        from: route.treasury,
                        fee_payer: route.treasury_fee_payer,
                        fee_token: route.treasury_fee_token,
                        to: action.portal,
                        nonce: action.signer_nonce,
                        calldata: &action.calldata,
                    },
                )
                .await?;
                return Ok(Some(ObservedDeposit {
                    transaction_hash,
                    block_number: log
                        .block_number
                        .ok_or(ReplenishmentProviderError::InvalidObservation)?,
                    deposit_hash: event.newCurrentDepositQueueHash,
                    deposit_number: event.depositNumber,
                    net_amount: event.netAmount,
                    transaction_cost,
                    deposit_fee: event.fee,
                }));
            }
        }
        Ok(None)
    }
}

struct ObservedDeposit {
    transaction_hash: B256,
    block_number: u64,
    deposit_hash: B256,
    deposit_number: u64,
    net_amount: u128,
    transaction_cost: TransactionCost,
    deposit_fee: u128,
}

/// Concrete provider-backed bridge. Construction is asynchronous so an unvalidated route cannot
/// accidentally become enabled.
pub struct ProviderBackedReplenishmentBridge<P, S, N> {
    route: ReplenishmentRouteConfig,
    finalized: FinalizedRouteSnapshot,
    provider: Arc<P>,
    actions: Arc<S>,
    nonces: Arc<N>,
}

impl<P, S, N> ProviderBackedReplenishmentBridge<P, S, N>
where
    P: ReplenishmentProvider,
    S: PreparedActionStore,
    N: ReplenishmentNoncePlanner,
{
    pub async fn enable(
        route: ReplenishmentRouteConfig,
        provider: Arc<P>,
        actions: Arc<S>,
        nonces: Arc<N>,
    ) -> Result<Self, ReplenishmentProviderError> {
        route.validate_local()?;
        let finalized = provider.validate_route(&route).await?;
        finalized.validate_against(&route)?;
        Ok(Self {
            route,
            finalized,
            provider,
            actions,
            nonces,
        })
    }

    pub const fn finalized_route(&self) -> &FinalizedRouteSnapshot {
        &self.finalized
    }

    fn validate_job(&self, job: &ReplenishmentJob) -> Result<(), ReplenishmentProviderError> {
        let mut contribution_ids = std::collections::BTreeSet::new();
        let contribution_total = job
            .contributions
            .iter()
            .try_fold(U256::ZERO, |total, item| {
                if item.transfer_id.is_zero()
                    || item.amount.is_zero()
                    || !contribution_ids.insert(item.transfer_id)
                {
                    return None;
                }
                total.checked_add(item.amount)
            });
        if job.job_id.is_zero()
            || job.source_inventory != self.route.source_inventory
            || job.source_fallback != self.route.source_fallback
            || job.treasury != self.route.treasury
            || job.deposit_refund != self.route.treasury
            || job.destination_pool != self.route.destination_pool_operator
            || job.gross_amount.is_zero()
            || job.gross_amount > self.route.maximum_replenishment_amount
            || job.contributions.is_empty()
            || job.contributions.len() > MAX_REPLENISHMENT_CONTRIBUTIONS
            || contribution_total != Some(job.gross_amount)
        {
            return Err(ReplenishmentProviderError::JobRouteMismatch);
        }
        Ok(())
    }

    fn withdrawal_action(
        &self,
        job: &ReplenishmentJob,
    ) -> Result<PreparedWithdrawalAction, ReplenishmentProviderError> {
        self.validate_job(job)?;
        let signer_nonce = self.nonces.source_signer_nonce(job.job_id)?;
        let transfer_ids = job
            .contributions
            .iter()
            .map(|contribution| contribution.transfer_id)
            .collect::<Vec<_>>();
        let calldata = Bytes::from(
            IFastTransfer::allocateInventoryAndWithdrawCall {
                jobId: job.job_id,
                transferIds: transfer_ids.clone(),
                token: self.route.source_token,
                treasury: self.route.treasury,
            }
            .abi_encode(),
        );
        let intent_hash = transaction_intent_hash(
            self.route.source_chain_id,
            self.route.source_fast_transfer,
            signer_nonce,
            self.route.source_fee_payer,
            self.route.source_fee_token,
            &calldata,
        );
        let native_job_intent_hash = inventory_job_intent_hash(
            job.job_id,
            self.route.source_inventory,
            self.route.source_fee_payer,
            self.route.source_token,
            self.route.treasury,
            &transfer_ids,
        );
        Ok(PreparedWithdrawalAction {
            job_id: job.job_id,
            transfer_ids,
            token: self.route.source_token,
            treasury: self.route.treasury,
            gross_amount: job.gross_amount,
            not_before_source_block: self.finalized.source_block_number,
            not_before_l1_block: self.finalized.l1_block_number,
            signer_nonce,
            fee_payer: self.route.source_fee_payer,
            fee_token: self.route.source_fee_token,
            calldata,
            native_job_intent_hash,
            intent_hash,
        })
    }

    fn deposit_action(
        &self,
        job: &ReplenishmentJob,
        amount: U256,
    ) -> Result<PreparedDepositAction, ReplenishmentProviderError> {
        self.validate_job(job)?;
        let amount =
            u128::try_from(amount).map_err(|_| ReplenishmentProviderError::InvalidAmount)?;
        if amount == 0 || U256::from(amount) > job.gross_amount {
            return Err(ReplenishmentProviderError::InvalidAmount);
        }
        let signer_nonce = self.nonces.treasury_signer_nonce(job.job_id)?;
        let encrypted = encrypt_deposit(
            &self.finalized.destination_key_x,
            self.finalized.destination_key_y_parity,
            self.route.destination_fast_transfer,
            job.job_id,
            self.route.treasury,
            self.route.destination_portal,
            self.finalized.destination_key_index,
        )
        .ok_or(ReplenishmentProviderError::Encryption)?;
        let encrypted = PersistedDepositPayload {
            ephemeral_pubkey_x: encrypted.eph_pub_x,
            ephemeral_pubkey_y_parity: encrypted.eph_pub_y_parity,
            ciphertext: encrypted.ciphertext.into(),
            nonce: encrypted.nonce.into(),
            tag: encrypted.tag.into(),
        };
        let calldata = Bytes::from(
            ZonePortal::depositCall {
                token: self.route.l1_token,
                amount,
                keyIndex: self.finalized.destination_key_index,
                encrypted: encrypted.as_abi(),
                tempoRefundRecipient: self.route.treasury,
            }
            .abi_encode(),
        );
        let intent_hash = transaction_intent_hash(
            self.route.l1_chain_id,
            self.route.destination_portal,
            signer_nonce,
            self.route.treasury_fee_payer,
            self.route.treasury_fee_token,
            &calldata,
        );
        Ok(PreparedDepositAction {
            job_id: job.job_id,
            portal: self.route.destination_portal,
            token: self.route.l1_token,
            amount,
            not_before_l1_block: self.finalized.l1_block_number,
            not_before_destination_block: self.finalized.destination_block_number,
            key_index: self.finalized.destination_key_index,
            encrypted,
            pool_recipient: self.route.destination_fast_transfer,
            refund_recipient: self.route.treasury,
            signer_nonce,
            fee_payer: self.route.treasury_fee_payer,
            fee_token: self.route.treasury_fee_token,
            calldata,
            intent_hash,
        })
    }

    fn refund_claim_action(
        &self,
        job: &ReplenishmentJob,
        expected_amount: U256,
    ) -> Result<PreparedRefundClaimAction, ReplenishmentProviderError> {
        self.validate_job(job)?;
        let expected_amount = u128::try_from(expected_amount)
            .map_err(|_| ReplenishmentProviderError::InvalidAmount)?;
        if expected_amount == 0 {
            return Err(ReplenishmentProviderError::InvalidAmount);
        }
        let signer_nonce = self.nonces.treasury_refund_claim_nonce(job.job_id)?;
        let calldata = Bytes::from(
            ZonePortal::claimRefundCall {
                token: self.route.l1_token,
            }
            .abi_encode(),
        );
        let intent_hash = transaction_intent_hash(
            self.route.l1_chain_id,
            self.route.destination_portal,
            signer_nonce,
            self.route.treasury_fee_payer,
            self.route.treasury_fee_token,
            &calldata,
        );
        Ok(PreparedRefundClaimAction {
            job_id: job.job_id,
            portal: self.route.destination_portal,
            token: self.route.l1_token,
            expected_amount,
            not_before_l1_block: self.finalized.l1_block_number,
            signer_nonce,
            fee_payer: self.route.treasury_fee_payer,
            fee_token: self.route.treasury_fee_token,
            calldata,
            intent_hash,
        })
    }

    fn stored_withdrawal(
        &self,
        record: &WithdrawalRecord,
    ) -> Result<PreparedWithdrawalAction, ReplenishmentProviderError> {
        match self.actions.get(record.transaction_intent_hash)? {
            PreparedReplenishmentAction::Withdrawal(action)
                if action.intent_hash == record.transaction_intent_hash
                    && action.signer_nonce == record.signer_nonce =>
            {
                Ok(action)
            }
            _ => Err(ReplenishmentProviderError::PreparedActionMismatch),
        }
    }

    fn stored_deposit(
        &self,
        record: &DepositRecord,
    ) -> Result<PreparedDepositAction, ReplenishmentProviderError> {
        match self.actions.get(record.transaction_intent_hash)? {
            PreparedReplenishmentAction::Deposit(action)
                if action.intent_hash == record.transaction_intent_hash
                    && action.signer_nonce == record.signer_nonce =>
            {
                Ok(action)
            }
            _ => Err(ReplenishmentProviderError::PreparedActionMismatch),
        }
    }

    fn stored_refund_claim(
        &self,
        job_id: B256,
        signer_nonce: u64,
    ) -> Result<PreparedRefundClaimAction, ReplenishmentProviderError> {
        let calldata = Bytes::from(
            ZonePortal::claimRefundCall {
                token: self.route.l1_token,
            }
            .abi_encode(),
        );
        let intent_hash = transaction_intent_hash(
            self.route.l1_chain_id,
            self.route.destination_portal,
            signer_nonce,
            self.route.treasury_fee_payer,
            self.route.treasury_fee_token,
            &calldata,
        );
        match self.actions.get(intent_hash)? {
            PreparedReplenishmentAction::RefundClaim(action)
                if action.job_id == job_id
                    && action.signer_nonce == signer_nonce
                    && action.intent_hash == intent_hash =>
            {
                Ok(action)
            }
            _ => Err(ReplenishmentProviderError::PreparedActionMismatch),
        }
    }
}

impl<P, S, N> ReplenishmentBridge for ProviderBackedReplenishmentBridge<P, S, N>
where
    P: ReplenishmentProvider,
    S: PreparedActionStore,
    N: ReplenishmentNoncePlanner,
{
    fn prepare_withdrawal(
        &self,
        job: &ReplenishmentJob,
    ) -> Result<WithdrawalRecord, ReplenishmentWorkerError> {
        let action = self.withdrawal_action(job).map_err(worker_error)?;
        self.actions
            .put(PreparedReplenishmentAction::Withdrawal(action.clone()))
            .map_err(worker_error)?;
        Ok(WithdrawalRecord {
            transaction_intent_hash: action.intent_hash,
            signer_nonce: action.signer_nonce,
            fallback_nonce: None,
            withdrawal_index: None,
            sender_tag: None,
            submission_hashes: Vec::new(),
            transaction_hash: None,
            accepted_batch_hash: None,
            queue_index: None,
            source_transaction_cost: None,
            withdrawal_fee: None,
            l1_transaction_cost: None,
        })
    }

    fn submit_or_reconcile_withdrawal<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a WithdrawalRecord,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            self.validate_job(job).map_err(worker_error)?;
            let action = self.stored_withdrawal(record).map_err(worker_error)?;
            let observation = self
                .provider
                .submit_or_observe_withdrawal(&self.route, &action)
                .await
                .map_err(worker_error)?;
            match observation {
                CanonicalWithdrawalObservation::Pending {
                    submission_hash,
                    canonical_transaction_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    queue_index,
                    accepted_batch_hash,
                } => Ok(LegObservation::WithdrawalPending {
                    submission_hash,
                    canonical_transaction_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    queue_index,
                    accepted_batch_hash,
                }),
                CanonicalWithdrawalObservation::TreasuryCredited {
                    transaction_hash,
                    queue_index,
                    accepted_batch_hash,
                    amount,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    l1_transaction_cost,
                } if !amount.is_zero() && amount <= job.gross_amount => {
                    Ok(LegObservation::TreasuryCredited {
                        transaction_hash,
                        queue_index,
                        accepted_batch_hash,
                        net_amount: amount,
                        fallback_nonce,
                        withdrawal_index,
                        sender_tag,
                        source_transaction_cost,
                        withdrawal_fee,
                        l1_transaction_cost,
                    })
                }
                CanonicalWithdrawalObservation::Bounced {
                    transaction_hash,
                    queue_index,
                    accepted_batch_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    l1_transaction_cost,
                } => Ok(LegObservation::WithdrawalBounced {
                    transaction_hash,
                    queue_index,
                    accepted_batch_hash,
                    fallback_nonce,
                    withdrawal_index,
                    sender_tag,
                    source_transaction_cost,
                    withdrawal_fee,
                    l1_transaction_cost,
                }),
                _ => Err(worker_error(ReplenishmentProviderError::InvalidObservation)),
            }
        })
    }

    fn reconcile_inventory_restoration<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            self.validate_job(job).map_err(worker_error)?;
            let record = job
                .withdrawal
                .as_ref()
                .ok_or_else(|| worker_error(ReplenishmentProviderError::PreparedActionMismatch))?;
            let action = self.stored_withdrawal(record).map_err(worker_error)?;
            match self
                .provider
                .observe_inventory_restoration(&self.route, &action)
                .await
                .map_err(worker_error)?
            {
                Some(amount) if amount == job.gross_amount => {
                    Ok(LegObservation::InventoryRestored { amount })
                }
                Some(_) => Err(worker_error(ReplenishmentProviderError::InvalidObservation)),
                None => Ok(LegObservation::Pending {
                    transaction_hash: record.transaction_hash,
                    queue_index: record.queue_index,
                    accepted_batch_hash: record.accepted_batch_hash,
                }),
            }
        })
    }

    fn prepare_deposit(
        &self,
        job: &ReplenishmentJob,
        treasury_credit: U256,
    ) -> Result<DepositRecord, ReplenishmentWorkerError> {
        let action = self
            .deposit_action(job, treasury_credit)
            .map_err(worker_error)?;
        self.actions
            .put(PreparedReplenishmentAction::Deposit(action.clone()))
            .map_err(worker_error)?;
        Ok(DepositRecord {
            transaction_intent_hash: action.intent_hash,
            signer_nonce: action.signer_nonce,
            queue_index: None,
            submission_hashes: Vec::new(),
            transaction_hash: None,
            transaction_cost: None,
            deposit_fee: None,
        })
    }

    fn submit_or_reconcile_deposit<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
        record: &'a DepositRecord,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            self.validate_job(job).map_err(worker_error)?;
            let action = self.stored_deposit(record).map_err(worker_error)?;
            match self
                .provider
                .submit_or_observe_deposit(&self.route, &action)
                .await
                .map_err(worker_error)?
            {
                CanonicalDepositObservation::Pending {
                    submission_hash,
                    canonical_transaction_hash,
                    transaction_cost,
                    deposit_fee,
                    deposit_number,
                } => Ok(LegObservation::DepositPending {
                    submission_hash,
                    canonical_transaction_hash,
                    transaction_cost,
                    deposit_fee,
                    queue_index: deposit_number,
                }),
                CanonicalDepositObservation::PoolCredited {
                    transaction_hash,
                    deposit_number,
                    amount,
                    transaction_cost,
                    deposit_fee,
                } if !amount.is_zero() && amount <= U256::from(action.amount) => {
                    Ok(LegObservation::PoolCredited {
                        transaction_hash,
                        queue_index: deposit_number,
                        net_amount: amount,
                        transaction_cost,
                        deposit_fee,
                    })
                }
                CanonicalDepositObservation::RefundPending {
                    transaction_hash,
                    deposit_number,
                    transaction_cost,
                    deposit_fee,
                } => Ok(LegObservation::DepositRefundPending {
                    transaction_hash,
                    queue_index: deposit_number,
                    transaction_cost,
                    deposit_fee,
                }),
                _ => Err(worker_error(ReplenishmentProviderError::InvalidObservation)),
            }
        })
    }

    fn reconcile_treasury_refund<'a>(
        &'a self,
        job: &'a ReplenishmentJob,
    ) -> WorkerFuture<'a, LegObservation> {
        Box::pin(async move {
            self.validate_job(job).map_err(worker_error)?;
            let record = job
                .deposit
                .as_ref()
                .ok_or_else(|| worker_error(ReplenishmentProviderError::PreparedActionMismatch))?;
            let action = self.stored_deposit(record).map_err(worker_error)?;
            if let Some(nonce) = self
                .nonces
                .treasury_refund_claim_nonce_if_reserved(job.job_id)
                .map_err(worker_error)?
            {
                match self.stored_refund_claim(job.job_id, nonce) {
                    Ok(claim) => {
                        return match self
                            .provider
                            .submit_or_observe_refund_claim(&self.route, &claim)
                            .await
                            .map_err(worker_error)?
                        {
                            Some(amount) if amount == U256::from(claim.expected_amount) => {
                                Ok(LegObservation::TreasuryRefunded { amount })
                            }
                            Some(_) => {
                                Err(worker_error(ReplenishmentProviderError::InvalidObservation))
                            }
                            None => Ok(LegObservation::Pending {
                                transaction_hash: record.transaction_hash,
                                queue_index: record.queue_index,
                                accepted_batch_hash: None,
                            }),
                        };
                    }
                    // A crash can persist the nonce reservation immediately before the prepared
                    // claim. Claimable state is still present because submission follows the
                    // action fsync, so the observation below reconstructs identical bytes.
                    Err(ReplenishmentProviderError::Storage(_)) => {}
                    Err(error) => return Err(worker_error(error)),
                }
            }
            match self
                .provider
                .observe_treasury_refund(&self.route, &action)
                .await
                .map_err(worker_error)?
            {
                CanonicalRefundObservation::Refunded(amount)
                    if !amount.is_zero() && amount <= U256::from(action.amount) =>
                {
                    Ok(LegObservation::TreasuryRefunded { amount })
                }
                CanonicalRefundObservation::ClaimRequired(amount)
                    if !amount.is_zero() && amount <= U256::from(action.amount) =>
                {
                    let claim = self
                        .refund_claim_action(job, amount)
                        .map_err(worker_error)?;
                    self.actions
                        .put(PreparedReplenishmentAction::RefundClaim(claim.clone()))
                        .map_err(worker_error)?;
                    match self
                        .provider
                        .submit_or_observe_refund_claim(&self.route, &claim)
                        .await
                        .map_err(worker_error)?
                    {
                        Some(refunded) if refunded == amount => {
                            Ok(LegObservation::TreasuryRefunded { amount: refunded })
                        }
                        Some(_) => {
                            Err(worker_error(ReplenishmentProviderError::InvalidObservation))
                        }
                        None => Ok(LegObservation::Pending {
                            transaction_hash: record.transaction_hash,
                            queue_index: record.queue_index,
                            accepted_batch_hash: None,
                        }),
                    }
                }
                CanonicalRefundObservation::Pending => Ok(LegObservation::Pending {
                    transaction_hash: record.transaction_hash,
                    queue_index: record.queue_index,
                    accepted_batch_hash: None,
                }),
                _ => Err(worker_error(ReplenishmentProviderError::InvalidObservation)),
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReplenishmentProviderError {
    #[error("invalid replenishment route")]
    InvalidRoute,
    #[error("finalized L1 registry or encryption-key snapshot is invalid")]
    InvalidFinalizedRegistry,
    #[error("replenishment job does not match its enabled route")]
    JobRouteMismatch,
    #[error("invalid replenishment amount")]
    InvalidAmount,
    #[error("destination deposit encryption failed")]
    Encryption,
    #[error("prepared action conflicts with durable economic intent")]
    PreparedActionMismatch,
    #[error("provider returned non-canonical or mismatched evidence")]
    InvalidObservation,
    #[error("finalized provider state is unavailable")]
    FinalizedStateUnavailable,
    #[error("signer-filled provider configuration does not match the route")]
    MissingSignerConfiguration,
    #[error("legacy Portal refund aggregation cannot be assigned safely to one job")]
    AmbiguousAggregatedRefund,
    #[error("canonical receipt evidence cannot be assigned uniquely to this job")]
    AmbiguousJobEvidence,
    #[error("canonical reconciliation log result exceeded its configured bound")]
    ReconciliationLimitExceeded,
    #[error("provider operation failed: {0}")]
    Provider(String),
    #[error("durable action/nonce storage failed: {0}")]
    Storage(String),
}

async fn account_allowed(
    provider: &DynProvider<TempoNetwork>,
    portal_address: Address,
    access_enforced: bool,
    account: Address,
    block: BlockId,
) -> Result<bool, ReplenishmentProviderError> {
    if !access_enforced {
        return Ok(true);
    }
    ZonePortal::new(portal_address, provider)
        .hasRole(account, ZonePortal::Role::Account)
        .block(block)
        .call()
        .await
        .map_err(provider_error)
}

fn validate_withdrawal_action(
    route: &ReplenishmentRouteConfig,
    action: &PreparedWithdrawalAction,
) -> Result<(), ReplenishmentProviderError> {
    let unique_transfer_ids = action
        .transfer_ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if action.job_id.is_zero()
        || action.transfer_ids.is_empty()
        || action.transfer_ids.len() > MAX_REPLENISHMENT_CONTRIBUTIONS
        || unique_transfer_ids.len() != action.transfer_ids.len()
        || unique_transfer_ids.contains(&B256::ZERO)
        || action.token != route.source_token
        || action.treasury != route.treasury
        || action.gross_amount.is_zero()
        || action.gross_amount > route.maximum_replenishment_amount
        || action.fee_payer != route.source_fee_payer
        || action.fee_token != route.source_fee_token
        || action.calldata.as_ref()
            != (IFastTransfer::allocateInventoryAndWithdrawCall {
                jobId: action.job_id,
                transferIds: action.transfer_ids.clone(),
                token: action.token,
                treasury: action.treasury,
            })
            .abi_encode()
            .as_slice()
        || action.native_job_intent_hash
            != inventory_job_intent_hash(
                action.job_id,
                route.source_inventory,
                action.fee_payer,
                action.token,
                action.treasury,
                &action.transfer_ids,
            )
        || action.intent_hash
            != transaction_intent_hash(
                route.source_chain_id,
                route.source_fast_transfer,
                action.signer_nonce,
                action.fee_payer,
                action.fee_token,
                &action.calldata,
            )
    {
        return Err(ReplenishmentProviderError::PreparedActionMismatch);
    }
    Ok(())
}

fn validate_deposit_action(
    route: &ReplenishmentRouteConfig,
    action: &PreparedDepositAction,
) -> Result<(), ReplenishmentProviderError> {
    let expected_deposit = ZonePortal::depositCall {
        token: action.token,
        amount: action.amount,
        keyIndex: action.key_index,
        encrypted: action.encrypted.as_abi(),
        tempoRefundRecipient: action.refund_recipient,
    }
    .abi_encode();
    if action.job_id.is_zero()
        || action.portal != route.destination_portal
        || action.token != route.l1_token
        || action.amount == 0
        || U256::from(action.amount) > route.maximum_replenishment_amount
        || action.pool_recipient != route.destination_fast_transfer
        || action.refund_recipient != route.treasury
        || action.fee_payer != route.treasury_fee_payer
        || action.fee_token != route.treasury_fee_token
        || action.calldata.as_ref() != expected_deposit.as_slice()
        || action.intent_hash
            != transaction_intent_hash(
                route.l1_chain_id,
                action.portal,
                action.signer_nonce,
                action.fee_payer,
                action.fee_token,
                &action.calldata,
            )
    {
        return Err(ReplenishmentProviderError::PreparedActionMismatch);
    }
    Ok(())
}

fn validate_refund_claim_action(
    route: &ReplenishmentRouteConfig,
    action: &PreparedRefundClaimAction,
) -> Result<(), ReplenishmentProviderError> {
    let calldata = ZonePortal::claimRefundCall {
        token: action.token,
    }
    .abi_encode();
    if action.job_id.is_zero()
        || action.portal != route.destination_portal
        || action.token != route.l1_token
        || action.expected_amount == 0
        || action.fee_payer != route.treasury_fee_payer
        || action.fee_token != route.treasury_fee_token
        || action.calldata.as_ref() != calldata.as_slice()
        || action.intent_hash
            != transaction_intent_hash(
                route.l1_chain_id,
                action.portal,
                action.signer_nonce,
                action.fee_payer,
                action.fee_token,
                &action.calldata,
            )
    {
        return Err(ReplenishmentProviderError::PreparedActionMismatch);
    }
    Ok(())
}

fn validate_inventory_job(
    route: &ReplenishmentRouteConfig,
    action: &PreparedWithdrawalAction,
    job: &IFastTransfer::InventoryJob,
) -> Result<(), ReplenishmentProviderError> {
    if job.intentHash != action.native_job_intent_hash
        || job.operator != route.source_inventory
        || job.token != action.token
        || job.treasury != action.treasury
        || U256::from(job.amount) != action.gross_amount
        || job.fallbackNonce == 0
    {
        return Err(ReplenishmentProviderError::InvalidObservation);
    }
    Ok(())
}

async fn validate_withdrawal_request(
    provider: &DynProvider<TempoNetwork>,
    route: &ReplenishmentRouteConfig,
    action: &PreparedWithdrawalAction,
    fallback_nonce: u64,
    withdrawal_index: u64,
    block_number: u64,
) -> Result<TokenAmount, ReplenishmentProviderError> {
    let logs = provider
        .get_logs(
            &Filter::new()
                .address(ZONE_OUTBOX_ADDRESS)
                .event_signature(IZoneOutbox::WithdrawalRequested::SIGNATURE_HASH)
                .topic1(B256::from(U256::from(withdrawal_index)))
                .from_block(block_number)
                .to_block(block_number),
        )
        .await
        .map_err(provider_error)?;
    require_bounded_logs(&logs)?;
    for log in logs {
        let event = IZoneOutbox::WithdrawalRequested::decode_log(&log.inner)
            .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
            .data;
        if event.sender == route.source_inventory
            && event.token == action.token
            && event.to == action.treasury
            && U256::from(event.amount) == action.gross_amount
            && event.memo == action.job_id
            && event.gasLimit == 0
            && event.fallbackNonce == fallback_nonce
            && event.data.is_empty()
            && event.revealTo.is_empty()
        {
            if U256::from(event.fee) > route.maximum_source_withdrawal_fee {
                return Err(ReplenishmentProviderError::InvalidObservation);
            }
            return Ok(TokenAmount {
                token: action.token,
                amount: U256::from(event.fee),
            });
        }
    }
    Err(ReplenishmentProviderError::InvalidObservation)
}

fn require_bounded_logs(
    logs: &[alloy_rpc_types_eth::Log],
) -> Result<(), ReplenishmentProviderError> {
    if logs.len() > MAX_RECONCILIATION_LOGS {
        return Err(ReplenishmentProviderError::ReconciliationLimitExceeded);
    }
    Ok(())
}

fn decode_accepted_batch(
    log: &alloy_rpc_types_eth::Log,
) -> Result<(u64, u64), ReplenishmentProviderError> {
    let topic = log
        .inner
        .topics()
        .first()
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    let (batch, queue) = if topic == &BatchSubmitted::SIGNATURE_HASH {
        let event = BatchSubmitted::decode_log(&log.inner)
            .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
            .data;
        (event.withdrawalBatchIndex, event.withdrawalQueueIndex)
    } else if topic == &LegacyBatchSubmitted::SIGNATURE_HASH {
        let event = LegacyBatchSubmitted::decode_log(&log.inner)
            .map_err(|_| ReplenishmentProviderError::InvalidObservation)?
            .data;
        (event.withdrawalBatchIndex, event.withdrawalQueueIndex)
    } else {
        return Err(ReplenishmentProviderError::InvalidObservation);
    };
    let queue = u64::try_from(queue).map_err(|_| ReplenishmentProviderError::InvalidObservation)?;
    Ok((batch, queue))
}

async fn canonical_log_transaction(
    provider: &DynProvider<TempoNetwork>,
    log: &alloy_rpc_types_eth::Log,
) -> Result<B256, ReplenishmentProviderError> {
    canonical_log_transaction_cost(provider, log)
        .await
        .map(|(hash, _)| hash)
}

async fn canonical_log_transaction_cost(
    provider: &DynProvider<TempoNetwork>,
    log: &alloy_rpc_types_eth::Log,
) -> Result<(B256, TransactionCost), ReplenishmentProviderError> {
    if log.removed {
        return Err(ReplenishmentProviderError::InvalidObservation);
    }
    let transaction_hash = log
        .transaction_hash
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    let receipt = provider
        .get_transaction_receipt(transaction_hash)
        .await
        .map_err(provider_error)?
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    if !receipt.status() || receipt.block_hash != log.block_hash {
        return Err(ReplenishmentProviderError::InvalidObservation);
    }
    let amount = U256::from(receipt.gas_used())
        .checked_mul(U256::from(receipt.effective_gas_price()))
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    if receipt.fee_token.is_none() && !amount.is_zero() {
        return Err(ReplenishmentProviderError::InvalidObservation);
    }
    Ok((
        transaction_hash,
        TransactionCost {
            transaction_hash,
            payer: receipt.fee_payer,
            token: receipt.fee_token,
            amount,
        },
    ))
}

async fn require_canonical_token_transfer(
    provider: &DynProvider<TempoNetwork>,
    transaction_hash: B256,
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
) -> Result<(), ReplenishmentProviderError> {
    let receipt = provider
        .get_transaction_receipt(transaction_hash)
        .await
        .map_err(provider_error)?
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    let mut matches = 0usize;
    for log in receipt.logs() {
        if log.address() != token {
            continue;
        }
        let Ok(event) = ITIP20::Transfer::decode_log(&log.inner) else {
            continue;
        };
        if event.data.from == from && event.data.to == to && event.data.amount == amount {
            matches += 1;
        }
    }
    if matches != 1 {
        return Err(ReplenishmentProviderError::AmbiguousJobEvidence);
    }
    Ok(())
}

struct CanonicalActionIntent<'a> {
    from: Address,
    fee_payer: Address,
    fee_token: Address,
    to: Address,
    nonce: u64,
    calldata: &'a [u8],
}

async fn canonical_action_transaction(
    provider: &DynProvider<TempoNetwork>,
    log: &alloy_rpc_types_eth::Log,
    intent: CanonicalActionIntent<'_>,
) -> Result<(B256, TransactionCost), ReplenishmentProviderError> {
    let (transaction_hash, cost) = canonical_log_transaction_cost(provider, log).await?;
    let transaction = provider
        .get_transaction_by_hash(transaction_hash)
        .await
        .map_err(provider_error)?
        .ok_or(ReplenishmentProviderError::InvalidObservation)?;
    let observed_fee_payer = transaction
        .inner
        .fee_payer(transaction.from())
        .map_err(provider_error)?;
    if transaction.from() != intent.from
        || observed_fee_payer != intent.fee_payer
        || transaction.inner.fee_token() != Some(intent.fee_token)
        || transaction.nonce() != intent.nonce
        || transaction.kind() != TxKind::Call(intent.to)
        || transaction.input().as_ref() != intent.calldata
        || cost.payer != intent.fee_payer
        || cost.token != Some(intent.fee_token).filter(|_| !cost.amount.is_zero())
    {
        return Err(ReplenishmentProviderError::InvalidObservation);
    }
    Ok((transaction_hash, cost))
}

async fn submit_action(
    provider: &DynProvider<TempoNetwork>,
    from: Address,
    to: Address,
    nonce: u64,
    calldata: Bytes,
    fee_token: Address,
) -> Option<B256> {
    let request = TempoTransactionRequest {
        inner: TransactionRequest::default()
            .with_from(from)
            .with_to(to)
            .with_nonce(nonce)
            .with_input(calldata),
        fee_token: Some(fee_token),
        ..Default::default()
    };
    // An RPC error is intentionally ambiguous: the transaction may already be accepted by the
    // node or a same-nonce replacement. Canonical state/event reconciliation decides the leg.
    provider
        .send_transaction(request)
        .await
        .ok()
        .map(|pending| *pending.tx_hash())
}

fn inventory_job_intent_hash(
    job_id: B256,
    operator: Address,
    fee_payer: Address,
    token: Address,
    treasury: Address,
    transfer_ids: &[B256],
) -> B256 {
    let mut encoded = Vec::with_capacity(124 + transfer_ids.len() * 32);
    encoded.extend_from_slice(b"TEMPO_FAST_REPLENISHMENT_WITHDRAWAL_V1");
    encoded.extend_from_slice(job_id.as_slice());
    encoded.extend_from_slice(operator.as_slice());
    encoded.extend_from_slice(fee_payer.as_slice());
    encoded.extend_from_slice(token.as_slice());
    encoded.extend_from_slice(treasury.as_slice());
    encoded.extend_from_slice(&(transfer_ids.len() as u32).to_be_bytes());
    for transfer_id in transfer_ids {
        encoded.extend_from_slice(transfer_id.as_slice());
    }
    keccak256(encoded)
}

fn withdrawal_sender_tag(sender: Address, transaction_hash: B256, fallback_nonce: u64) -> B256 {
    let mut encoded = [0_u8; 60];
    encoded[..20].copy_from_slice(sender.as_slice());
    encoded[20..52].copy_from_slice(transaction_hash.as_slice());
    encoded[52..].copy_from_slice(&fallback_nonce.to_be_bytes());
    keccak256(encoded)
}

fn refund_matches_deposit(deposit_net: u128, refund: u128, bounceback_fee: u128) -> bool {
    refund.checked_add(bounceback_fee) == Some(deposit_net)
}

fn retain_unique_refund(
    slot: &mut Option<u128>,
    refund: u128,
) -> Result<(), ReplenishmentProviderError> {
    if slot.is_some() {
        return Err(ReplenishmentProviderError::AmbiguousJobEvidence);
    }
    *slot = Some(refund);
    Ok(())
}

fn transaction_intent_hash(
    chain_id: u64,
    to: Address,
    nonce: u64,
    fee_payer: Address,
    fee_token: Address,
    calldata: &[u8],
) -> B256 {
    let mut encoded = Vec::with_capacity(108 + calldata.len());
    encoded.extend_from_slice(b"TEMPO_REPLENISHMENT_TRANSACTION_V1");
    encoded.extend_from_slice(&chain_id.to_be_bytes());
    encoded.extend_from_slice(to.as_slice());
    encoded.extend_from_slice(&nonce.to_be_bytes());
    encoded.extend_from_slice(fee_payer.as_slice());
    encoded.extend_from_slice(fee_token.as_slice());
    encoded.extend_from_slice(&(calldata.len() as u32).to_be_bytes());
    encoded.extend_from_slice(calldata);
    keccak256(encoded)
}

fn worker_error(error: ReplenishmentProviderError) -> ReplenishmentWorkerError {
    ReplenishmentWorkerError::Bridge(error.to_string())
}

fn require_same_action(
    path: &Path,
    expected: &PreparedReplenishmentAction,
) -> Result<(), ReplenishmentProviderError> {
    let bytes = fs::read(path).map_err(storage_error)?;
    let stored =
        serde_json::from_slice::<PreparedReplenishmentAction>(&bytes).map_err(storage_error)?;
    if &stored != expected {
        return Err(ReplenishmentProviderError::PreparedActionMismatch);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ReplenishmentProviderError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(storage_error)
}

fn persist_nonce_state(
    path: &Path,
    state: &NoncePlannerState,
    create_new: bool,
) -> Result<(), ReplenishmentProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| ReplenishmentProviderError::Storage("nonce path has no parent".into()))?;
    let bytes = serde_json::to_vec(state).map_err(storage_error)?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(storage_error)?
        .as_nanos();
    let temporary = parent.join(format!(
        ".nonce-state.{}.{}.tmp",
        std::process::id(),
        unique
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(storage_error)?;
    file.write_all(&bytes).map_err(storage_error)?;
    file.sync_all().map_err(storage_error)?;
    drop(file);
    let result = if create_new {
        fs::hard_link(&temporary, path)
    } else {
        fs::rename(&temporary, path)
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        if create_new && error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(ReplenishmentProviderError::Storage(
                "nonce state was concurrently initialized".into(),
            ));
        }
        return Err(storage_error(error));
    }
    if create_new {
        fs::remove_file(&temporary).map_err(storage_error)?;
    }
    sync_directory(parent)
}

fn validate_nonce_state(state: &NoncePlannerState) -> Result<(), ReplenishmentProviderError> {
    if state.source_signer.is_zero() || state.treasury_signer.is_zero() {
        return Err(ReplenishmentProviderError::Storage(
            "nonce state has a zero signer".into(),
        ));
    }
    if state
        .source_jobs
        .values()
        .any(|nonce| *nonce >= state.next_source_nonce)
    {
        return Err(ReplenishmentProviderError::Storage(
            "source nonce state is not monotonic".into(),
        ));
    }
    let mut treasury_nonces = std::collections::BTreeSet::new();
    if state
        .treasury_deposit_jobs
        .values()
        .chain(state.treasury_refund_jobs.values())
        .any(|nonce| *nonce >= state.next_treasury_nonce || !treasury_nonces.insert(*nonce))
    {
        return Err(ReplenishmentProviderError::Storage(
            "treasury nonce state is not monotonic and unique".into(),
        ));
    }
    Ok(())
}

fn storage_error(error: impl std::fmt::Display) -> ReplenishmentProviderError {
    ReplenishmentProviderError::Storage(error.to_string())
}

fn provider_error(error: impl std::fmt::Display) -> ReplenishmentProviderError {
    ReplenishmentProviderError::Provider(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolValue;
    use tempfile::tempdir;

    fn test_route() -> ReplenishmentRouteConfig {
        ReplenishmentRouteConfig {
            l1_chain_id: 1,
            source_chain_id: 2,
            destination_chain_id: 3,
            protocol_version: 1,
            source_portal: Address::repeat_byte(1),
            destination_portal: Address::repeat_byte(2),
            source_fast_transfer: FAST_TRANSFER_ADDRESS,
            destination_fast_transfer: FAST_TRANSFER_ADDRESS,
            l1_token: Address::repeat_byte(3),
            source_token: Address::repeat_byte(3),
            destination_token: Address::repeat_byte(3),
            source_inventory: Address::repeat_byte(6),
            source_fallback: Address::repeat_byte(6),
            source_fee_payer: Address::repeat_byte(6),
            treasury_fee_payer: Address::repeat_byte(7),
            source_fee_token: Address::repeat_byte(12),
            treasury_fee_token: Address::repeat_byte(13),
            minimum_source_fee_reserve: U256::ONE,
            minimum_treasury_fee_reserve: U256::ONE,
            maximum_replenishment_amount: U256::from(1_000_000_u64),
            maximum_source_withdrawal_fee: U256::ONE,
            maximum_destination_deposit_fee: U256::ONE,
            treasury: Address::repeat_byte(7),
            destination_pool_operator: Address::repeat_byte(8),
        }
    }

    #[test]
    fn sender_tag_keeps_numeric_nonce_distinct_from_hash_identity() {
        let sender = Address::repeat_byte(1);
        let transaction_hash = B256::repeat_byte(2);
        let fallback_nonce = 0x0102_0304_0506_0708_u64;
        let expected = keccak256((sender, transaction_hash, fallback_nonce).abi_encode_packed());
        assert_eq!(
            withdrawal_sender_tag(sender, transaction_hash, fallback_nonce),
            expected
        );
        assert_ne!(expected, transaction_hash);
    }

    #[test]
    fn cross_job_refund_evidence_is_exact_and_ambiguous_matches_fail_closed() {
        assert!(refund_matches_deposit(100, 90, 10));
        assert!(!refund_matches_deposit(100, 90, 9));
        let mut match_for_job = None;
        retain_unique_refund(&mut match_for_job, 90).unwrap();
        assert_eq!(match_for_job, Some(90));
        assert!(matches!(
            retain_unique_refund(&mut match_for_job, 80),
            Err(ReplenishmentProviderError::AmbiguousJobEvidence)
        ));
    }

    #[test]
    fn transaction_intent_binds_nonce_target_and_calldata() {
        let base = transaction_intent_hash(
            1,
            Address::repeat_byte(2),
            3,
            Address::repeat_byte(7),
            Address::repeat_byte(8),
            &[4, 5],
        );
        assert_ne!(
            base,
            transaction_intent_hash(
                1,
                Address::repeat_byte(2),
                4,
                Address::repeat_byte(7),
                Address::repeat_byte(8),
                &[4, 5]
            )
        );
        assert_ne!(
            base,
            transaction_intent_hash(
                1,
                Address::repeat_byte(3),
                3,
                Address::repeat_byte(7),
                Address::repeat_byte(8),
                &[4, 5]
            )
        );
        assert_ne!(
            base,
            transaction_intent_hash(
                1,
                Address::repeat_byte(2),
                3,
                Address::repeat_byte(7),
                Address::repeat_byte(8),
                &[4, 6]
            )
        );
        assert_ne!(
            base,
            transaction_intent_hash(
                1,
                Address::repeat_byte(2),
                3,
                Address::repeat_byte(8),
                Address::repeat_byte(8),
                &[4, 5]
            )
        );
        assert_ne!(
            base,
            transaction_intent_hash(
                1,
                Address::repeat_byte(2),
                3,
                Address::repeat_byte(7),
                Address::repeat_byte(9),
                &[4, 5]
            )
        );
    }

    #[test]
    fn local_route_validation_requires_native_endpoints() {
        let mut route = test_route();
        assert!(route.validate_local().is_ok());
        route.minimum_source_fee_reserve = U256::ZERO;
        route.minimum_treasury_fee_reserve = U256::ZERO;
        route.maximum_source_withdrawal_fee = U256::ZERO;
        route.maximum_destination_deposit_fee = U256::ZERO;
        assert!(route.validate_local().is_ok());
        route.destination_fast_transfer = Address::repeat_byte(9);
        assert!(matches!(
            route.validate_local(),
            Err(ReplenishmentProviderError::InvalidRoute)
        ));
    }

    #[test]
    fn failed_policy_preflight_leaves_inventory_as_queued_liability() {
        let route = test_route();
        let snapshot = FinalizedRouteSnapshot {
            l1_block_number: 10,
            l1_block_hash: B256::repeat_byte(1),
            source_block_number: 11,
            destination_block_number: 12,
            l1_chain_id: route.l1_chain_id,
            source_chain_id: route.source_chain_id,
            destination_chain_id: route.destination_chain_id,
            source_portal: route.source_portal,
            destination_portal: route.destination_portal,
            l1_token: route.l1_token,
            source_token: route.source_token,
            destination_token: route.destination_token,
            source_inventory: route.source_inventory,
            source_fallback: route.source_fallback,
            treasury: route.treasury,
            destination_pool_operator: route.destination_pool_operator,
            source_fast_epoch: 1,
            destination_fast_epoch: 1,
            source_protocol_version: route.protocol_version,
            destination_protocol_version: route.protocol_version,
            source_epoch_open: true,
            destination_epoch_open: true,
            l1_token_decimals: 6,
            source_token_decimals: 6,
            destination_token_decimals: 6,
            destination_key_index: U256::ONE,
            destination_key_x: B256::repeat_byte(2),
            destination_key_y_parity: 2,
            source_inventory_allowed: true,
            source_fallback_allowed: false,
            source_fee_payer_allowed: true,
            treasury_allowed_on_source: true,
            treasury_allowed_on_destination: true,
            source_token_enabled: true,
            destination_token_enabled: true,
            destination_deposits_active: true,
            source_outbox_allowance: route.maximum_replenishment_amount
                + route.maximum_source_withdrawal_fee,
            source_fee_payer_outbox_allowance: route.maximum_source_withdrawal_fee,
            destination_portal_allowance: route.maximum_replenishment_amount,
            source_fee_reserve: route.minimum_source_fee_reserve,
            treasury_fee_reserve: route.minimum_treasury_fee_reserve,
            destination_pool_initialized: true,
            destination_replenishment_route_configured: true,
        };
        let job = ReplenishmentJob::allocate(
            B256::repeat_byte(3),
            route.source_inventory,
            route.source_fallback,
            route.treasury,
            route.destination_pool_operator,
            Address::repeat_byte(9),
            vec![zone_fast_transfer::replenishment::InventoryContribution {
                transfer_id: B256::repeat_byte(4),
                amount: U256::ONE,
            }],
        )
        .unwrap();
        assert!(matches!(
            snapshot.validate_against(&route),
            Err(ReplenishmentProviderError::InvalidFinalizedRegistry)
        ));
        assert_eq!(
            job.stage,
            zone_fast_transfer::ReplenishmentStage::InventoryAllocated
        );
        assert!(job.withdrawal.is_none());
    }

    #[test]
    fn file_action_store_is_immutable_and_recovers_exact_calldata() {
        let directory = tempdir().unwrap();
        let store = FilePreparedActionStore::open(directory.path()).unwrap();
        let action = PreparedReplenishmentAction::Withdrawal(PreparedWithdrawalAction {
            job_id: B256::repeat_byte(1),
            transfer_ids: vec![B256::repeat_byte(2)],
            token: Address::repeat_byte(3),
            treasury: Address::repeat_byte(4),
            gross_amount: U256::from(5),
            not_before_source_block: 1,
            not_before_l1_block: 2,
            signer_nonce: 6,
            fee_payer: Address::repeat_byte(7),
            fee_token: Address::repeat_byte(12),
            calldata: Bytes::from_static(&[9, 10]),
            native_job_intent_hash: B256::repeat_byte(8),
            intent_hash: B256::repeat_byte(11),
        });
        store.put(action.clone()).unwrap();
        store.put(action.clone()).unwrap();
        assert_eq!(store.get(action.intent_hash()).unwrap(), action);

        let mut conflict = action.clone();
        let PreparedReplenishmentAction::Withdrawal(conflict) = &mut conflict else {
            unreachable!()
        };
        conflict.signer_nonce += 1;
        assert!(matches!(
            store.put(PreparedReplenishmentAction::Withdrawal(conflict.clone())),
            Err(ReplenishmentProviderError::PreparedActionMismatch)
        ));
    }

    #[test]
    fn nonce_planner_fsyncs_stable_economic_action_reservations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nonces.json");
        let initial = NoncePlannerState {
            source_signer: Address::repeat_byte(1),
            treasury_signer: Address::repeat_byte(2),
            next_source_nonce: 7,
            next_treasury_nonce: 11,
            source_jobs: BTreeMap::new(),
            treasury_deposit_jobs: BTreeMap::new(),
            treasury_refund_jobs: BTreeMap::new(),
        };
        persist_nonce_state(&path, &initial, true).unwrap();
        let planner = FileReplenishmentNoncePlanner {
            path: path.clone(),
            state: Mutex::new(initial),
        };
        let first = B256::repeat_byte(3);
        let second = B256::repeat_byte(4);
        assert_eq!(planner.source_signer_nonce(first).unwrap(), 7);
        assert_eq!(planner.source_signer_nonce(first).unwrap(), 7);
        assert_eq!(planner.treasury_signer_nonce(first).unwrap(), 11);
        assert_eq!(planner.treasury_signer_nonce(second).unwrap(), 12);
        assert_eq!(
            planner
                .treasury_refund_claim_nonce_if_reserved(first)
                .unwrap(),
            None
        );
        assert_eq!(planner.treasury_refund_claim_nonce(first).unwrap(), 13);

        let recovered: NoncePlannerState =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(recovered.source_jobs[&first], 7);
        assert_eq!(recovered.treasury_deposit_jobs[&first], 11);
        assert_eq!(recovered.treasury_deposit_jobs[&second], 12);
        assert_eq!(recovered.treasury_refund_jobs[&first], 13);
    }
}
