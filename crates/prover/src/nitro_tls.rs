//! AWS Nitro evidence binding for the prover's TLS certificate.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use aws_lc_rs::{
    digest::{Digest as AwsLcDigest, SHA384 as AWS_LC_SHA384, digest as aws_lc_digest},
    signature::{
        ECDSA_P384_SHA384_ASN1, ECDSA_P384_SHA384_FIXED, ParsedPublicKey as AwsLcParsedPublicKey,
    },
};
use rcgen::{
    CertificateParams, CustomExtension, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData as _,
    date_time_ymd,
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_name,
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use sha2::{Digest as _, Sha256};
use tempo_nitro_attestation::{
    NitroAttestation, P384_FIXED_SIGNATURE_SIZE, P384_PUBLIC_KEY_SIZE, P384Verifier, SHA384_SIZE,
    Sha384Hasher,
};
use x509_parser::{certificate::X509Certificate, parse_x509_certificate};

/// Private X.509 extension carrying the raw, COSE_Sign1-encoded Nitro document.
pub const NITRO_ATTESTATION_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 1];
const MAX_FUTURE_SKEW: Duration = Duration::from_secs(300);

/// AWS Nitro Enclaves commercial-partition root certificate (G1), in DER form.
const AWS_NITRO_ROOT_DER: &[u8; 533] = &alloy_primitives::hex!(
    "3082021130820196a003020102021100f93175681b90afe11d46ccb4e4e7f856300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3139313032383133323830355a170d3439313032383134323830355a3049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004fc0254eba608c1f36870e29ada90be46383292736e894bfff672d989444b5051e534a4b1f6dbe3c0bc581a32b7b176070ede12d69a3fea211b66e752cf7dd1dd095f6f1370f4170843d9dc100121e4cf63012809664487c9796284304dc53ff4a3423040300f0603551d130101ff040530030101ff301d0603551d0e041604149025b50dd90547e796c396fa729dcf99a9df4b96300e0603551d0f0101ff040403020186300a06082a8648ce3d0403030369003066023100a37f2f91a1c9bd5ee7b8627c1698d255038e1f0343f95b63a9628c3d39809545a11ebcbf2e3b55d8aeee71b4c3d6adf3023100a2f39b1605b27028a5dd4ba069b5016e65b4fbde8fe0061d6a53197f9cdaf5d943bc61fc2beb03cb6fee8d2302f3dff6"
);

/// Complete allowlist for a particular enclave deployment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NitroPolicy {
    pub(crate) pcrs: BTreeMap<usize, Vec<Vec<u8>>>,
    pub(crate) max_age: Duration,
    pub(crate) module_id: Option<String>,
}

impl NitroPolicy {
    pub fn validate(&self) -> Result<(), NitroError> {
        if self.max_age.is_zero()
            || !(0..=2).all(|index| self.pcrs.contains_key(&index))
            || self.pcrs.iter().any(|(index, values)| {
                *index > 31
                    || values.is_empty()
                    || values.iter().any(|value| value.len() != SHA384_SIZE)
            })
        {
            return Err(NitroError::IncompletePolicy);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NitroBinding<'a> {
    pub context: &'a [u8],
    pub nonce: &'a [u8],
    pub tls_spki: &'a [u8],
}

pub trait NitroAttester: Send + Sync + 'static {
    fn attest(
        &self,
        nonce: &[u8],
        tls_spki: &[u8],
        user_data: &[u8],
    ) -> Result<Vec<u8>, NitroError>;
}

/// Generate a single-use enclave-held key and certificate for one client nonce.
pub fn server_config_for_nonce(
    attester: &dyn NitroAttester,
    context: &[u8],
    nonce: &[u8],
    subject: &str,
    provider: Arc<CryptoProvider>,
) -> Result<rustls::ServerConfig, NitroError> {
    if nonce.len() < 32 {
        return Err(NitroError::NonceTooShort);
    }
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|_| NitroError::CertificateGeneration)?;
    let tls_spki = key_pair.subject_public_key_info();
    let user_data = binding_user_data(NitroBinding {
        context,
        nonce,
        tls_spki: &tls_spki,
    });
    let document = attester.attest(nonce, &tls_spki, &user_data)?;
    if document.len() > tempo_nitro_attestation::MAX_DOCUMENT_SIZE {
        return Err(NitroError::DocumentTooLarge);
    }

