//! ABI dispatch helpers for zone precompiles.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileHalt, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    IntoPrecompileResult, Result, error::TempoPrecompileError, input_cost, storage::StorageCtx,
};

alloy_sol_types::sol! {
    error StaticCallNotAllowed();
}

/// Dispatches a read-only storage-backed call with decoded arguments.
#[inline]
#[allow(dead_code)]
pub(crate) fn view<T: SolCall>(
    call: T,
    f: impl FnOnce(T) -> Result<T::Return>,
) -> PrecompileResult {
    f(call).into_precompile_result(0, 0, |ret| T::abi_encode_returns(&ret).into())
}

/// Dispatches a state-mutating storage-backed call that returns ABI-encoded data.
///
/// Rejects static calls with [`StaticCallNotAllowed`].
#[inline]
#[allow(dead_code)]
pub(crate) fn mutate<T: SolCall>(
    call: T,
    sender: Address,
    f: impl FnOnce(Address, T) -> Result<T::Return>,
) -> PrecompileResult {
    if StorageCtx.is_static() {
        return Ok(PrecompileOutput::revert(
            0,
            StaticCallNotAllowed {}.abi_encode().into(),
            StorageCtx.reservoir(),
        ));
    }
    f(sender, call).into_precompile_result(0, 0, |ret| T::abi_encode_returns(&ret).into())
}

/// Dispatches a state-mutating storage-backed call that returns no data.
///
/// Rejects static calls with [`StaticCallNotAllowed`].
#[inline]
pub(crate) fn mutate_void<T: SolCall>(
    call: T,
    sender: Address,
    f: impl FnOnce(Address, T) -> Result<()>,
) -> PrecompileResult {
    if StorageCtx.is_static() {
        return Ok(PrecompileOutput::revert(
            0,
            StaticCallNotAllowed {}.abi_encode().into(),
            StorageCtx.reservoir(),
        ));
    }
    f(sender, call).into_precompile_result(0, 0, |()| Bytes::new())
}

/// Deducts the calldata input cost, returning an OOG halt result if insufficient gas.
#[inline]
pub(crate) fn charge_input_cost(
    storage: &mut StorageCtx,
    calldata: &[u8],
) -> Option<PrecompileResult> {
    if storage.deduct_gas(input_cost(calldata.len())).is_err() {
        return Some(Ok(storage.halt_output(PrecompileHalt::OutOfGas)));
    }
    None
}

#[inline]
fn fill_state_gas(output: &mut PrecompileOutput, storage: &StorageCtx) {
    if storage.spec().is_t4() && output.is_success() {
        output.gas_refunded = storage.gas_refunded();
    }

    if storage.amsterdam_eip8037_enabled() {
        if output.is_success() {
            output.reservoir = storage.reservoir();
            output.state_gas_used = storage.state_gas_used();
        } else {
            output.reservoir = storage.state_gas_used() + storage.reservoir();
            output.state_gas_used = 0;
        }
    }
}

/// Decodes calldata for a storage-backed precompile, then dispatches to `f`.
#[inline]
pub(crate) fn dispatch_call<T>(
    calldata: &[u8],
    decode: impl FnOnce(&[u8]) -> core::result::Result<T, alloy_sol_types::Error>,
    f: impl FnOnce(T) -> PrecompileResult,
) -> PrecompileResult {
    let storage = StorageCtx::default();

    if calldata.len() < 4 {
        return missing_selector_result();
    }

    match decode(calldata) {
        Ok(call) => f(call).map(|mut res| {
            res.gas_used = storage.gas_used();
            fill_state_gas(&mut res, &storage);
            res
        }),
        Err(alloy_sol_types::Error::UnknownSelector { selector, .. }) => {
            storage.error_result(TempoPrecompileError::UnknownFunctionSelector(*selector))
        }
        Err(_) => Ok(storage.revert_output(Bytes::new())),
    }
}

/// Decodes calldata for a stateless precompile, then dispatches to `f`.
#[inline]
pub(crate) fn dispatch_stateless_call<T>(
    calldata: &[u8],
    reservoir: u64,
    decode: impl FnOnce(&[u8]) -> core::result::Result<T, alloy_sol_types::Error>,
    f: impl FnOnce(T) -> PrecompileResult,
) -> PrecompileResult {
    if calldata.len() < 4 {
        return missing_selector_stateless_result(reservoir);
    }

    match decode(calldata) {
        Ok(call) => f(call),
        Err(_) => Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir)),
    }
}

