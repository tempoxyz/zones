//! Attestation-only client for the Linux Nitro Secure Module device.
//!
//! ABI: <https://github.com/torvalds/linux/blob/v6.6/include/uapi/linux/nsm.h>
//! Wire format: <https://github.com/aws/aws-nitro-enclaves-nsm-api/blob/v0.5.2/src/api/mod.rs>
//! The signed attestation document is returned unchanged; this module only decodes
//! the device's response envelope, not the document or its signature.

use std::{fs::File, io, os::fd::AsRawFd};

use serde::Serialize;
use serde_bytes::Bytes;

const REQUEST_MAX_SIZE: usize = 0x1000;
const RESPONSE_MAX_SIZE: usize = 0x3000;

// The Linux UAPI uses fixed-width u64 fields, not native Rust slice layouts.
#[repr(C)]
struct NsmIovec {
    addr: u64,
    len: u64,
}

#[repr(C)]
struct NsmMessage {
    request: NsmIovec,
    response: NsmIovec,
}

const NSM_IOCTL_RAW: libc::c_ulong = libc::_IOWR::<NsmMessage>(0x0a, 0) as libc::c_ulong;

#[derive(Serialize)]
enum Request<'a> {
    Attestation {
        user_data: &'a Bytes,
        nonce: Option<&'a Bytes>,
        public_key: Option<&'a Bytes>,
    },
}

pub(super) fn attestation(digest: &[u8; 32]) -> Result<Vec<u8>, String> {
    let device = File::options()
        .read(true)
        .write(true)
        .open("/dev/nsm")
        .map_err(|error| format!("Nitro Secure Module device is unavailable: {error}"))?;
    request_attestation(&device, digest)
}

fn encode_request(digest: &[u8; 32]) -> Result<Vec<u8>, String> {
    let request = Request::Attestation {
        user_data: Bytes::new(digest),
        nonce: None,
        public_key: None,
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&request, &mut encoded)
        .map_err(|error| format!("Could not encode Nitro attestation request: {error}"))?;
    if encoded.len() > REQUEST_MAX_SIZE {
        return Err("Nitro attestation request exceeds the device limit".into());
    }
    Ok(encoded)
}

fn request_attestation(device: &File, digest: &[u8; 32]) -> Result<Vec<u8>, String> {
    let request = encode_request(digest)?;
    let mut response = [0u8; RESPONSE_MAX_SIZE];
    let mut message = NsmMessage {
        request: NsmIovec {
            addr: request.as_ptr() as u64,
            len: request.len() as u64,
        },
        response: NsmIovec {
            addr: response.as_mut_ptr() as u64,
            len: response.len() as u64,
        },
    };

    // SAFETY: message has the Linux nsm_raw layout. Both buffer pointers remain
    // valid for the synchronous ioctl; their lengths match their allocations.
    // The kernel may write only to the response buffer and message metadata.
    let result = unsafe { libc::ioctl(device.as_raw_fd(), NSM_IOCTL_RAW, &mut message) };
    if result < 0 {
        return Err(format!(
            "Nitro attestation ioctl failed: {}",
            io::Error::last_os_error()
        ));
    }
    let length = usize::try_from(message.response.len)
        .map_err(|_| "Nitro attestation response length does not fit usize")?;
    let response = response
        .get(..length)
        .ok_or("Nitro attestation response exceeds the device buffer")?;
    decode_response(response)
}

