//! The private PKI behind remote agents (mutual TLS).
//!
//! - The panel CA is ECDSA P-384 / SHA-384 and lives sealed in panel-api's
//!   database. It signs two kinds of leaf, both ECDSA P-256:
//!   - one panel client certificate (`clientAuth` only, short-lived and
//!     rotated automatically), and
//!   - one server certificate per remote agent (`serverAuth` only), issued
//!     from a CSR the agent made, so the agent's private key never leaves its
//!     host.
//! - Every leaf carries exactly one extended key usage. webpki enforces it,
//!   so an agent certificate cannot be used as a client certificate against
//!   another agent, and the panel's client certificate cannot serve.
//! - Every connection is TLS 1.3 only.
//!
//! The issuer copies nothing from a CSR except its public key (whose
//! signature is verified): names, key usages, and validity are always the
//! panel's own.

use crate::error::{CoreError, CoreResult};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::server::AllowAnyAuthenticatedClient;
use rustls::{Certificate, ClientConfig, PrivateKey, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Validity of the panel client certificate; it is renewed once less than
/// [`CLIENT_RENEW_BEFORE_DAYS`] remain.
pub const CLIENT_CERT_DAYS: i64 = 30;
pub const CLIENT_RENEW_BEFORE_DAYS: i64 = 10;
/// Validity of an agent server certificate.
pub const AGENT_CERT_DAYS: i64 = 90;
const CA_DAYS: i64 = 3650;

pub const PANEL_CLIENT_NAME: &str = "scontrol-panel";

/// A certificate with its key, both PEM.
#[derive(Clone)]
pub struct KeyedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for KeyedCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedCert")
            .field("cert_pem", &self.cert_pem)
            .field("key_pem", &"<redacted>")
            .finish()
    }
}

/// Facts about an issued certificate the panel records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedInfo {
    pub serial_hex: String,
    /// SHA-256 of the DER certificate, lowercase hex.
    pub fingerprint: String,
    pub not_after: chrono::DateTime<chrono::Utc>,
}

fn pki_error(context: &str, error: impl std::fmt::Display) -> CoreError {
    CoreError::External(format!("{context}: {error}"))
}

fn random_serial() -> SerialNumber {
    let mut bytes = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
    // Positive and non-zero as RFC 5280 requires.
    bytes[0] &= 0x7f;
    bytes[0] |= 0x01;
    SerialNumber::from_slice(&bytes)
}

fn validity(params: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    // A little back-dating tolerates clock skew between hosts.
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(days);
}

fn to_chrono(t: time::OffsetDateTime) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(t.unix_timestamp(), 0).unwrap_or_default()
}

fn issued_info(cert: &rcgen::Certificate) -> IssuedInfo {
    let params = cert.params();
    IssuedInfo {
        serial_hex: params
            .serial_number
            .as_ref()
            .map(|s| hex::encode(s.to_bytes()))
            .unwrap_or_default(),
        fingerprint: fingerprint_der(cert.der()),
        not_after: to_chrono(params.not_after),
    }
}

pub fn fingerprint_der(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// SHA-256 fingerprint of the first certificate in a PEM bundle.
pub fn fingerprint_pem(cert_pem: &str) -> CoreResult<String> {
    let certs = parse_certs(cert_pem)?;
    Ok(fingerprint_der(&certs[0].0))
}

/// Creates the panel CA.
pub fn generate_ca() -> CoreResult<KeyedCert> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
        .map_err(|e| pki_error("generating CA key", e))?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "scontrol agent CA");
    name.push(DnType::OrganizationName, "scontrol");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(random_serial());
    validity(&mut params, CA_DAYS);
    let cert = params
        .self_signed(&key)
        .map_err(|e| pki_error("self-signing CA", e))?;
    Ok(KeyedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// The CA as an issuer. rcgen needs a `Certificate` value; rebuilding it
/// from the stored PEM keeps the subject and key identifier the same, which
/// is all an issued leaf refers to.
fn load_issuer(ca: &KeyedCert) -> CoreResult<(rcgen::Certificate, KeyPair)> {
    let key = KeyPair::from_pem(&ca.key_pem).map_err(|e| pki_error("loading CA key", e))?;
    let params = CertificateParams::from_ca_cert_pem(&ca.cert_pem)
        .map_err(|e| pki_error("loading CA certificate", e))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| pki_error("loading CA certificate", e))?;
    Ok((cert, key))
}