macro_rules! dispatch {
    ($calldata:expr, |$call:ident| match $match_call:ident {
        $($iface:ident::$calls:ident {
            $(
                $(#[schedule($($gate:ident = $hf:ident),+ $(,)?)])*
                $variant:ident($binding:pat) => $body:expr
            ),* $(,)?
        })+
    } $(,)?) => {
        paste::paste! {{
            #[cfg(debug_assertions)]
            {
                let mut selectors = ::alloc::collections::BTreeSet::new();
                $(assert!(
                    <$iface::$calls as alloy_sol_types::SolInterface>::selectors().all(|s| selectors.insert(s)),
                    "duplicate precompile selector in dispatch! macro",
                );)*
            }

            if let Some(selector) = crate::dispatch::selector_from_calldata($calldata) {
                $($($($(
                    if selector == <$iface::[<$variant Call>] as alloy_sol_types::SolCall>::SELECTOR
                        && !crate::dispatch::$gate(tempo_chainspec::hardfork::TempoHardfork::$hf)
                    {
                        return crate::dispatch::unknown_selector_result($calldata);
                    }
                )+)*)*)+
                $(
                    if <$iface::$calls as alloy_sol_types::SolInterface>::valid_selector(selector) {
                        type Calls = $iface::$calls;
                        return crate::dispatch::dispatch_call($calldata, <Calls as alloy_sol_types::SolInterface>::abi_decode_validate, |$call| match $match_call {
                            $(Calls::$variant($binding) => $body,)*
                        });
                    }
                )*
                return crate::dispatch::unknown_selector_result($calldata);
            }
            crate::dispatch::missing_selector_result()
        }}
    };

    ($calldata:expr, $reservoir:expr, |$call:ident| match $match_call:ident {
        $($iface:ident::$calls:ident {
            $(
                $variant:ident($binding:pat) => $body:expr
            ),* $(,)?
        })+
    } $(,)?) => {
        paste::paste! {{
            #[cfg(debug_assertions)]
            {
                let mut selectors = ::alloc::collections::BTreeSet::new();
                $(assert!(
                    <$iface::$calls as alloy_sol_types::SolInterface>::selectors().all(|s| selectors.insert(s)),
                    "duplicate precompile selector in dispatch! macro",
                );)*
            }

            if let Some(selector) = crate::dispatch::selector_from_calldata($calldata) {
                $(
                    if <$iface::$calls as alloy_sol_types::SolInterface>::valid_selector(selector) {
                        type Calls = $iface::$calls;
                        return crate::dispatch::dispatch_stateless_call(
                            $calldata,
                            $reservoir,
                            <Calls as alloy_sol_types::SolInterface>::abi_decode,
                            |$call| match $match_call {
                                $(Calls::$variant($binding) => $body,)*
                            },
                        );
                    }
                )*
                return crate::dispatch::unknown_selector_stateless_result($reservoir);
            }
            crate::dispatch::missing_selector_stateless_result($reservoir)
        }}
    };
}

pub(crate) use dispatch;

pub(crate) fn selector_from_calldata(calldata: &[u8]) -> Option<[u8; 4]> {
    calldata.get(..4).map(|selector| {
        selector
            .try_into()
            .expect("selector slice has exactly 4 bytes")
    })
}

pub(crate) fn missing_selector_result() -> PrecompileResult {
    let storage = StorageCtx::default();

    if storage.spec().is_t1() {
        Ok(storage.revert_output(Bytes::new()))
    } else {
        Ok(storage.halt_output(PrecompileHalt::Other(
            "Invalid input: missing function selector".into(),
        )))
    }
}

#[inline]
#[allow(dead_code)]
pub(crate) fn since(hardfork: tempo_chainspec::hardfork::TempoHardfork) -> bool {
    StorageCtx.spec() >= hardfork
}

#[inline]
#[allow(dead_code)]
pub(crate) fn until(hardfork: tempo_chainspec::hardfork::TempoHardfork) -> bool {
    StorageCtx.spec() < hardfork
}

pub(crate) fn unknown_selector_result(calldata: &[u8]) -> PrecompileResult {
    let selector = selector_from_calldata(calldata).expect("calldata len >= 4 after decode");
    StorageCtx::default().error_result(TempoPrecompileError::UnknownFunctionSelector(selector))
}

pub(crate) fn missing_selector_stateless_result(reservoir: u64) -> PrecompileResult {
    Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir))
}

pub(crate) fn unknown_selector_stateless_result(reservoir: u64) -> PrecompileResult {
    Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir))
}
