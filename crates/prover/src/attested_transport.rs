//! Nitro-attested TLS transport for prover requests.

use std::{collections::BTreeMap, io, path::Path, sync::Arc, time::Duration};

use rand::RngCore as _;
use rustls::{ClientConfig, crypto::CryptoProvider, pki_types::ServerName};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream,
    server::TlsStream as ServerTlsStream,
};

use crate::nitro_tls::{
    NitroAttester, NitroError, NitroPolicy, NitroServerVerifier, server_config_for_nonce,
};

const HELLO_MAGIC: [u8; 8] = *b"TZRATLS1";
const HELLO_VERSION: u8 = 1;
const NONCE_BYTES: usize = 32;
const MAX_CONTEXT_BYTES: usize = 1024;
const MAX_POLICY_BYTES: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PROVER_CONTEXT: &[u8] = b"tempo-zone-prover/chunked-json/v1";
const PROVER_SERVER_NAME: &str = "tempo-zone-prover.invalid";

/// A remote prover address paired with the policy required to authenticate it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteProverConfig {
    address: String,
    policy: NitroPolicy,
}

impl RemoteProverConfig {
    /// Parse and validate a remote prover configuration before any connection is attempted.
    pub fn from_policy_file(address: String, path: &Path) -> Result<Self, AttestedTransportError> {
        let bytes = std::fs::read(path).map_err(|source| AttestedTransportError::PolicyFile {
            path: path.to_owned(),
            source,
        })?;
        Self::from_policy_json(address, &bytes)
    }

    /// Parse and validate a remote prover configuration from JSON bytes.
    pub fn from_policy_json(address: String, bytes: &[u8]) -> Result<Self, AttestedTransportError> {
        if address.trim().is_empty() {
            return Err(AttestedTransportError::InvalidPolicy(
                "remote prover address must not be empty".into(),
            ));
        }
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(AttestedTransportError::InvalidPolicy(
                "policy exceeds 64 KiB".into(),
            ));
        }
        let raw: RemoteProverAttestationPolicy = serde_json::from_slice(bytes)
            .map_err(|error| AttestedTransportError::InvalidPolicy(error.to_string()))?;
        Ok(Self {
            address,
            policy: raw.try_into()?,
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Connect and authenticate fresh Nitro evidence before returning a TLS stream.
    pub async fn connect(&self) -> Result<ClientTlsStream<TcpStream>, AttestedTransportError> {
        let stream = timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(&self.address))
            .await
            .map_err(|_| AttestedTransportError::Timeout("TCP connect"))??;
        connect_stream(stream, self.policy.clone()).await
    }
}

#[derive(Clone, Debug)]
struct AttestedHello {
    nonce: [u8; NONCE_BYTES],
    context: Vec<u8>,
}

impl AttestedHello {
    fn new(context: Vec<u8>) -> io::Result<Self> {
        if context.is_empty() || context.len() > MAX_CONTEXT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attested transport context must be between 1 and 1024 bytes",
            ));
        }
        let mut nonce = [0_u8; NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce);
        Ok(Self { nonce, context })
    }
}

async fn write_hello<T: AsyncWrite + Unpin>(
    stream: &mut T,
    hello: &AttestedHello,
) -> io::Result<()> {
    stream.write_all(&HELLO_MAGIC).await?;
    stream.write_u8(HELLO_VERSION).await?;
    stream.write_all(&hello.nonce).await?;
    stream
        .write_u16(hello.context.len().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "attested transport context is too large",
            )
        })?)
        .await?;
    stream.write_all(&hello.context).await?;
    stream.flush().await
}

async fn read_hello<T: AsyncRead + Unpin>(stream: &mut T) -> io::Result<AttestedHello> {
    let mut magic = [0_u8; HELLO_MAGIC.len()];
    stream.read_exact(&mut magic).await?;
    if magic != HELLO_MAGIC || stream.read_u8().await? != HELLO_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid attested transport preface",
        ));
    }
    let mut nonce = [0_u8; NONCE_BYTES];
    stream.read_exact(&mut nonce).await?;
    let context_length = usize::from(stream.read_u16().await?);
    if context_length == 0 || context_length > MAX_CONTEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid attested transport context length",
        ));
    }
    let mut context = vec![0_u8; context_length];
    stream.read_exact(&mut context).await?;
    Ok(AttestedHello { nonce, context })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteProverAttestationPolicy {
    pcrs: BTreeMap<usize, Vec<String>>,
    #[serde(default = "default_max_age_seconds")]
    max_age_seconds: u64,
    #[serde(default)]
    module_id: Option<String>,
}

fn default_max_age_seconds() -> u64 {
    300
}

impl TryFrom<RemoteProverAttestationPolicy> for NitroPolicy {
    type Error = AttestedTransportError;

