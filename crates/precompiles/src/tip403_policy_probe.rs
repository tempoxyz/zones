//! Test-only TIP-403 authorization probe.
//!
//! Role values map to `AuthRole::{Transfer, Sender, Recipient, MintRecipient}` in that order.
//! `caller` is explicit so tests do not accidentally derive policy subjects from `msg.sender`;
//! it is currently informational because TIP-403 authorization itself depends only on the account.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, address};
use alloy_sol_types::{SolError, SolInterface};
use tempo_precompiles::{
    dispatch::{dispatch_call, typed::view},
    tip20::TIP20Token,
    tip403_registry::{AuthRole, TIP403Registry},
};

use crate::{ZonePrecompileEnv, execution};

/// Deliberately high, test-only address outside every production precompile namespace.
pub const TIP403_POLICY_PROBE_ADDRESS: Address =
    address!("0xffffffffffffffffffffffffffffffffffffff03");

alloy_sol_types::sol! {
    error InvalidRole();

    /// Test-only interface for querying the Rust TIP-403 implementation.
    interface ITIP403PolicyProbe {
        #[derive(PartialEq, Eq)]
        enum Role { Transfer, Sender, Recipient, MintRecipient }
        function isAuthorized(address token, address account, address caller, Role role)
            external view returns (bool);
    }
}

/// Creates a probe over the same transaction-local storage context as production precompiles.
pub fn create_tip403_policy_probe(env: &ZonePrecompileEnv) -> DynPrecompile {
    execution::create_precompile(
        "Tip403PolicyProbe",
        env,
        execution::NoCallRules,
        |data, _msg_sender| {
            dispatch_call(
                data,
                ITIP403PolicyProbe::ITIP403PolicyProbeCalls::abi_decode,
                |call| match call {
                    ITIP403PolicyProbe::ITIP403PolicyProbeCalls::isAuthorized(call) => {
                        let role = match call.role {
                            ITIP403PolicyProbe::Role::Transfer => AuthRole::Transfer,
                            ITIP403PolicyProbe::Role::Sender => AuthRole::Sender,
                            ITIP403PolicyProbe::Role::Recipient => AuthRole::Recipient,
                            ITIP403PolicyProbe::Role::MintRecipient => AuthRole::MintRecipient,
                            ITIP403PolicyProbe::Role::__Invalid => {
                                return Ok(tempo_precompiles::storage::StorageCtx::default()
                                    .revert_output(InvalidRole {}.abi_encode().into()));
                            }
                        };
                        view(call, |call| {
                            let policy_id = TIP20Token::from_address_unchecked(call.token)
                                .transfer_policy_id()?;
                            TIP403Registry::new().is_authorized_as(policy_id, call.account, role)
                        })
                    }
                },
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256, address, keccak256};
    use alloy_sol_types::{SolCall, SolInterface, SolValue};
    use revm::precompile::PrecompileOutput;
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::{
        input_cost,
        storage::PrecompileStorageProvider,
        tip403_registry::{ALLOW_ALL_POLICY_ID, tip403_registry_slots},
    };

    use crate::test_utils::{
        TestContext, call_precompile, test_context, test_env, test_storage_provider,
    };

    const TOKEN: Address = address!("0x20c0000000000000000000000000000000000042");
    const ACCOUNT: Address = address!("0x0000000000000000000000000000000000000aaa");
    fn call(role: ITIP403PolicyProbe::Role, policy_id: u64, gas: u64) -> PrecompileOutput {
        let mut ctx: TestContext = test_context();
        ctx.cfg.spec = TempoHardfork::T8;
        let slot = keccak256((TOKEN, tip403_registry_slots::TOKEN_TRANSFER_POLICIES).abi_encode());
        let packed = U256::from(policy_id) | (U256::ONE << 64);
        test_storage_provider(&mut ctx, u64::MAX, false)
            .sstore(
                tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS,
                U256::from_be_bytes(B256::from(slot).0),
                packed,
            )
            .unwrap();
        let env = test_env(&ctx);
        let probe = create_tip403_policy_probe(&env);
        let data = ITIP403PolicyProbe::isAuthorizedCall {
            token: TOKEN,
            account: ACCOUNT,
            caller: ACCOUNT,
            role,
        }
        .abi_encode();
        call_precompile(
            &mut ctx,
            &probe,
            ACCOUNT,
            &data,
            gas,
            true,
            TIP403_POLICY_PROBE_ADDRESS,
            TIP403_POLICY_PROBE_ADDRESS,
        )
        .unwrap()
    }

    #[test]
    fn decodes_every_role_and_reports_policy_denial() {
        for role in [
            ITIP403PolicyProbe::Role::Transfer,
            ITIP403PolicyProbe::Role::Sender,
            ITIP403PolicyProbe::Role::Recipient,
            ITIP403PolicyProbe::Role::MintRecipient,
        ] {
            assert!(!bool::abi_decode(&call(role, 0, u64::MAX).bytes).unwrap());
        }
    }

    #[test]
    fn rejects_malformed_and_invalid_role_calldata() {
        assert!(ITIP403PolicyProbe::ITIP403PolicyProbeCalls::abi_decode(&[1, 2, 3]).is_err());
        let mut data = ITIP403PolicyProbe::isAuthorizedCall {
            token: TOKEN,
            account: ACCOUNT,
            caller: ACCOUNT,
            role: ITIP403PolicyProbe::Role::Transfer,
        }
        .abi_encode();
        *data.last_mut().unwrap() = 4;
        assert!(matches!(
            ITIP403PolicyProbe::ITIP403PolicyProbeCalls::abi_decode(&data),
            Ok(ITIP403PolicyProbe::ITIP403PolicyProbeCalls::isAuthorized(call))
                if call.role == ITIP403PolicyProbe::Role::__Invalid
        ));
    }

    #[test]
    fn charges_input_and_storage_gas() {
        let output = call(
            ITIP403PolicyProbe::Role::Transfer,
            ALLOW_ALL_POLICY_ID,
            u64::MAX,
        );
        assert!(output.gas_used >= input_cost(4 + 32 * 4));
    }
}
