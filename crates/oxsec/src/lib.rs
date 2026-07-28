#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Security helpers for the oxrdp agent protocol.
//!
//! This crate owns the protocol's TLS identity generation, pinned client certificate verifier,
//! and shared-token comparison. It intentionally does not use the older RDP-client trust-on-first-
//! use verifier.

use std::fmt::{self, Write as _};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{self, ring, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as RustlsError, ServerConfig,
    SignatureScheme,
};
use sha2::{Digest, Sha256};

/// A serialized self-signed identity for the agent: certificate + private key, both DER.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// DER-encoded X.509 certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded private key.
    pub key_der: Vec<u8>,
}

impl AgentIdentity {
    /// Generate a fresh self-signed identity. `subject` is a display name (e.g. the guest
    /// hostname); it is NOT used for verification — pinning is.
    pub fn generate(subject: &str) -> Result<Self, SecError> {
        let key_pair = KeyPair::generate().map_err(SecError::CertGeneration)?;
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, subject);

        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name;

        let cert = params
            .self_signed(&key_pair)
            .map_err(SecError::CertGeneration)?;

        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: key_pair.serialize_der(),
        })
    }

    /// Load from PEM files, generating and persisting a new identity if they are absent.
    /// This is what the agent calls at startup.
    pub fn load_or_generate(
        cert_path: &Path,
        key_path: &Path,
        subject: &str,
    ) -> Result<Self, SecError> {
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => {
                let cert = CertificateDer::from_pem_file(cert_path).map_err(SecError::Pem)?;
                let key = PrivateKeyDer::from_pem_file(key_path).map_err(SecError::Pem)?;

                Ok(Self {
                    cert_der: cert.to_vec(),
                    key_der: key.secret_der().to_vec(),
                })
            }
            (false, false) => {
                let identity = Self::generate(subject)?;
                fs::write(cert_path, pem_block("CERTIFICATE", &identity.cert_der)?)
                    .map_err(SecError::Io)?;
                fs::write(key_path, pem_block("PRIVATE KEY", &identity.key_der)?)
                    .map_err(SecError::Io)?;
                restrict_key_permissions(key_path)?;
                Ok(identity)
            }
            _ => Err(SecError::IncompleteIdentityFiles),
        }
    }

    /// SHA-256 of the certificate's SubjectPublicKeyInfo — the value the client pins.
    /// Return it as lowercase hex so it can be passed on a command line or in a config file.
    pub fn spki_pin(&self) -> Result<String, SecError> {
        let pin = spki_pin_from_cert(&CertificateDer::from(self.cert_der.as_slice()))?;
        Ok(hex_encode(&pin))
    }
}

/// Build the agent's TLS server configuration from its identity. No client certificates:
/// authentication is the protocol-level token, checked after the TLS handshake.
pub fn server_config(identity: &AgentIdentity) -> Result<Arc<rustls::ServerConfig>, SecError> {
    let provider = Arc::new(ring::default_provider());
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::try_from(identity.key_der.clone()).map_err(SecError::InvalidKey)?;

    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(SecError::Rustls)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(SecError::Rustls)?;

    Ok(Arc::new(config))
}

