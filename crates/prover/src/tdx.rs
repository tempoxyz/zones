//! Experimental TDX quote-v4 profile and local Intel DCAP integration.
//!
//! DCAP's local QVL may fetch collateral. This module is not a consensus verifier and must not
//! be called from an L1 precompile. Measurement policies come from an independently approved VM.

use std::io;

use alloy_primitives::{B256, FixedBytes};
use serde::Deserialize;
use zone_spf::{BatchOutput, PublicInputs};

use crate::{ProofBundle, TDX_VERIFIER_CONFIG_V1, tdx_batch_attestation_hash};

/// Bounds quote allocations before invoking the native verifier.
pub const MAX_QUOTE_BYTES: usize = 64 * 1024;
const HEADER_BYTES: usize = 48;
const BODY_BYTES: usize = 584;
const SIGNATURE_OFFSET: usize = HEADER_BYTES + BODY_BYTES;

/// Exact deployment identity, including boot/runtime measurements and owner configuration.
/// Approve complete tuples rather than independently mixing measurements from multiple builds.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Measurements {
    pub mr_td: FixedBytes<48>,
    pub mr_config_id: FixedBytes<48>,
    pub mr_owner: FixedBytes<48>,
    pub mr_owner_config: FixedBytes<48>,
    pub rtmrs: [FixedBytes<48>; 4],
    pub td_attributes: u64,
    pub xfam: u64,
}

/// Local verification policy. Quotes with debug enabled are always rejected.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub measurements: Vec<Measurements>,
}

impl Policy {
    pub fn validate(&self) -> io::Result<()> {
        require(
            !self.measurements.is_empty(),
            "empty TDX measurement policy",
        )?;
        for m in &self.measurements {
            require(!m.mr_td.is_zero(), "zero TDX build measurement")?;
            require(m.td_attributes & 1 == 0, "debug TDX policy is forbidden")?;
        }
        Ok(())
    }

    /// Verify signature/collateral with DCAP, then apply the pinned identity and report binding.
    pub fn verify(&self, quote: &[u8], report_data: &[u8; 64], now: u64) -> io::Result<()> {
        self.validate()?;
        let parsed = Quote::parse(quote)?;
        native::verify(quote, now)?;
        self.verify_claims(&parsed, report_data)
    }

    fn verify_claims(&self, quote: &Quote, report_data: &[u8; 64]) -> io::Result<()> {
        require(quote.measurements.td_attributes & 1 == 0, "debug TDX quote")?;
        require(
            self.measurements.contains(&quote.measurements),
            "TDX measurement mismatch",
        )?;
        require(
            &quote.report_data == report_data,
            "TDX report data mismatch",
        )
    }

    /// Verify a replay output's experimental TDX proof independently of L1 activation.
    pub fn verify_batch(
        &self,
        bundle: &ProofBundle,
        inputs: &PublicInputs,
        output: &BatchOutput,
        now: u64,
    ) -> io::Result<()> {
        require(
            bundle.verifier_config.as_ref() == TDX_VERIFIER_CONFIG_V1,
            "not a TDX proof",
        )?;
        self.verify(
            &bundle.proof,
            &batch_report_data(tdx_batch_attestation_hash(inputs, output)),
            now,
        )
    }
}

/// A batch commits its digest in the first half and requires a zero second half.
pub fn batch_report_data(digest: B256) -> [u8; 64] {
    let mut data = [0; 64];
    data[..32].copy_from_slice(digest.as_slice());
    data
}

/// TLS report data binds the certificate digest and a fresh client challenge separately.
pub fn transport_report_data(binding: &[u8], nonce: &[u8]) -> io::Result<[u8; 64]> {
    require(
        binding.len() == 32 && nonce.len() == 32,
        "invalid TDX TLS binding",
    )?;
    let mut data = [0; 64];
    data[..32].copy_from_slice(binding);
    data[32..].copy_from_slice(nonce);
    Ok(data)
}