    let mut parameters = CertificateParams::new(vec![subject.to_owned()])
        .map_err(|_| NitroError::CertificateGeneration)?;
    // The enclave does not need wall time: freshness comes from the challenge and NSM timestamp.
    parameters.not_before = date_time_ymd(2024, 1, 1);
    parameters.not_after = date_time_ymd(9999, 12, 31);
    parameters
        .custom_extensions
        .push(CustomExtension::from_oid_content(
            NITRO_ATTESTATION_OID,
            document,
        ));
    let certificate = parameters
        .self_signed(&key_pair)
        .map_err(|_| NitroError::CertificateGeneration)?;
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let certificate = CertificateDer::from(certificate.der().to_vec());

    rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| NitroError::CertificateGeneration)?
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)
        .map_err(|_| NitroError::CertificateGeneration)
}

#[derive(Debug)]
pub struct NitroServerVerifier {
    policy: NitroPolicy,
    context: Vec<u8>,
    nonce: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl NitroServerVerifier {
    pub fn new(
        policy: NitroPolicy,
        context: Vec<u8>,
        nonce: Vec<u8>,
        provider: Arc<CryptoProvider>,
    ) -> Result<Self, NitroError> {
        policy.validate()?;
        if nonce.len() < 32 {
            return Err(NitroError::NonceTooShort);
        }
        Ok(Self {
            policy,
            context,
            nonce,
            provider,
        })
    }

    fn certificate_extension<'a>(
        certificate: &'a X509Certificate<'a>,
    ) -> Result<&'a [u8], rustls::Error> {
        let oid = x509_parser::oid_registry::Oid::from(NITRO_ATTESTATION_OID)
            .map_err(|_| rustls::Error::General("invalid Nitro attestation OID".into()))?;
        certificate
            .get_extension_unique(&oid)
            .map_err(|_| rustls::Error::General("duplicate Nitro attestation extension".into()))?
            .map(|extension| extension.value)
            .ok_or_else(|| rustls::Error::General("missing Nitro attestation extension".into()))
    }
}

impl ServerCertVerifier for NitroServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(rustls::Error::General(
                "Nitro attested TLS leaf must be self-signed".into(),
            ));
        }
        let certificate = parse_x509_certificate(end_entity.as_ref())
            .map(|(_, certificate)| certificate)
            .map_err(|_| rustls::Error::General("invalid TLS certificate".into()))?;
        if certificate.subject() != certificate.issuer()
            || certificate.verify_signature(None).is_err()
        {
            return Err(rustls::Error::General(
                "invalid self-signed TLS certificate".into(),
            ));
        }
        verify_cert_unix_time(&certificate, now)?;
        verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
        let evidence = Self::certificate_extension(&certificate)?;
        verify_document(
            evidence,
            &self.policy,
            NitroBinding {
                context: &self.context,
                nonce: &self.nonce,
                tls_spki: certificate.public_key().raw,
            },
            SystemTime::UNIX_EPOCH + Duration::from_secs(now.as_secs()),
        )
        .map_err(|error| {
            rustls::Error::General(format!("Nitro attestation verification failed: {error}"))
        })?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn binding_user_data(binding: NitroBinding<'_>) -> [u8; 32] {
    let mut digest = Sha256::new();
    for value in [binding.context, binding.nonce, binding.tls_spki] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest.finalize().into()
}

fn verify_document(
    encoded: &[u8],
    policy: &NitroPolicy,
    binding: NitroBinding<'_>,
    now: SystemTime,
) -> Result<NitroAttestation, NitroError> {
    policy.validate()?;
    let now_since_epoch = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| NitroError::TimestampInvalid)?;
    let document = tempo_nitro_attestation::verify_attestation(
        encoded,
        now_since_epoch.as_secs(),
        AWS_NITRO_ROOT_DER,
        &AwsLcP384,
    )
    .map_err(|error| NitroError::InvalidDocument(format!("{error:?}")))?;
    verify_binding(&document, policy, binding, now)?;
    Ok(document)
}