fn decode_response(mut encoded: &[u8]) -> Result<Vec<u8>, String> {
    use ciborium::Value;

    if encoded.is_empty() || encoded.len() > RESPONSE_MAX_SIZE {
        return Err("Invalid Nitro attestation response length".into());
    }
    let response: Value = ciborium::from_reader(&mut encoded)
        .map_err(|error| format!("Could not decode Nitro attestation response: {error}"))?;
    if !encoded.is_empty() {
        return Err("Trailing data in Nitro attestation response".into());
    }
    let Value::Map(mut envelope) = response else {
        return Err("Nitro Secure Module returned an unexpected response".into());
    };
    if envelope.len() != 1 {
        return Err("Nitro Secure Module returned an unexpected response".into());
    }
    let (kind, value) = envelope.pop().expect("checked single-entry map");
    match (kind.as_text(), value) {
        (Some("Error"), Value::Text(code)) => {
            Err(format!("Nitro attestation request failed: {code}"))
        }
        (Some("Attestation"), Value::Map(mut fields)) if fields.len() == 1 => {
            let (name, document) = fields.pop().expect("checked single-entry map");
            match (name.as_text(), document) {
                (Some("document"), Value::Bytes(document)) if !document.is_empty() => Ok(document),
                _ => Err("Nitro Secure Module returned an invalid attestation document".into()),
            }
        }
        _ => Err("Nitro Secure Module returned an unexpected response".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-encoded protocol fixtures, independent of the client's serializer.
    // The document bytes are synthetic: these test the envelope, not signatures.
    const RESPONSE: &[u8] = b"\xa1\x6bAttestation\xa1\x68document\x43\xd2\x84\x40";

    #[test]
    fn matches_linux_ioctl_abi() {
        assert_eq!(size_of::<NsmIovec>(), 16);
        assert_eq!(size_of::<NsmMessage>(), 32);
        assert_eq!(std::mem::offset_of!(NsmMessage, response), 16);
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert_eq!(NSM_IOCTL_RAW, 0xc020_0a00);
    }

    #[test]
    fn encodes_named_fields_and_byte_string_with_null_optional_fields() {
        let mut expected = b"\xa1\x6bAttestation\xa3\x69user_data\x58\x20".to_vec();
        expected.extend_from_slice(&[0x42; 32]);
        expected.extend_from_slice(b"\x65nonce\xf6\x6apublic_key\xf6");
        assert_eq!(encode_request(&[0x42; 32]).unwrap(), expected);
    }

    #[test]
    fn preserves_signed_document_bytes() {
        assert_eq!(decode_response(RESPONSE).unwrap(), [0xd2, 0x84, 0x40]);
    }

    #[test]
    fn reports_device_error() {
        let error = decode_response(b"\xa1\x65Error\x6fInvalidArgument").unwrap_err();
        assert!(error.contains("InvalidArgument"), "{error}");
    }

    #[test]
    fn rejects_malformed_or_unexpected_responses() {
        for invalid in [
            &b""[..],
            &RESPONSE[..RESPONSE.len() - 1],
            b"\xff",
            b"\xa0",
            b"\xa1\x69GetRandom\xa0",
            b"\xa1\x6bAttestation\xa0",
            b"\xa1\x6bAttestation\xa1\x68document\x40",
            b"\xa1\x6bAttestation\xa1\x68document\x63abc",
            b"\xa1\x6bAttestation\xa1\x68document\x83\x01\x02\x03",
        ] {
            assert!(decode_response(invalid).is_err(), "accepted {invalid:?}");
        }
        let mut trailing = RESPONSE.to_vec();
        trailing.push(0);
        assert!(decode_response(&trailing).is_err());
        assert!(decode_response(&[0; RESPONSE_MAX_SIZE + 1]).is_err());
    }

    #[test]
    fn reports_ioctl_failure() {
        let device = File::options()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let error = request_attestation(&device, &[0; 32]).unwrap_err();
        assert!(error.contains("ioctl failed"), "{error}");
    }

    #[test]
    #[ignore = "requires /dev/nsm inside an AWS Nitro Enclave"]
    fn live_attestation_contains_requested_digest() {
        use ciborium::Value;

        let digest = [0x42; 32];
        let document = attestation(&digest).unwrap();
        let cose: Value = ciborium::from_reader(document.as_slice()).unwrap();
        let cose = match cose {
            Value::Tag(18, cose) => *cose,
            cose => cose,
        };
        let fields = cose.as_array().expect("COSE_Sign1 array");
        assert_eq!(fields.len(), 4);
        let payload = fields[2].as_bytes().expect("COSE payload");
        let payload: Value = ciborium::from_reader(payload.as_slice()).unwrap();
        let user_data = payload
            .as_map()
            .unwrap()
            .iter()
            .find(|(name, _)| name.as_text() == Some("user_data"))
            .map(|(_, value)| value.as_bytes().unwrap())
            .unwrap();
        assert_eq!(user_data.as_slice(), digest);
    }
}
