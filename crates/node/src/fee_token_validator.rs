//! Zone-specific fee-token admission for the shared Tempo transaction pool.

use alloy_primitives::Address;
use reth_storage_api::StateProviderFactory;
use revm::precompile::PrecompileError;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_revm::{TempoInvalidTransaction, error::FeePaymentError};
use tempo_transaction_pool::validator::{
    FeeTokenSettlement, FeeTokenValidationError, FeeTokenValidator,
};
use tempo_zone_contracts::ZONE_FEE_MANAGER_ADDRESS;
use zone_l1::TempoStateExt;
use zone_precompiles::{ZoneConfigReader, ZoneTip403ProxyRegistry, policy::PolicyCheck};
use zone_primitives::policy::AuthRole;

/// Validates resolved fee tokens against the portal and L1 TIP-403 policy.
#[derive(Clone)]
pub(crate) struct ZoneFeeTokenValidator<Client, L1, Policy> {
    client: Client,
    l1_reader: L1,
    registry: ZoneTip403ProxyRegistry<Policy>,
}

impl<Client, L1, Policy> core::fmt::Debug for ZoneFeeTokenValidator<Client, L1, Policy> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ZoneFeeTokenValidator")
            .finish_non_exhaustive()
    }
}

impl<Client, L1, Policy> ZoneFeeTokenValidator<Client, L1, Policy> {
    pub(crate) const fn new(
        client: Client,
        l1_reader: L1,
        registry: ZoneTip403ProxyRegistry<Policy>,
    ) -> Self {
        Self {
            client,
            l1_reader,
            registry,
        }
    }
}

impl<Client, L1, Policy> ZoneFeeTokenValidator<Client, L1, Policy>
where
    L1: ZoneConfigReader,
    Policy: PolicyCheck,
{
    fn validate_at_block(
        &self,
        fee_payer: Address,
        fee_token: Address,
        spec: TempoHardfork,
        block_number: u64,
    ) -> Result<FeeTokenSettlement, FeeTokenValidationError> {
        let enabled = self
            .l1_reader
            .is_enabled_token(fee_token, block_number)
            .map_err(Self::provider_error)?;
        if !enabled {
            return Err(FeeTokenValidationError::Invalid(
                TempoInvalidTransaction::InvalidFeeToken(fee_token),
            ));
        }

        let policy_id = self
            .registry
            .resolve_transfer_policy_id(fee_token)
            .map_err(Self::provider_error)?;
        let authorized = if spec.is_t8() {
            self.registry
                .is_authorized(policy_id, fee_payer, AuthRole::Sender)
                .map_err(Self::provider_error)?
        } else {
            self.registry
                .is_transfer_authorized(policy_id, fee_payer, ZONE_FEE_MANAGER_ADDRESS)
                .map_err(Self::provider_error)?
        };
        if !authorized {
            return Err(FeeTokenValidationError::Invalid(
                TempoInvalidTransaction::CollectFeePreTx(FeePaymentError::Other(
                    "TIP-403 policy forbids fee transfer".into(),
                )),
            ));
        }

        Ok(FeeTokenSettlement::Direct)
    }

    fn provider_error(error: PrecompileError) -> FeeTokenValidationError {
        FeeTokenValidationError::Other(Box::new(error))
    }
}

impl<Client, L1, Policy> FeeTokenValidator for ZoneFeeTokenValidator<Client, L1, Policy>
where
    Client: StateProviderFactory + Send + Sync,
    L1: ZoneConfigReader,
    Policy: PolicyCheck + Send + Sync,
{
    fn validate_fee_token(
        &self,
        fee_payer: Address,
        fee_token: Address,
        spec: TempoHardfork,
    ) -> Result<FeeTokenSettlement, FeeTokenValidationError> {
        let state = self
            .client
            .latest()
            .map_err(|error| FeeTokenValidationError::Other(Box::new(error)))?;
        let block_number = state
            .tempo_block_number()
            .map_err(|error| FeeTokenValidationError::Other(Box::new(error)))?;
        self.validate_at_block(fee_payer, fee_token, spec, block_number)
    }

    fn uses_fee_amm(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use tempo_contracts::precompiles::ITIP403Registry::PolicyType;
    use zone_precompiles::L1StorageReader;

    use super::*;

    #[derive(Clone)]
    struct MockL1 {
        enabled: bool,
    }

    impl L1StorageReader for MockL1 {
        fn read_l1_storage(
            &self,
            _account: Address,
            _slot: B256,
            _block_number: u64,
        ) -> Result<B256, PrecompileError> {
            Ok(B256::with_last_byte(u8::from(self.enabled)))
        }
    }

    impl ZoneConfigReader for MockL1 {
        fn zone_portal_address(&self) -> Address {
            Address::random()
        }
    }

    #[derive(Clone)]
    struct MockPolicy {
        sender: bool,
        recipient: bool,
    }

    impl PolicyCheck for MockPolicy {
        fn is_authorized(
            &self,
            _policy_id: u64,
            _user: Address,
            role: AuthRole,
        ) -> Result<bool, PrecompileError> {
            Ok(match role {
                AuthRole::Transfer => self.sender && self.recipient,
                AuthRole::Sender => self.sender,
                AuthRole::Recipient => self.recipient,
                AuthRole::MintRecipient => false,
            })
        }

        fn resolve_transfer_policy_id(&self, _token: Address) -> Result<u64, PrecompileError> {
            Ok(1)
        }

        fn policy_type_sync(&self, _policy_id: u64) -> Result<PolicyType, PrecompileError> {
            unreachable!("not used by fee-token admission")
        }

        fn compound_policy_data(
            &self,
            _policy_id: u64,
        ) -> Result<(u64, u64, u64), PrecompileError> {
            unreachable!("not used by fee-token admission")
        }

        fn policy_exists(&self, _policy_id: u64) -> Result<bool, PrecompileError> {
            unreachable!("not used by fee-token admission")
        }

        fn policy_id_counter(&self) -> u64 {
            1
        }
    }

    fn validator(
        enabled: bool,
        sender: bool,
        recipient: bool,
    ) -> ZoneFeeTokenValidator<(), MockL1, MockPolicy> {
        let policy = MockPolicy { sender, recipient };
        ZoneFeeTokenValidator::new((), MockL1 { enabled }, ZoneTip403ProxyRegistry::new(policy))
    }

    #[test]
    fn enabled_authorized_tokens_settle_directly() {
        let result = validator(true, true, true).validate_at_block(
            Address::random(),
            Address::random(),
            TempoHardfork::T7,
            42,
        );
        assert!(matches!(result, Ok(FeeTokenSettlement::Direct)));
    }

    #[test]
    fn disabled_tokens_are_rejected() {
        let token = Address::random();
        let result = validator(false, true, true).validate_at_block(
            Address::random(),
            token,
            TempoHardfork::T7,
            42,
        );
        assert!(matches!(
            result,
            Err(FeeTokenValidationError::Invalid(
                TempoInvalidTransaction::InvalidFeeToken(rejected)
            )) if rejected == token
        ));
    }

    #[test]
    fn t8_exempts_fee_manager_recipient_authorization() {
        let fee_payer = Address::random();
        let token = Address::random();
        let validator = validator(true, true, false);

        assert!(matches!(
            validator.validate_at_block(fee_payer, token, TempoHardfork::T7, 42),
            Err(FeeTokenValidationError::Invalid(
                TempoInvalidTransaction::CollectFeePreTx(_)
            ))
        ));
        assert!(matches!(
            validator.validate_at_block(fee_payer, token, TempoHardfork::T8, 42),
            Ok(FeeTokenSettlement::Direct)
        ));
    }
}
