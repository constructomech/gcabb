//! Cryptographic verification of update metadata and artifacts.
//!
//! Update signing is deliberately independent of platform code signing: it must
//! work identically on all three targets, and it must keep working before OS
//! certificates exist. Transport security is not treated as sufficient. A
//! release is trusted because it carries a valid signature from a key this
//! build ships, not because it arrived over HTTPS from a plausible URL.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Signing algorithm. Only ed25519 exists today; the field is present so an
/// algorithm change is a detectable rejection instead of a silent misparse.
pub const ALGORITHM: &str = "ed25519";

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("signature file is malformed: {0}")]
    MalformedSignature(String),
    #[error("unsupported signature algorithm {found}, expected {ALGORITHM}")]
    UnsupportedAlgorithm { found: String },
    #[error("update was signed with unknown key {key_id}; this build does not trust that key")]
    UntrustedKey { key_id: String },
    #[error("signature does not match the update manifest")]
    BadSignature,
    #[error("no signing keys are configured, so no update can be trusted")]
    NoTrustedKeys,
    #[error("artifact hash mismatch: expected {expected}, downloaded {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("artifact size mismatch: expected {expected} bytes, downloaded {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
}

/// A detached signature over the exact bytes of an update manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetachedSignature {
    pub algorithm: String,
    /// Short identifier of the signing key, so keys can be rotated and a
    /// mismatch can be reported precisely instead of as a generic failure.
    pub key_id: String,
    /// Base64-encoded 64-byte ed25519 signature.
    pub signature: String,
}

impl DetachedSignature {
    /// Decodes the signature bytes.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError`] when the algorithm is unknown or the payload is
    /// not a well-formed ed25519 signature.
    pub fn decode(&self) -> Result<Signature, VerifyError> {
        if self.algorithm != ALGORITHM {
            return Err(VerifyError::UnsupportedAlgorithm {
                found: self.algorithm.clone(),
            });
        }
        let raw = BASE64
            .decode(self.signature.trim())
            .map_err(|error| VerifyError::MalformedSignature(error.to_string()))?;
        let bytes: [u8; 64] = raw
            .try_into()
            .map_err(|_| VerifyError::MalformedSignature("signature is not 64 bytes".to_owned()))?;
        Ok(Signature::from_bytes(&bytes))
    }
}

/// A public key this build trusts to sign releases.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedKey {
    pub key_id: String,
    pub key: VerifyingKey,
}

impl TrustedKey {
    /// Builds a trusted key from a base64-encoded 32-byte ed25519 public key.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError`] when the key is not a valid ed25519 public key.
    pub fn from_base64(key_id: &str, encoded: &str) -> Result<Self, VerifyError> {
        let raw = BASE64
            .decode(encoded.trim())
            .map_err(|error| VerifyError::MalformedSignature(error.to_string()))?;
        let bytes: [u8; 32] = raw.try_into().map_err(|_| {
            VerifyError::MalformedSignature("public key is not 32 bytes".to_owned())
        })?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|error| VerifyError::MalformedSignature(error.to_string()))?;
        Ok(Self {
            key_id: key_id.to_owned(),
            key,
        })
    }
}

/// The set of keys whose signatures this build accepts.
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    keys: Vec<TrustedKey>,
}

impl TrustStore {
    #[must_use]
    pub fn new(keys: Vec<TrustedKey>) -> Self {
        Self { keys }
    }

