//! TLS for the RDP security layer.
//!
//! winpodx connects to a guest over `/sec:tls` whose certificate is self-signed and not
//! anchored to any public CA. The client therefore uses a trust-on-first-use posture so the
//! encrypted channel can be established — the same posture as FreeRDP's `/cert:ignore` /
//! `/cert:tofu`.
//!
//! SECURITY NOTE: [`TofuVerifier`] currently accepts ANY server certificate. This protects
//! confidentiality against a passive eavesdropper but NOT against an active
//! man-in-the-middle. Certificate pinning (remember-on-first-use) is a planned hardening;
//! until then, only point oxrdp at servers you control on a trusted network (the winpodx
//! single-tenant model).

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

/// A [`ServerCertVerifier`] that accepts any server certificate (trust-on-first-use).
///
/// See the module-level security note. Signature checks are delegated to the configured
/// crypto provider's algorithms, but the certificate chain and server identity are NOT
/// validated.
#[derive(Debug)]
pub struct TofuVerifier {
    supported_schemes: Vec<SignatureScheme>,
}

impl TofuVerifier {
    /// Build a verifier advertising the given provider's supported signature schemes.
    pub fn new(provider: &CryptoProvider) -> Self {
        Self {
            supported_schemes: provider
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes.clone()
    }
}

/// Build a rustls [`ClientConfig`] for an RDP server with a trust-on-first-use certificate
/// posture (see [`TofuVerifier`]). Uses the `ring` crypto provider.
pub fn tls_client_config() -> Arc<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(TofuVerifier::new(&provider));
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builds() {
        let _config = tls_client_config();
    }

    #[test]
    fn verifier_accepts_any_cert() {
        let provider = ring::default_provider();
        let verifier = TofuVerifier::new(&provider);
        let cert = CertificateDer::from(vec![0u8; 4]);
        let name = ServerName::try_from("example.com").unwrap();
        let verdict = verifier.verify_server_cert(&cert, &[], &name, &[], UnixTime::now());
        assert!(verdict.is_ok());
        assert!(!verifier.supported_verify_schemes().is_empty());
    }
}
