use std::sync::Arc;
use std::{fmt, ops::Deref, str::FromStr};

use ed25519_consensus as ed25519;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use ed25519::Error;
pub use ed25519::Signature;

/// Verified (used as type witness).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Verified;
/// Unverified (used as type witness).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Unverified;

pub trait Signer: Send + Sync {
    /// Return this signer's public/verification key.
    fn public_key(&self) -> &PublicKey;
    /// Sign a message and return the signature.
    fn sign(&self, msg: &[u8]) -> Signature;
}

impl<T> Signer for Arc<T>
where
    T: Signer + ?Sized,
{
    fn sign(&self, msg: &[u8]) -> Signature {
        self.deref().sign(msg)
    }

    fn public_key(&self) -> &PublicKey {
        self.deref().public_key()
    }
}

impl<T> Signer for &T
where
    T: Signer + ?Sized,
{
    fn sign(&self, msg: &[u8]) -> Signature {
        self.deref().sign(msg)
    }

    fn public_key(&self) -> &PublicKey {
        self.deref().public_key()
    }
}

/// The public/verification key.
#[derive(Serialize, Deserialize, Eq, Debug, Copy, Clone)]
#[serde(into = "String", try_from = "String")]
pub struct PublicKey(pub ed25519::VerificationKey);

/// The private/signing key.
pub type SecretKey = ed25519::SigningKey;

#[derive(Error, Debug)]
pub enum PublicKeyError {
    #[error("invalid length {0}")]
    InvalidLength(usize),
    #[error("invalid multibase string: {0}")]
    Multibase(#[from] multibase::Error),
    #[error("invalid key: {0}")]
    InvalidKey(#[from] ed25519_consensus::Error),
}

impl std::hash::Hash for PublicKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_bytes().hash(state)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_human())
    }
}

impl From<PublicKey> for String {
    fn from(other: PublicKey) -> Self {
        other.to_human()
    }
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl From<ed25519::VerificationKey> for PublicKey {
    fn from(other: ed25519::VerificationKey) -> Self {
        Self(other)
    }
}

impl TryFrom<[u8; 32]> for PublicKey {
    type Error = ed25519::Error;

    fn try_from(other: [u8; 32]) -> Result<Self, Self::Error> {
        Ok(Self(ed25519::VerificationKey::try_from(other)?))
    }
}

impl PublicKey {
    pub fn to_human(&self) -> String {
        multibase::encode(multibase::Base::Base58Btc, &self.0)
    }
}

impl FromStr for PublicKey {
    type Err = PublicKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (_, bytes) = multibase::decode(s)?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| PublicKeyError::InvalidLength(v.len()))?;
        let key = ed25519::VerificationKey::try_from(ed25519::VerificationKeyBytes::from(array))?;

        Ok(Self(key))
    }
}

impl TryFrom<String> for PublicKey {
    type Error = PublicKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_str(&value)
    }
}

impl Deref for PublicKey {
    type Target = ed25519::VerificationKey;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod test {
    use crate::crypto::PublicKey;
    use quickcheck_macros::quickcheck;
    use std::str::FromStr;

    #[quickcheck]
    fn prop_encode_decode(input: PublicKey) {
        let encoded = input.to_string();
        let decoded = PublicKey::from_str(&encoded).unwrap();

        assert_eq!(input, decoded);
    }
}