    /// Trust store baked into this build.
    ///
    /// `GCABB_UPDATE_PUBLIC_KEY` is injected at compile time by the release
    /// workflow. A build without it has an empty store and therefore cannot
    /// accept any update, which is the correct failure: an unsigned build
    /// stream is worse than no updates at all.
    #[must_use]
    pub fn embedded() -> Self {
        let Some(encoded) = option_env!("GCABB_UPDATE_PUBLIC_KEY") else {
            return Self::default();
        };
        let key_id = option_env!("GCABB_UPDATE_KEY_ID").unwrap_or("default");
        match TrustedKey::from_base64(key_id, encoded) {
            Ok(key) => Self::new(vec![key]),
            Err(error) => {
                tracing::error!(%error, "embedded update signing key is unusable");
                Self::default()
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Verifies a detached signature over the exact manifest bytes.
    ///
    /// `payload` must be the bytes as fetched, not a re-serialisation of a
    /// parsed manifest: re-encoding can reorder or reformat fields and would
    /// invalidate an otherwise good signature.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError`] when no keys are configured, the key is unknown,
    /// or the signature does not match.
    pub fn verify(
        &self,
        payload: &[u8],
        signature: &DetachedSignature,
    ) -> Result<&TrustedKey, VerifyError> {
        if self.keys.is_empty() {
            return Err(VerifyError::NoTrustedKeys);
        }
        let decoded = signature.decode()?;
        let trusted = self
            .keys
            .iter()
            .find(|candidate| candidate.key_id == signature.key_id)
            .ok_or_else(|| VerifyError::UntrustedKey {
                key_id: signature.key_id.clone(),
            })?;
        trusted
            .key
            .verify_strict(payload, &decoded)
            .map_err(|_| VerifyError::BadSignature)?;
        Ok(trusted)
    }
}

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Confirms a downloaded artifact matches the size and hash the manifest
/// promised.
///
/// # Errors
///
/// Returns [`VerifyError`] on any mismatch. A truncated download fails here
/// rather than being unpacked into the install directory.
pub fn verify_artifact_bytes(
    bytes: &[u8],
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), VerifyError> {
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(VerifyError::SizeMismatch {
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        return Err(VerifyError::HashMismatch {
            expected: expected_sha256.to_owned(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use ed25519_dalek::{Signer as _, SigningKey};
    use rand::rngs::OsRng;

    use super::{
        ALGORITHM, DetachedSignature, TrustStore, TrustedKey, VerifyError, sha256_hex,
        verify_artifact_bytes,
    };

    fn signing_pair(key_id: &str) -> (SigningKey, TrustStore) {
        let signing = SigningKey::generate(&mut OsRng);
        let encoded = BASE64.encode(signing.verifying_key().to_bytes());
        let trusted = TrustedKey::from_base64(key_id, &encoded).unwrap();
        (signing, TrustStore::new(vec![trusted]))
    }

    fn sign(signing: &SigningKey, key_id: &str, payload: &[u8]) -> DetachedSignature {
        DetachedSignature {
            algorithm: ALGORITHM.to_owned(),
            key_id: key_id.to_owned(),
            signature: BASE64.encode(signing.sign(payload).to_bytes()),
        }
    }

    #[test]
    fn a_genuine_signature_verifies() {
        let (signing, store) = signing_pair("release-1");
        let payload = b"{\"version\":\"0.2.0\"}";
        let signature = sign(&signing, "release-1", payload);
        assert!(store.verify(payload, &signature).is_ok());
    }

    #[test]
    fn a_tampered_manifest_is_rejected() {
        let (signing, store) = signing_pair("release-1");
        let signature = sign(&signing, "release-1", b"{\"version\":\"0.2.0\"}");
        let tampered = b"{\"version\":\"9.9.9\"}";
        assert!(matches!(
            store.verify(tampered, &signature),
            Err(VerifyError::BadSignature)
        ));
    }

    #[test]
    fn a_signature_from_an_untrusted_key_is_rejected() {
        let (_, store) = signing_pair("release-1");
        let (attacker, _) = signing_pair("attacker");
        let payload = b"{\"version\":\"0.2.0\"}";
        // Attacker signs with their own key but claims the trusted key id.
        let forged = sign(&attacker, "release-1", payload);
        assert!(matches!(
            store.verify(payload, &forged),
            Err(VerifyError::BadSignature)
        ));
    }

    #[test]
    fn an_unknown_key_id_is_reported_precisely() {
        let (signing, store) = signing_pair("release-1");
        let payload = b"payload";
        let signature = sign(&signing, "rotated-key", payload);
        assert!(matches!(
            store.verify(payload, &signature),
            Err(VerifyError::UntrustedKey { .. })
        ));
    }

    #[test]
    fn a_build_with_no_keys_trusts_nothing() {
        let store = TrustStore::default();
        let (signing, _) = signing_pair("release-1");
        let payload = b"payload";
        let signature = sign(&signing, "release-1", payload);
        assert!(matches!(
            store.verify(payload, &signature),
            Err(VerifyError::NoTrustedKeys)
        ));
    }

    #[test]
    fn an_unknown_algorithm_is_refused() {
        let (signing, store) = signing_pair("release-1");
        let payload = b"payload";
        let mut signature = sign(&signing, "release-1", payload);
        signature.algorithm = "rsa".to_owned();
        assert!(matches!(
            store.verify(payload, &signature),
            Err(VerifyError::UnsupportedAlgorithm { .. })
        ));
    }

    #[test]
    fn a_truncated_download_fails_before_install() {
        let bytes = b"complete artifact";
        let hash = sha256_hex(bytes);
        assert!(matches!(
            verify_artifact_bytes(&bytes[..5], bytes.len() as u64, &hash),
            Err(VerifyError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn a_corrupted_download_fails_before_install() {
        let bytes = b"complete artifact";
        let hash = sha256_hex(b"different artifact");
        assert!(matches!(
            verify_artifact_bytes(bytes, bytes.len() as u64, &hash),
            Err(VerifyError::HashMismatch { .. })
        ));
    }

    #[test]
    fn a_matching_download_passes() {
        let bytes = b"complete artifact";
        let hash = sha256_hex(bytes);
        assert!(verify_artifact_bytes(bytes, bytes.len() as u64, &hash).is_ok());
    }
}