fn leaf_params(
    common_name: &str,
    sans: Vec<SanType>,
    eku: ExtendedKeyUsagePurpose,
    days: i64,
) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    name.push(DnType::OrganizationName, "scontrol");
    params.distinguished_name = name;
    params.subject_alt_names = sans;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![eku];
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(random_serial());
    validity(&mut params, days);
    params
}

/// Issues the panel's client certificate (and its key).
pub fn issue_client_cert(ca: &KeyedCert, days: i64) -> CoreResult<(KeyedCert, IssuedInfo)> {
    let (issuer, issuer_key) = load_issuer(ca)?;
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| pki_error("generating client key", e))?;
    let params = leaf_params(
        PANEL_CLIENT_NAME,
        Vec::new(),
        ExtendedKeyUsagePurpose::ClientAuth,
        days,
    );
    let cert = params
        .signed_by(&key, &issuer, &issuer_key)
        .map_err(|e| pki_error("signing client certificate", e))?;
    let info = issued_info(&cert);
    Ok((
        KeyedCert {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        },
        info,
    ))
}

/// The SAN entry for an agent address host (a DNS name or an IP address).
fn host_san(host: &str) -> CoreResult<SanType> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SanType::IpAddress(ip));
    }
    if !crate::types::validate::is_valid_domain(host) {
        return Err(CoreError::Validation(format!(
            "'{host}' is not a valid hostname or IP address"
        )));
    }
    Ok(SanType::DnsName(host.to_string().try_into().map_err(
        |e| CoreError::Validation(format!("invalid hostname: {e}")),
    )?))
}

/// Signs an agent's CSR as a server certificate for `host`. Only the CSR's
/// (signature-checked) public key is used; it must be ECDSA P-256/P-384 or
/// Ed25519.
pub fn sign_agent_csr(
    ca: &KeyedCert,
    csr_pem: &str,
    host: &str,
    server_id: uuid::Uuid,
    days: i64,
) -> CoreResult<(String, IssuedInfo)> {
    if csr_pem.len() > 16 * 1024 {
        return Err(CoreError::Validation("the CSR is too large".into()));
    }
    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem.trim()).map_err(|e| {
        CoreError::Validation(format!("not a valid certificate signing request: {e}"))
    })?;
    let algorithm = csr.public_key.algorithm();
    if ![
        &rcgen::PKCS_ECDSA_P256_SHA256,
        &rcgen::PKCS_ECDSA_P384_SHA384,
        &rcgen::PKCS_ED25519,
    ]
    .contains(&algorithm)
    {
        return Err(CoreError::Validation(
            "the CSR key must be ECDSA P-256/P-384 or Ed25519".into(),
        ));
    }
    let (issuer, issuer_key) = load_issuer(ca)?;
    csr.params = leaf_params(
        &format!("scontrol-agent-{server_id}"),
        vec![host_san(host)?],
        ExtendedKeyUsagePurpose::ServerAuth,
        days,
    );
    let cert = csr
        .signed_by(&issuer, &issuer_key)
        .map_err(|e| pki_error("signing agent certificate", e))?;
    let info = issued_info(&cert);
    Ok((cert.pem(), info))
}

/// A new agent key and a CSR for it (run on the agent host).
pub fn generate_agent_csr(host: &str) -> CoreResult<KeyedCert> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| pki_error("generating agent key", e))?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "scontrol-agent");
    params.distinguished_name = name;
    params.subject_alt_names = vec![host_san(host)?];
    let csr = params
        .serialize_request(&key)
        .map_err(|e| pki_error("creating CSR", e))?;
    Ok(KeyedCert {
        cert_pem: csr.pem().map_err(|e| pki_error("encoding CSR", e))?,
        key_pem: key.serialize_pem(),
    })
}

fn parse_certs(pem: &str) -> CoreResult<Vec<Certificate>> {
    let certs = rustls_pemfile::certs(&mut pem.as_bytes())
        .map_err(|e| CoreError::Validation(format!("reading certificate PEM: {e}")))?;
    if certs.is_empty() {
        return Err(CoreError::Validation("no certificate in PEM".into()));
    }
    Ok(certs.into_iter().map(Certificate).collect())
}

fn parse_key(pem: &str) -> CoreResult<PrivateKey> {
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut pem.as_bytes())
        .map_err(|e| CoreError::Validation(format!("reading key PEM: {e}")))?;
    if keys.len() != 1 {
        return Err(CoreError::Validation(
            "expected exactly one PKCS#8 private key".into(),
        ));
    }
    Ok(PrivateKey(keys.remove(0)))
}