fn verify_binding(
    document: &NitroAttestation,
    policy: &NitroPolicy,
    binding: NitroBinding<'_>,
    now: SystemTime,
) -> Result<(), NitroError> {
    if document.nonce != binding.nonce {
        return Err(NitroError::NonceMismatch);
    }
    if document.public_key != binding.tls_spki {
        return Err(NitroError::SpkiMismatch);
    }
    if document.user_data != binding_user_data(binding) {
        return Err(NitroError::UserDataMismatch);
    }
    if let Some(module_id) = &policy.module_id
        && document.module_id != *module_id
    {
        return Err(NitroError::ModuleIdMismatch);
    }
    let timestamp = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(document.timestamp))
        .ok_or(NitroError::TimestampInvalid)?;
    match now.duration_since(timestamp) {
        Ok(age) if age > policy.max_age => return Err(NitroError::Stale),
        Ok(_) => {}
        Err(error) if error.duration() > MAX_FUTURE_SKEW => {
            return Err(NitroError::TimestampInFuture);
        }
        Err(_) => {}
    }
    for (index, allowed) in &policy.pcrs {
        let actual = document
            .pcrs
            .iter()
            .find(|pcr| usize::from(pcr.index) == *index)
            .ok_or(NitroError::PcrSetMismatch)?;
        if !allowed.iter().any(|expected| expected == &actual.value) {
            return Err(NitroError::PcrMismatch(*index));
        }
    }
    Ok(())
}

fn verify_cert_unix_time(
    certificate: &X509Certificate<'_>,
    now: UnixTime,
) -> Result<(), rustls::Error> {
    let now = now.as_secs() as i64;
    if now < certificate.validity().not_before.timestamp()
        || now > certificate.validity().not_after.timestamp()
    {
        return Err(rustls::Error::General(
            "TLS certificate is not currently valid".into(),
        ));
    }
    Ok(())
}

struct AwsLcP384;

impl Sha384Hasher for AwsLcP384 {
    fn sha384(&self, input: &[u8]) -> [u8; SHA384_SIZE] {
        aws_lc_digest(&AWS_LC_SHA384, input)
            .as_ref()
            .try_into()
            .expect("SHA-384 has a fixed output")
    }
}

impl P384Verifier for AwsLcP384 {
    fn validate_public_key(&self, public_key: &[u8; P384_PUBLIC_KEY_SIZE]) -> bool {
        AwsLcParsedPublicKey::new(&ECDSA_P384_SHA384_ASN1, public_key).is_ok()
    }

    fn verify_der(
        &self,
        public_key: &[u8; P384_PUBLIC_KEY_SIZE],
        digest: &[u8; SHA384_SIZE],
        signature_der: &[u8],
    ) -> bool {
        verify_digest(&ECDSA_P384_SHA384_ASN1, public_key, digest, signature_der)
    }

    fn verify_fixed(
        &self,
        public_key: &[u8; P384_PUBLIC_KEY_SIZE],
        digest: &[u8; SHA384_SIZE],
        signature: &[u8; P384_FIXED_SIGNATURE_SIZE],
    ) -> bool {
        verify_digest(&ECDSA_P384_SHA384_FIXED, public_key, digest, signature)
    }
}

fn verify_digest(
    algorithm: &'static aws_lc_rs::signature::EcdsaVerificationAlgorithm,
    public_key: &[u8; P384_PUBLIC_KEY_SIZE],
    digest: &[u8; SHA384_SIZE],
    signature: &[u8],
) -> bool {
    let Ok(public_key) = AwsLcParsedPublicKey::new(algorithm, public_key) else {
        return false;
    };
    let Ok(digest) = AwsLcDigest::import_less_safe(digest, &AWS_LC_SHA384) else {
        return false;
    };
    public_key.verify_digest_sig(&digest, signature).is_ok()
}