/// Parsed claims are untrusted until DCAP verification succeeds.
struct Quote {
    measurements: Measurements,
    report_data: [u8; 64],
}

impl Quote {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        require(
            bytes.len() >= SIGNATURE_OFFSET + 4 && bytes.len() <= MAX_QUOTE_BYTES,
            "invalid TDX quote size",
        )?;
        require(
            bytes[..2] == 4u16.to_le_bytes(),
            "only TDX quote v4 is supported",
        )?;
        require(
            bytes[2..4] == 2u16.to_le_bytes(),
            "TDX quote must use ECDSA P-256",
        )?;
        require(bytes[4..8] == 0x81u32.to_le_bytes(), "quote is not TDX")?;
        let signature_len = u32::from_le_bytes(
            bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        require(
            signature_len > 0 && signature_len == bytes.len() - SIGNATURE_OFFSET - 4,
            "invalid TDX signature length",
        )?;
        let body = &bytes[HEADER_BYTES..SIGNATURE_OFFSET];
        let measurement = |offset: usize| FixedBytes::from_slice(&body[offset..offset + 48]);
        Ok(Self {
            measurements: Measurements {
                td_attributes: u64::from_le_bytes(body[120..128].try_into().unwrap()),
                xfam: u64::from_le_bytes(body[128..136].try_into().unwrap()),
                mr_td: measurement(136),
                mr_config_id: measurement(184),
                mr_owner: measurement(232),
                mr_owner_config: measurement(280),
                rtmrs: [
                    measurement(328),
                    measurement(376),
                    measurement(424),
                    measurement(472),
                ],
            },
            report_data: body[520..584].try_into().unwrap(),
        })
    }
}

pub fn quote(report_data: &[u8; 64]) -> io::Result<Vec<u8>> {
    let bytes = native::quote(report_data)?;
    let parsed = Quote::parse(&bytes)?;
    require(
        &parsed.report_data == report_data,
        "quote generator returned wrong report data",
    )?;
    require(
        parsed.measurements.td_attributes & 1 == 0,
        "debug TDX guest",
    )?;
    Ok(bytes)
}

