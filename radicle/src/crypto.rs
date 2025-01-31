use std::sync::Arc;
use std::{fmt, ops::Deref, str::FromStr};

use ed25519_compact as ed25519;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use ed25519::{Error, KeyPair, Seed};

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

/// Cryptographic signature.
#[derive(PartialEq, Eq, Copy, Clone)]
pub struct Signature(pub ed25519::Signature);

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = multibase::Base::Base58Btc;
        write!(f, "{}", multibase::encode(base, self.deref()))
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", self)
    }
}

#[derive(Error, Debug)]
pub enum SignatureError {
    #[error("invalid multibase string: {0}")]
    Multibase(#[from] multibase::Error),
    #[error("invalid signature: {0}")]
    Invalid(#[from] ed25519::Error),
}

impl From<ed25519::Signature> for Signature {
    fn from(other: ed25519::Signature) -> Self {
        Self(other)
    }
}

impl FromStr for Signature {
    type Err = SignatureError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (_, bytes) = multibase::decode(s)?;
        let sig = ed25519::Signature::from_slice(bytes.as_slice())?;

        Ok(Self(sig))
    }
}

impl Deref for Signature {
    type Target = ed25519::Signature;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<[u8; 64]> for Signature {
    fn from(bytes: [u8; 64]) -> Self {
        Self(ed25519::Signature::new(bytes))
    }
}

impl TryFrom<&[u8]> for Signature {
    type Error = ed25519::Error;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        ed25519::Signature::from_slice(bytes).map(Self)
    }
}

/// The public/verification key.
#[derive(Serialize, Deserialize, Eq, Copy, Clone)]
#[serde(into = "String", try_from = "String")]
pub struct PublicKey(pub ed25519::PublicKey);

impl PublicKey {
    pub fn from_pem(pem: &str) -> Result<Self, ed25519::Error> {
        ed25519::PublicKey::from_pem(pem).map(Self)
    }
}

/// The private/signing key.
pub type SecretKey = ed25519::SecretKey;

#[derive(Error, Debug)]
pub enum PublicKeyError {
    #[error("invalid length {0}")]
    InvalidLength(usize),
    #[error("invalid multibase string: {0}")]
    Multibase(#[from] multibase::Error),
    #[error("invalid key: {0}")]
    InvalidKey(#[from] ed25519::Error),
}

impl std::hash::Hash for PublicKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.deref().hash(state)
    }
}

impl PartialOrd for PublicKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.as_ref().partial_cmp(other.as_ref())
    }
}

impl Ord for PublicKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_ref().cmp(other.as_ref())
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

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self)
    }
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl From<ed25519::PublicKey> for PublicKey {
    fn from(other: ed25519::PublicKey) -> Self {
        Self(other)
    }
}

impl TryFrom<[u8; 32]> for PublicKey {
    type Error = ed25519::Error;

    fn try_from(other: [u8; 32]) -> Result<Self, Self::Error> {
        Ok(Self(ed25519::PublicKey::new(other)))
    }
}

impl PublicKey {
    pub fn to_human(&self) -> String {
        multibase::encode(multibase::Base::Base58Btc, self.0.deref())
    }
}

impl FromStr for PublicKey {
    type Err = PublicKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (_, bytes) = multibase::decode(s)?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| PublicKeyError::InvalidLength(v.len()))?;
        let key = ed25519::PublicKey::new(array);

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
    type Target = ed25519::PublicKey;

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
