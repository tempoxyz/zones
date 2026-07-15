use std::{fmt, path::Path};

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{
    Signer as _,
    ed25519::{PrivateKey, PublicKey},
};

/// An Ed25519 private key loaded as a node's Commonware identity.
#[derive(Clone)]
pub(crate) struct Ed25519Identity(PrivateKey);

impl Ed25519Identity {
    /// Reads an unencrypted, hex-encoded identity from `path`.
    pub(crate) fn read_from_file(path: impl AsRef<Path>) -> Result<Self, Ed25519IdentityError> {
        let path = path.as_ref();
        let encoded =
            std::fs::read_to_string(path).map_err(|source| Ed25519IdentityError::Read {
                path: path.to_owned(),
                source,
            })?;
        Self::from_hex(encoded.trim())
    }

    /// Parses an unencrypted, hex-encoded identity.
    pub(crate) fn from_hex(encoded: &str) -> Result<Self, Ed25519IdentityError> {
        let bytes = const_hex::decode(encoded).map_err(Ed25519IdentityError::Hex)?;
        let key = PrivateKey::decode(&bytes[..]).map_err(Ed25519IdentityError::Decode)?;
        Ok(Self(key))
    }

    /// Returns the Ed25519 public key corresponding to this private key.
    pub(crate) fn ed25519_public_key(&self) -> PublicKey {
        self.0.public_key()
    }

    pub(crate) fn into_private_key(self) -> PrivateKey {
        self.0
    }
}

impl fmt::Debug for Ed25519Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ed25519Identity")
            .field("ed25519_public_key", &self.ed25519_public_key())
            .finish_non_exhaustive()
    }
}

/// Errors produced while loading a Commonware Ed25519 identity.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Ed25519IdentityError {
    #[error("failed reading Commonware Ed25519 identity `{path}`")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Commonware Ed25519 identity is not valid hex")]
    Hex(#[source] const_hex::FromHexError),

    #[error("Commonware identity is not a valid Ed25519 private key")]
    Decode(#[source] commonware_codec::Error),
}

#[cfg(test)]
mod tests {
    use commonware_codec::Encode as _;
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::Ed25519Identity;

    #[test]
    fn parses_hex_identity() {
        let key = PrivateKey::from_seed(7);
        let identity =
            Ed25519Identity::from_hex(&const_hex::encode_prefixed(key.encode().as_ref())).unwrap();

        assert_eq!(identity.ed25519_public_key(), key.public_key());
    }
}