/// Build the client's TLS configuration, accepting exactly one server public key.
/// `spki_pin_hex` is the value from `AgentIdentity::spki_pin`, provisioned out of band.
/// Any other certificate MUST be rejected, including a valid publicly-trusted one.
pub fn client_config_pinned(spki_pin_hex: &str) -> Result<Arc<rustls::ClientConfig>, SecError> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(PinnedServerVerifier::new(spki_pin_hex, &provider)?);
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(SecError::Rustls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Constant-time comparison of an authentication token, for the agent's handshake.
/// Returns true only on an exact match. Must not short-circuit on the first differing byte
/// and must not leak the expected length through timing in a way that reveals the secret.
///
/// The byte loop below runs for `expected.len()` iterations — the server's own fixed,
/// attacker-independent value — never `presented.len()`. `presented` is whatever an
/// unauthenticated peer chose to send, and looping on its length would make this function's
/// cost a function of that choice: every `Option::get` bounds check inside the loop is a
/// branch on "is this index still within the secret," so driving the loop by the attacker's
/// own input lets them vary how many of those branches land in-bounds versus out simply by
/// varying how much they send, which is exactly the kind of length-correlated timing this
/// function's contract forbids. Bounding the loop by `expected.len()` instead makes every call
/// take the same number of iterations regardless of what the peer sends.
pub fn verify_token(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = expected.len() ^ presented.len();

    for (index, expected_byte) in expected.iter().copied().enumerate() {
        let presented_byte = presented.get(index).copied().unwrap_or(0);
        diff |= usize::from(expected_byte ^ presented_byte);
    }

    diff == 0
}

/// Read an auth token from a file, trimming trailing whitespace/newline. Errors if the file
/// is missing or empty. (The token must never be passed via argv — argv is world-readable.)
pub fn load_token(path: &Path) -> Result<String, SecError> {
    let token = fs::read_to_string(path)
        .map_err(SecError::Io)?
        .trim_end()
        .to_string();

    if token.is_empty() {
        return Err(SecError::EmptyToken);
    }

    Ok(token)
}

/// Error type for oxrdp security helpers.
#[derive(Debug)]
pub enum SecError {
    /// Filesystem I/O failed.
    Io(std::io::Error),
    /// Certificate generation failed.
    CertGeneration(rcgen::Error),
    /// PEM parsing failed.
    Pem(rustls::pki_types::pem::Error),
    /// rustls configuration or verification failed.
    Rustls(rustls::Error),
    /// Stored identity files were partially present.
    IncompleteIdentityFiles,
    /// Private key DER was malformed or used an unsupported encoding.
    InvalidKey(&'static str),
    /// SPKI pin hex was empty, malformed, or the wrong length.
    InvalidPin,
    /// Certificate DER could not be parsed.
    InvalidCertificate,
    /// Token file was empty after trimming trailing whitespace.
    EmptyToken,
}

impl fmt::Display for SecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::CertGeneration(err) => write!(f, "certificate generation failed: {err}"),
            Self::Pem(err) => write!(f, "PEM parsing failed: {err}"),
            Self::Rustls(err) => write!(f, "rustls error: {err}"),
            Self::IncompleteIdentityFiles => {
                f.write_str("certificate and key files must both exist or both be absent")
            }
            Self::InvalidKey(err) => write!(f, "invalid private key DER: {err}"),
            Self::InvalidPin => f.write_str("SPKI pin must be 64 lowercase hex characters"),
            Self::InvalidCertificate => f.write_str("invalid certificate DER"),
            Self::EmptyToken => f.write_str("token file is empty"),
        }
    }
}

impl std::error::Error for SecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::CertGeneration(err) => Some(err),
            Self::Pem(err) => Some(err),
            Self::Rustls(err) => Some(err),
            Self::IncompleteIdentityFiles
            | Self::InvalidKey(_)
            | Self::InvalidPin
            | Self::InvalidCertificate
            | Self::EmptyToken => None,
        }
    }
}

#[derive(Debug)]
struct PinnedServerVerifier {
    pin: [u8; 32],
    supported_schemes: Vec<SignatureScheme>,
    supported_algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    fn new(spki_pin_hex: &str, provider: &CryptoProvider) -> Result<Self, SecError> {
        let pin = hex_decode_pin(spki_pin_hex)?;
        let supported_algs = provider.signature_verification_algorithms;
        Ok(Self {
            pin,
            supported_schemes: supported_algs.supported_schemes(),
            supported_algs,
        })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let actual_pin = spki_pin_from_cert(end_entity)
            .map_err(|_| RustlsError::InvalidCertificate(CertificateError::BadEncoding))?;

        if ct_eq(&self.pin, &actual_pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes.clone()
    }
}

fn spki_pin_from_cert(cert: &CertificateDer<'_>) -> Result<[u8; 32], SecError> {
    let parsed = ParsedCertificate::try_from(cert).map_err(|_| SecError::InvalidCertificate)?;
    let digest = Sha256::digest(parsed.subject_public_key_info().as_ref());
    Ok(digest.into())
}

fn hex_decode_pin(hex: &str) -> Result<[u8; 32], SecError> {
    if hex.len() != 64 {
        return Err(SecError::InvalidPin);
    }

    let mut out = [0u8; 32];
    for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(chunk[0]).ok_or(SecError::InvalidPin)?;
        let low = hex_value(chunk[1]).ok_or(SecError::InvalidPin)?;
        out[index] = (high << 4) | low;
    }

    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

fn ct_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for index in 0..left.len() {
        diff |= left[index] ^ right[index];
    }
    diff == 0
}

