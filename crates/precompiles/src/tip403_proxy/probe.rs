//! Test-only TIP-403 authorization probe.
//!
//! Role values map to `AuthRole::{Transfer, Sender, Recipient, MintRecipient}` in that order.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, address};
use alloy_sol_types::SolInterface;
use tempo_contracts::precompiles::TIP403RegistryError;
use tempo_precompiles::{
    dispatch::{dispatch_call, typed::view},
    tip20::TIP20Token,
    tip403_registry::{AuthRole, TIP403Registry},
};

use crate::{ZonePrecompileEnv, execution};

pub const TIP403_PROBE_ADDRESS: Address = address!("0x403Cffffffffffffffffffffffffffffffffffff");

alloy_sol_types::sol! {
    /// Test-only interface for querying the Rust TIP-403 implementation.
    interface ITIP403Probe {
        function isAuthorized(address token, address account, uint8 role) external view returns (bool);
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
                ITIP403Probe::ITIP403ProbeCalls::abi_decode,
                |call| match call {
                    ITIP403Probe::ITIP403ProbeCalls::isAuthorized(call) => view(call, |call| {
                        let role = match call.role {
                            0 => AuthRole::Transfer,
                            1 => AuthRole::Sender,
                            2 => AuthRole::Recipient,
                            3 => AuthRole::MintRecipient,
                            _ => {
                                return Err(TIP403RegistryError::incompatible_policy_type().into());
                            }
                        };
                        let id = TIP20Token::from_address(call.token)?.transfer_policy_id()?;
                        TIP403Registry::new().is_authorized_as(id, call.account, role)
                    }),
                },
            )
        },
    )
}