fn require(condition: bool, message: &'static str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, message))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod native {
    use super::*;
    use libloading::{Library, Symbol};
    use std::{ffi::c_void, ptr};

    // Intel tdx_attest.h and sgx_dcap_quoteverify.h. time_t is signed 64-bit on Linux x86_64.
    type GetQuote = unsafe extern "C" fn(
        *const u8,
        *const u8,
        u32,
        *mut u8,
        *mut *mut u8,
        *mut u32,
        u32,
    ) -> u32;
    type FreeQuote = unsafe extern "C" fn(*mut u8) -> u32;
    type VerifyQuote = unsafe extern "C" fn(
        *const u8,
        u32,
        *const c_void,
        i64,
        *mut u32,
        *mut u32,
        *mut c_void,
        u32,
        *mut u8,
    ) -> u32;

    pub(super) fn quote(data: &[u8; 64]) -> io::Result<Vec<u8>> {
        // SAFETY: the installed Intel library is trusted code. Symbols match Intel's C ABI;
        // pointers are valid for the call and the returned allocation is freed by its owner.
        unsafe {
            let lib = Library::new("libtdx_attest.so.1").map_err(io::Error::other)?;
            let get: Symbol<'_, GetQuote> =
                lib.get(b"tdx_att_get_quote\0").map_err(io::Error::other)?;
            let free: Symbol<'_, FreeQuote> =
                lib.get(b"tdx_att_free_quote\0").map_err(io::Error::other)?;
            let mut pointer = ptr::null_mut();
            let mut length = 0;
            let status = get(
                data.as_ptr(),
                ptr::null(),
                0,
                ptr::null_mut(),
                &mut pointer,
                &mut length,
                0,
            );
            let result = if status != 0 {
                Err(io::Error::other(format!(
                    "TDX quote generation failed: {status:#x}"
                )))
            } else if pointer.is_null() || length == 0 || length as usize > MAX_QUOTE_BYTES {
                Err(io::Error::other("invalid DCAP quote allocation"))
            } else {
                Ok(std::slice::from_raw_parts(pointer, length as usize).to_vec())
            };
            if !pointer.is_null() {
                free(pointer);
            }
            result
        }
    }

    pub(super) fn verify(bytes: &[u8], now: u64) -> io::Result<()> {
        let time = i64::try_from(now).map_err(io::Error::other)?;
        // SAFETY: symbol matches Intel's ABI; all buffers live for the duration of the call.
        // Null collateral asks the local quote provider for collateral. Null QvE uses local QVL.
        unsafe {
            let lib = Library::new("libsgx_dcap_quoteverify.so.1").map_err(io::Error::other)?;
            let verify: Symbol<'_, VerifyQuote> = lib
                .get(b"tdx_qv_verify_quote\0")
                .map_err(io::Error::other)?;
            let mut expired = 1;
            let mut result = u32::MAX;
            let status = verify(
                bytes.as_ptr(),
                bytes.len() as u32,
                ptr::null(),
                time,
                &mut expired,
                &mut result,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            );
            if status != 0 || expired != 0 || result != 0 {
                return Err(io::Error::other(format!(
                    "TDX DCAP verification rejected quote: api={status:#x}, expired={expired}, tcb={result:#x}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod native {
    use super::*;
    pub(super) fn quote(_: &[u8; 64]) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TDX requires Linux x86_64",
        ))
    }
    pub(super) fn verify(_: &[u8], _: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TDX requires Linux x86_64",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut bytes = vec![0; SIGNATURE_OFFSET + 5];
        bytes[..2].copy_from_slice(&4u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x81u32.to_le_bytes());
        bytes[HEADER_BYTES + 136..HEADER_BYTES + 184].fill(1);
        bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_malformed_quotes_without_native_code() {
        let good = fixture();
        for len in 0..good.len() {
            assert!(Quote::parse(&good[..len]).is_err());
        }
        for (offset, value) in [(0, 5), (2, 3), (4, 0), (SIGNATURE_OFFSET, 2)] {
            let mut bad = good.clone();
            bad[offset] = value;
            assert!(Quote::parse(&bad).is_err());
        }
        let mut trailing = good;
        trailing.push(0);
        assert!(Quote::parse(&trailing).is_err());
    }

    #[test]
    fn requires_exact_identity_and_binding_and_rejects_debug() {
        let mut bytes = fixture();
        let data = batch_report_data(B256::repeat_byte(42));
        bytes[HEADER_BYTES + 520..HEADER_BYTES + 584].copy_from_slice(&data);
        let parsed = Quote::parse(&bytes).unwrap();
        let policy = Policy {
            measurements: vec![parsed.measurements.clone()],
        };
        policy.validate().unwrap();
        policy.verify_claims(&parsed, &data).unwrap();
        assert!(policy.verify_claims(&parsed, &[0; 64]).is_err());
        bytes[HEADER_BYTES + 328] = 1;
        assert!(
            policy
                .verify_claims(&Quote::parse(&bytes).unwrap(), &data)
                .is_err()
        );
        bytes[HEADER_BYTES + 120] = 1;
        let debug = Quote::parse(&bytes).unwrap();
        assert!(
            Policy {
                measurements: vec![debug.measurements.clone()]
            }
            .validate()
            .is_err()
        );
        assert!(policy.verify_claims(&debug, &data).is_err());
        assert!(
            Policy {
                measurements: vec![]
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn transport_binds_fresh_challenge_in_full_report_data() {
        let data = transport_report_data(&[1; 32], &[2; 32]).unwrap();
        assert_eq!(data[..32], [1; 32]);
        assert_eq!(data[32..], [2; 32]);
        assert_ne!(data, transport_report_data(&[1; 32], &[3; 32]).unwrap());
        assert!(transport_report_data(&[1; 31], &[2; 32]).is_err());
    }
}