/// Restrict a freshly written private key file to the owner only, where the platform lets us
/// do that without `unsafe`.
///
/// `fs::write` creates a new file with whatever the umask (POSIX) or inherited ACL (Windows)
/// leaves it at — on a POSIX system that is commonly world-readable (mode 0644 under a typical
/// umask). A private key any other local account can read defeats the whole pinning model in
/// `client_config_pinned`/`PinnedServerVerifier`: whoever can read it can present the exact
/// certificate a client already trusts and finish the TLS handshake as this agent, no network
/// position beyond reaching the client needed. There is no safe way to tighten a Windows ACL
/// without unsafe FFI, and this crate is `#![forbid(unsafe_code)]`; on Windows the file keeps
/// its parent directory's inherited ACL, which is a gap a deployment must close itself (e.g. by
/// placing the key under a directory only the agent's own account can read).
#[cfg(unix)]
fn restrict_key_permissions(path: &Path) -> Result<(), SecError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(SecError::Io)
}

/// No-op placeholder documenting the gap — see the `#[cfg(unix)]` version's doc comment.
#[cfg(not(unix))]
fn restrict_key_permissions(_path: &Path) -> Result<(), SecError> {
    Ok(())
}

fn pem_block(label: &str, der: &[u8]) -> Result<String, SecError> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut body = String::with_capacity(der.len().div_ceil(3) * 4);
    for chunk in der.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);

        body.push(char::from(TABLE[((value >> 18) & 0x3f) as usize]));
        body.push(char::from(TABLE[((value >> 12) & 0x3f) as usize]));
        body.push(if chunk.len() > 1 {
            char::from(TABLE[((value >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        body.push(if chunk.len() > 2 {
            char::from(TABLE[(value & 0x3f) as usize])
        } else {
            '='
        });
    }

    let mut pem = String::new();
    writeln!(&mut pem, "-----BEGIN {label}-----").map_err(|_| SecError::InvalidCertificate)?;
    for line in body.as_bytes().chunks(64) {
        let line = std::str::from_utf8(line).map_err(|_| SecError::InvalidCertificate)?;
        writeln!(&mut pem, "{line}").map_err(|_| SecError::InvalidCertificate)?;
    }
    writeln!(&mut pem, "-----END {label}-----").map_err(|_| SecError::InvalidCertificate)?;
    Ok(pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_usable_identity_and_lowercase_hex_pin() {
        let identity = AgentIdentity::generate("guest").expect("identity");
        let pin = identity.spki_pin().expect("pin");

        assert!(!identity.cert_der.is_empty());
        assert!(!identity.key_der.is_empty());
        assert_eq!(pin.len(), 64);
        assert!(pin
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }

    #[test]
    fn generated_identities_have_different_pins() {
        let first = AgentIdentity::generate("guest").expect("first identity");
        let second = AgentIdentity::generate("guest").expect("second identity");

        assert_ne!(
            first.spki_pin().expect("first pin"),
            second.spki_pin().expect("second pin")
        );
    }

    /// A freshly generated private key must not be readable by anyone but the owner. `fs::write`
    /// alone leaves it at the umask's default (commonly 0644, world-readable), which would let
    /// any other local account read it and impersonate this agent to a client that already
    /// trusts its pin — see `restrict_key_permissions`'s doc comment.
    #[cfg(unix)]
    #[test]
    fn generated_key_file_is_not_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("agent-cert.pem");
        let key_path = dir.path().join("agent-key.pem");
        AgentIdentity::load_or_generate(&cert_path, &key_path, "guest").expect("generated");

        let mode = fs::metadata(&key_path)
            .expect("key metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "key file mode was {:o}, expected 0600 (owner read/write only)",
            mode & 0o777
        );
    }

    #[test]
    fn load_or_generate_persists_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("agent-cert.pem");
        let key_path = dir.path().join("agent-key.pem");

        let first =
            AgentIdentity::load_or_generate(&cert_path, &key_path, "guest").expect("generated");
        let second = AgentIdentity::load_or_generate(&cert_path, &key_path, "guest").expect("read");

        assert!(cert_path.exists());
        assert!(key_path.exists());
        assert_eq!(
            first.spki_pin().expect("first pin"),
            second.spki_pin().expect("second pin")
        );
    }

    #[test]
    fn configs_build() {
        let identity = AgentIdentity::generate("guest").expect("identity");
        let pin = identity.spki_pin().expect("pin");

        let _server = server_config(&identity).expect("server config");
        let _client = client_config_pinned(&pin).expect("client config");
    }

    #[test]
    fn pinned_verifier_accepts_only_matching_identity() {
        let first = AgentIdentity::generate("guest-a").expect("first identity");
        let second = AgentIdentity::generate("guest-b").expect("second identity");
        let provider = ring::default_provider();
        let verifier = PinnedServerVerifier::new(&first.spki_pin().expect("pin"), &provider)
            .expect("verifier");
        let name = ServerName::try_from("example.test").expect("server name");
        let first_cert = CertificateDer::from(first.cert_der);
        let second_cert = CertificateDer::from(second.cert_der);

        assert!(verifier
            .verify_server_cert(&first_cert, &[], &name, &[], UnixTime::now())
            .is_ok());
        assert!(verifier
            .verify_server_cert(&second_cert, &[], &name, &[], UnixTime::now())
            .is_err());
    }

    #[test]
    fn verify_token_checks_exact_match() {
        assert!(verify_token("s3cret", "s3cret"));
        assert!(!verify_token("s3cret", "wrong"));
        assert!(!verify_token("s3cret", "s3c"));
        assert!(!verify_token("s3cret", ""));
    }

    /// The comparison loop is bounded by `expected.len()`, not `presented.len()` (see the
    /// function's doc comment). These cover both directions of length mismatch explicitly,
    /// since that is exactly the boundary the loop direction changed.
    #[test]
    fn verify_token_rejects_regardless_of_which_side_is_longer() {
        // `presented` longer than `expected`, sharing `expected` as an exact prefix — the old
        // presented-driven loop would have walked past `expected`'s end reading zeros; the
        // fixed, expected-driven loop never looks past `expected.len()` at all. Either way the
        // length check must still catch it.
        assert!(!verify_token("s3cret", "s3cretXX"));
        // `presented` shorter than `expected`, an exact prefix of it.
        assert!(!verify_token("s3cretXX", "s3cret"));
        // Same length, differ only in the last byte — must not match on a partial prefix.
        assert!(!verify_token("s3cret", "s3creX"));
        // Both empty is a degenerate exact match.
        assert!(verify_token("", ""));
        assert!(!verify_token("", "x"));
        assert!(!verify_token("x", ""));
    }

    #[test]
    fn load_token_trims_trailing_newline_and_rejects_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token.txt");
        let empty_path = dir.path().join("empty.txt");

        fs::write(&token_path, "secret\n").expect("write token");
        fs::write(&empty_path, "\n").expect("write empty");

        assert_eq!(load_token(&token_path).expect("token"), "secret");
        assert!(matches!(load_token(&empty_path), Err(SecError::EmptyToken)));
    }
}