    fn try_from(raw: RemoteProverAttestationPolicy) -> Result<Self, Self::Error> {
        let mut pcrs = BTreeMap::new();
        for (index, allowed) in raw.pcrs {
            let values = allowed
                .into_iter()
                .map(|value| {
                    const_hex::decode(value.strip_prefix("0x").unwrap_or(&value)).map_err(|error| {
                        AttestedTransportError::InvalidPolicy(format!("PCR {index}: {error}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            pcrs.insert(index, values);
        }
        let policy = Self {
            pcrs,
            max_age: Duration::from_secs(raw.max_age_seconds),
            module_id: raw.module_id,
        };
        policy
            .validate()
            .map_err(|error| AttestedTransportError::InvalidPolicy(error.to_string()))?;
        Ok(policy)
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

async fn connect_stream<T>(
    mut stream: T,
    policy: NitroPolicy,
) -> Result<ClientTlsStream<T>, AttestedTransportError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let hello = AttestedHello::new(PROVER_CONTEXT.to_vec())?;
    timeout(HANDSHAKE_TIMEOUT, write_hello(&mut stream, &hello))
        .await
        .map_err(|_| AttestedTransportError::Timeout("attestation preface write"))??;
    let provider = provider();
    let verifier = NitroServerVerifier::new(
        policy,
        hello.context,
        hello.nonce.to_vec(),
        provider.clone(),
    )?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let server_name = ServerName::try_from(PROVER_SERVER_NAME)
        .map_err(|_| AttestedTransportError::InvalidServerName)?
        .to_owned();
    timeout(
        HANDSHAKE_TIMEOUT,
        TlsConnector::from(Arc::new(client_config)).connect(server_name, stream),
    )
    .await
    .map_err(|_| AttestedTransportError::Timeout("TLS handshake"))?
    .map_err(Into::into)
}

/// Read the client challenge, create a single-use attested certificate, and require TLS 1.3.
pub async fn accept<T>(
    mut stream: T,
    attester: Arc<dyn NitroAttester>,
) -> Result<ServerTlsStream<T>, AttestedTransportError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let hello = timeout(HANDSHAKE_TIMEOUT, read_hello(&mut stream))
        .await
        .map_err(|_| AttestedTransportError::Timeout("attestation preface read"))??;
    if hello.context != PROVER_CONTEXT {
        return Err(AttestedTransportError::InvalidContext);
    }
    let nonce = hello.nonce;
    let server_provider = provider();
    let server_config = tokio::task::spawn_blocking(move || {
        server_config_for_nonce(
            attester.as_ref(),
            PROVER_CONTEXT,
            &nonce,
            PROVER_SERVER_NAME,
            server_provider,
        )
    })
    .await
    .map_err(|_| AttestedTransportError::AttestationWorker)??;
    timeout(
        HANDSHAKE_TIMEOUT,
        TlsAcceptor::from(Arc::new(server_config)).accept(stream),
    )
    .await
    .map_err(|_| AttestedTransportError::Timeout("TLS handshake"))?
    .map_err(Into::into)
}

#[derive(Debug, thiserror::Error)]
pub enum AttestedTransportError {
    #[error("could not read prover attestation policy {path}: {source}")]
    PolicyFile {
        path: std::path::PathBuf,
        source: io::Error,
    },
    #[error("invalid prover attestation policy: {0}")]
    InvalidPolicy(String),
    #[error("attested transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("attested TLS configuration failed: {0}")]
    Tls(#[from] rustls::Error),
    #[error("Nitro attestation failed: {0}")]
    Nitro(#[from] NitroError),
    #[error("attested transport timed out during {0}")]
    Timeout(&'static str),
    #[error("invalid prover TLS server name")]
    InvalidServerName,
    #[error("attested transport context does not identify the prover protocol")]
    InvalidContext,
    #[error("Nitro attestation worker panicked")]
    AttestationWorker,
}

#[cfg(test)]
mod tests {
    use tempo_nitro_attestation::SHA384_SIZE;
    use tokio::io::{AsyncWriteExt as _, duplex};

    use super::*;

    #[tokio::test]
    async fn hello_round_trip() {
        let hello = AttestedHello::new(PROVER_CONTEXT.to_vec()).unwrap();
        let (mut writer, mut reader) = duplex(4096);
        let expected = hello.clone();
        let write = tokio::spawn(async move { write_hello(&mut writer, &hello).await });
        let received = read_hello(&mut reader).await.unwrap();
        write.await.unwrap().unwrap();
        assert_eq!(received.nonce, expected.nonce);
        assert_eq!(received.context, expected.context);
    }

    #[tokio::test]
    async fn rejects_a_plaintext_prover_frame_as_an_attestation_preface() {
        let (mut writer, mut reader) = duplex(4096);
        writer.write_all(&[0_u8; 8]).await.unwrap();
        writer.shutdown().await.unwrap();
        let error = read_hello(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    struct InvalidAttester;

    impl NitroAttester for InvalidAttester {
        fn attest(
            &self,
            _nonce: &[u8],
            _tls_spki: &[u8],
            _user_data: &[u8],
        ) -> Result<Vec<u8>, NitroError> {
            Ok(vec![0])
        }
    }

    #[tokio::test]
    async fn rejects_tls_without_valid_nitro_evidence() {
        let (client, server) = duplex(64 * 1024);
        let server = tokio::spawn(accept(server, Arc::new(InvalidAttester)));
        let allowed = vec![vec![0x11; SHA384_SIZE]];
        let policy = NitroPolicy {
            pcrs: BTreeMap::from([(0, allowed.clone()), (1, allowed.clone()), (2, allowed)]),
            max_age: Duration::from_secs(300),
            module_id: None,
        };

        assert!(connect_stream(client, policy).await.is_err());
        assert!(server.await.unwrap().is_err());
    }

    #[test]
    fn policy_rejects_missing_or_malformed_pcrs() {
        for json in [
            br#"{"pcrs":{}}"#.as_slice(),
            br#"{"pcrs":{"0":["not-hex"]}}"#.as_slice(),
            br#"{"pcrs":{"0":["11"]}}"#.as_slice(),
        ] {
            assert!(RemoteProverConfig::from_policy_json("127.0.0.1:5000".into(), json).is_err());
        }
    }

    #[test]
    fn policy_accepts_pcr_zero_through_two() {
        let pcr = "11".repeat(48);
        let json = serde_json::to_vec(&serde_json::json!({
            "pcrs": { "0": [&pcr], "1": [&pcr], "2": [&pcr] },
            "max_age_seconds": 300,
        }))
        .unwrap();
        assert!(RemoteProverConfig::from_policy_json("127.0.0.1:5000".into(), &json).is_ok());
    }
}
