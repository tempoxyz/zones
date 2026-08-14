//! Versioned value encoding for persistence tables.

use bincode::Options;
use reth_codecs::{Compress, Decompress, DecompressError};
use serde::{Serialize, de::DeserializeOwned};
/// Largest encoded persistence value accepted by the durable codec.
pub(crate) const MAX_VALUE_SIZE: u64 = 8 * 1024 * 1024;
const ENVELOPE_VERSION: u8 = 1;

/// Failure while encoding or decoding a persisted value envelope.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CodecError {
    #[error("value exceeds 8 MiB limit")]
    Oversize,
    #[error("unknown value envelope version {0}")]
    Version(u8),
    #[error("malformed value: {0}")]
    Malformed(String),
}
/// Return the deterministic bincode configuration used inside value envelopes.
fn options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .with_limit(MAX_VALUE_SIZE - 1)
}

fn unbounded_options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}
/// Encode one persistence value with its versioned envelope.
pub(crate) fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, CodecError> {
    let mut out = vec![ENVELOPE_VERSION];
    options().serialize_into(&mut out, v).map_err(map)?;
    if out.len() as u64 > MAX_VALUE_SIZE {
        return Err(CodecError::Oversize);
    }
    Ok(out)
}
/// Decode one bounded, versioned persistence value envelope.
pub(crate) fn decode<T: DeserializeOwned>(v: &[u8]) -> Result<T, CodecError> {
    if v.len() as u64 > MAX_VALUE_SIZE {
        return Err(CodecError::Oversize);
    }
    let (&version, body) = v
        .split_first()
        .ok_or_else(|| CodecError::Malformed("empty envelope".into()))?;
    if version != ENVELOPE_VERSION {
        return Err(CodecError::Version(version));
    }
    options().deserialize(body).map_err(map)
}

/// Encode a logical value that will be split into independently bounded rows.
pub(crate) fn encode_unbounded<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut out = vec![ENVELOPE_VERSION];
    unbounded_options()
        .serialize_into(&mut out, value)
        .map_err(map)?;
    Ok(out)
}

/// Decode a logical value after its bounded rows have been authenticated and joined.
pub(crate) fn decode_unbounded<T: DeserializeOwned>(value: &[u8]) -> Result<T, CodecError> {
    let (&version, body) = value
        .split_first()
        .ok_or_else(|| CodecError::Malformed("empty envelope".into()))?;
    if version != ENVELOPE_VERSION {
        return Err(CodecError::Version(version));
    }
    unbounded_options().deserialize(body).map_err(map)
}
/// Map bincode failures into durable codec errors.
fn map(e: Box<bincode::ErrorKind>) -> CodecError {
    if matches!(*e, bincode::ErrorKind::SizeLimit) {
        CodecError::Oversize
    } else {
        CodecError::Malformed(e.to_string())
    }
}

/// Implement the common value codec for one serde persistence record.
macro_rules! value_codec {
    ($($value:ty),+ $(,)?) => {
        $(
            impl Compress for $value {
                type Compressed = Vec<u8>;

                fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                    let encoded = encode(self)
                        .expect("persistence values are validated before database writes");
                    buf.put_slice(&encoded);
                }
            }

            impl Decompress for $value {
                fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
                    decode(value).map_err(|error| {
                        DecompressError::new(std::io::Error::other(error))
                    })
                }
            }
        )+
    };
}

value_codec!(
    super::CheckpointManifest,
    super::CheckpointChunk,
    super::JournalEntry,
    super::Finding
);

impl Compress for super::MetaValue {
    type Compressed = Vec<u8>;
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        match self {
            Self::Version(version) => {
                buf.put_u8(0);
                buf.put_slice(&version.to_be_bytes());
            }
            Self::Metadata(metadata) => {
                buf.put_u8(1);
                buf.put_slice(&encode(metadata).expect("metadata has a fixed encoded size"));
            }
        }
    }
}

impl Decompress for super::MetaValue {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        match value.split_first() {
            Some((0, body)) if body.len() == 4 => Ok(Self::Version(u32::from_be_bytes(
                body.try_into().expect("length checked"),
            ))),
            Some((1, body)) => decode(body)
                .map(Box::new)
                .map(Self::Metadata)
                .map_err(|error| DecompressError::new(std::io::Error::other(error))),
            _ => Err(DecompressError::new(std::io::Error::other(
                "invalid metadata tag or version width",
            ))),
        }
    }
}