fn roots(ca_pem: &str) -> CoreResult<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in parse_certs(ca_pem)? {
        roots
            .add(&cert)
            .map_err(|e| CoreError::Validation(format!("invalid CA certificate: {e}")))?;
    }
    Ok(roots)
}

/// TLS 1.3 client configuration for panel-api: trusts only the panel CA and
/// presents the panel client certificate.
pub fn client_config(ca_pem: &str, identity: &KeyedCert) -> CoreResult<Arc<ClientConfig>> {
    let config = ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| pki_error("TLS client config", e))?
        .with_root_certificates(roots(ca_pem)?)
        .with_client_auth_cert(
            parse_certs(&identity.cert_pem)?,
            parse_key(&identity.key_pem)?,
        )
        .map_err(|e| pki_error("TLS client certificate", e))?;
    Ok(Arc::new(config))
}

/// TLS 1.3 server configuration for a remote agent: a client must present a
/// certificate from the panel CA with the `clientAuth` usage.
pub fn server_config(ca_pem: &str, identity: &KeyedCert) -> CoreResult<Arc<ServerConfig>> {
    let verifier = AllowAnyAuthenticatedClient::new(roots(ca_pem)?).boxed();
    let config = ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| pki_error("TLS server config", e))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            parse_certs(&identity.cert_pem)?,
            parse_key(&identity.key_pem)?,
        )
        .map_err(|e| pki_error("TLS server certificate", e))?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_leaves_chain_to_the_ca_and_carry_one_usage() {
        let ca = generate_ca().unwrap();
        let (client, info) = issue_client_cert(&ca, CLIENT_CERT_DAYS).unwrap();
        assert_eq!(info.fingerprint, fingerprint_pem(&client.cert_pem).unwrap());
        assert_eq!(info.serial_hex.len(), 32);
        let days = (info.not_after - chrono::Utc::now()).num_days();
        assert!((CLIENT_CERT_DAYS - 1..=CLIENT_CERT_DAYS).contains(&days));
        client_config(&ca.cert_pem, &client).unwrap();

        let request = generate_agent_csr("agent.example.com").unwrap();
        let (cert_pem, agent_info) = sign_agent_csr(
            &ca,
            &request.cert_pem,
            "agent.example.com",
            uuid::Uuid::nil(),
            AGENT_CERT_DAYS,
        )
        .unwrap();
        assert_ne!(agent_info.serial_hex, info.serial_hex);
        server_config(
            &ca.cert_pem,
            &KeyedCert {
                cert_pem,
                key_pem: request.key_pem,
            },
        )
        .unwrap();
    }

    #[test]
    fn csr_names_are_ignored_and_the_host_is_validated() {
        let ca = generate_ca().unwrap();
        // The CSR asks for another name; the panel's host is what is signed.
        let request = generate_agent_csr("evil.example.com").unwrap();
        let (cert_pem, _) = sign_agent_csr(
            &ca,
            &request.cert_pem,
            "10.0.0.7",
            uuid::Uuid::nil(),
            AGENT_CERT_DAYS,
        )
        .unwrap();
        let params = CertificateParams::from_ca_cert_pem(&cert_pem).unwrap();
        assert_eq!(
            params.subject_alt_names,
            vec![SanType::IpAddress("10.0.0.7".parse().unwrap())]
        );
        assert!(sign_agent_csr(&ca, &request.cert_pem, "bad host", uuid::Uuid::nil(), 1).is_err());
    }

    #[test]
    fn tampered_or_foreign_csrs_are_refused() {
        let ca = generate_ca().unwrap();
        assert!(sign_agent_csr(&ca, "not a csr", "a.example.com", uuid::Uuid::nil(), 1).is_err());
        let request = generate_agent_csr("a.example.com").unwrap();
        // Flip one byte of the DER inside the PEM body.
        let der = pem_body(&request.cert_pem);
        let mut tampered = der.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0x01;
        let tampered_pem = to_pem("CERTIFICATE REQUEST", &tampered);
        assert!(sign_agent_csr(&ca, &tampered_pem, "a.example.com", uuid::Uuid::nil(), 1).is_err());
    }

    fn pem_body(pem: &str) -> Vec<u8> {
        use base64::Engine;
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap()
    }

    fn to_pem(label: &str, der: &[u8]) -> String {
        use base64::Engine;
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }
}