#[derive(Debug, thiserror::Error)]
pub enum NitroError {
    #[error(
        "Nitro PCR policy must specify PCR0-2 with nonempty SHA-384 values and positive freshness"
    )]
    IncompletePolicy,
    #[error("Nitro attestation document exceeds the protocol limit")]
    DocumentTooLarge,
    #[error("Nitro attestation document is invalid: {0}")]
    InvalidDocument(String),
    #[error("Nitro attestation nonce does not match this TLS connection")]
    NonceMismatch,
    #[error("Nitro attestation nonce must have at least 32 bytes")]
    NonceTooShort,
    #[error("Nitro attestation public key does not match the TLS certificate SPKI")]
    SpkiMismatch,
    #[error("Nitro attestation user_data does not bind the protocol, nonce, and SPKI")]
    UserDataMismatch,
    #[error("Nitro attestation module ID does not match policy")]
    ModuleIdMismatch,
    #[error("Nitro attestation timestamp is invalid")]
    TimestampInvalid,
    #[error("Nitro attestation timestamp is in the future")]
    TimestampInFuture,
    #[error("Nitro attestation is stale")]
    Stale,
    #[error("Nitro attestation is missing a PCR required by policy")]
    PcrSetMismatch,
    #[error("Nitro attestation PCR {0} is not approved")]
    PcrMismatch(usize),
    #[error("could not generate the Nitro-attested TLS certificate")]
    CertificateGeneration,
    #[error("Nitro attestation generation failed: {0}")]
    AttestationGeneration(String),
}

#[cfg(test)]
mod tests {
    use tempo_nitro_attestation::Pcr;

    use super::*;

    fn policy() -> NitroPolicy {
        NitroPolicy {
            pcrs: BTreeMap::from([
                (0, vec![vec![0x11; SHA384_SIZE]]),
                (1, vec![vec![0x12; SHA384_SIZE]]),
                (2, vec![vec![0x13; SHA384_SIZE]]),
            ]),
            max_age: Duration::from_secs(60),
            module_id: Some("test-module".into()),
        }
    }

    fn attestation(binding: NitroBinding<'_>, now: SystemTime) -> NitroAttestation {
        NitroAttestation {
            module_id: "test-module".into(),
            timestamp: now
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            pcrs: vec![
                Pcr {
                    index: 0,
                    value: vec![0x11; SHA384_SIZE],
                },
                Pcr {
                    index: 1,
                    value: vec![0x12; SHA384_SIZE],
                },
                Pcr {
                    index: 2,
                    value: vec![0x13; SHA384_SIZE],
                },
            ],
            public_key: binding.tls_spki.to_vec(),
            user_data: binding_user_data(binding).to_vec(),
            nonce: binding.nonce.to_vec(),
            leaf_cert_hash: [0; 32],
        }
    }

    #[test]
    fn requires_nonce_spki_context_and_complete_policy() {
        let now = SystemTime::now();
        let context = b"prover";
        let nonce = [7; 32];
        let spki = [8; 91];
        let binding = NitroBinding {
            context,
            nonce: &nonce,
            tls_spki: &spki,
        };
        let valid = attestation(binding, now);
        verify_binding(&valid, &policy(), binding, now).unwrap();

        let mut wrong_nonce = valid.clone();
        wrong_nonce.nonce[0] ^= 1;
        assert!(matches!(
            verify_binding(&wrong_nonce, &policy(), binding, now),
            Err(NitroError::NonceMismatch)
        ));

        let mut wrong_spki = valid.clone();
        wrong_spki.public_key[0] ^= 1;
        assert!(matches!(
            verify_binding(&wrong_spki, &policy(), binding, now),
            Err(NitroError::SpkiMismatch)
        ));

        let mut wrong_context = valid;
        wrong_context.user_data[0] ^= 1;
        assert!(matches!(
            verify_binding(&wrong_context, &policy(), binding, now),
            Err(NitroError::UserDataMismatch)
        ));
    }
}
