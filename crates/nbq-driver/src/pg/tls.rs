//! TLS for Postgres/Redshift.
//!
//! VerifyFull checks the hostname and chain with OS trust plus public PEM
//! roots. Prefer/Require retain their explicit encryption-only semantics.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;
use crate::{ConnectParams, DriverError, SslMode};

const AWS_ROOTS: &str = include_str!("../../certificates/aws-rds-global-bundle.pem");

pub fn validate_ca_bundle(pem: &str) -> crate::Result<Vec<CertificateDer<'static>>> {
    if pem.len() > 512 * 1024 || pem.contains("PRIVATE KEY") {
        return Err(DriverError::connection("Choose a public PEM CA bundle under 512 KiB, without private keys"));
    }
    let certs = rustls_pemfile::certs(&mut pem.as_bytes()).collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| DriverError::connection(format!("Invalid CA bundle: {e}")))?;
    if certs.is_empty() && !pem.trim().is_empty() {
        return Err(DriverError::connection("No PEM certificates found in CA bundle"));
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in &certs {
        roots.add(cert.clone()).map_err(|e| DriverError::connection(format!("Invalid CA certificate: {e}")))?;
    }
    Ok(certs)
}

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub fn make_tls(params: &ConnectParams) -> crate::Result<MakeRustlsConnect> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports default TLS versions");
    let config = if params.ssl == SslMode::VerifyFull {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        let host = params.host.to_ascii_lowercase();
        let pem = if !params.ssl_root_cert.is_empty() {
            params.ssl_root_cert.as_str()
        } else if host.ends_with(".rds.amazonaws.com") || host.ends_with(".redshift.amazonaws.com") {
            AWS_ROOTS
        } else { "" };
        for cert in validate_ca_bundle(pem)? {
            roots.add(cert).map_err(|e| DriverError::connection(format!("Invalid CA: {e}")))?;
        }
        if roots.is_empty() {
            return Err(DriverError::connection("No trusted roots available. Import a public CA certificate."));
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder.dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        .with_no_client_auth()
    };
    Ok(MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_public_ca_bundles() {
        assert!(validate_ca_bundle(AWS_ROOTS).unwrap().len() > 1);
        for invalid in ["not a PEM", "-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----", "-----BEGIN PRIVATE KEY-----"] {
            assert!(validate_ca_bundle(invalid).is_err());
        }
        assert!(validate_ca_bundle(&"x".repeat(512 * 1024 + 1)).is_err());
    }
}
