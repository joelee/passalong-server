//! API keys: what a device proves itself with.
//!
//! A key is `pal_<key id>_<secret>`. The key id, 12 lower-case hex digits,
//! is public: it is the key's name in every command, log line, and audit
//! row. The secret, 64 lower-case hex digits, is 256 random bits. The server
//! keeps the SHA-256 of the secret and nothing else of it: a slow password
//! hash adds nothing to a secret nobody could guess, and costs every
//! request. A secret is shown once, when the key is made, and cannot be
//! shown again because it is not kept.

use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::ids::ApiKeyId;
use crate::random::RandomSource;

const PREFIX: &str = "pal_";
const ID_DIGITS: usize = 12;
const SECRET_DIGITS: usize = 64;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A whole key, secret included. It exists in two places only: where a key
/// is made, to be shown once, and where a request's token is parsed, to be
/// hashed at once. It has no `Display`, and its `Debug` shows the key id.
pub struct ApiKey {
    id: ApiKeyId,
    secret: String,
}

impl ApiKey {
    /// A new key: 6 random bytes of id, 32 of secret.
    pub fn generate(rng: &mut dyn RandomSource) -> Self {
        let (mut id, mut secret) = ([0_u8; ID_DIGITS / 2], [0_u8; SECRET_DIGITS / 2]);
        rng.fill(&mut id);
        rng.fill(&mut secret);
        Self {
            id: ApiKeyId::new(hex(&id)),
            secret: hex(&secret),
        }
    }

    /// Parses a bearer token. `None` for anything but the exact form; the
    /// caller answers `UNAUTHENTICATED` and says no more.
    pub fn parse(token: &str) -> Option<Self> {
        let rest = token.strip_prefix(PREFIX)?;
        let (id, secret) = rest.split_once('_')?;
        let well_formed = id.len() == ID_DIGITS
            && secret.len() == SECRET_DIGITS
            && is_lower_hex(id)
            && is_lower_hex(secret);
        well_formed.then(|| Self {
            id: ApiKeyId::new(id),
            secret: secret.to_owned(),
        })
    }

    /// The key's public id.
    pub fn id(&self) -> &ApiKeyId {
        &self.id
    }

    /// What the server keeps of the secret.
    pub fn hash(&self) -> SecretHash {
        SecretHash(Sha256::digest(self.secret.as_bytes()).into())
    }

    /// The whole key as text. For `key create`, which prints it once; there
    /// is no other reason to call this.
    pub fn reveal(&self) -> String {
        format!("{PREFIX}{}_{}", self.id.as_str(), self.secret)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey({}, secret withheld)", self.id.as_str())
    }
}

/// The SHA-256 of a key's secret.
#[derive(Clone)]
pub struct SecretHash([u8; 32]);

impl SecretHash {
    /// Whether two hashes are equal, in time that does not depend on where
    /// they differ.
    pub fn matches(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }

    /// For the database.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// From the database.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    /// The hash of no key: what an unknown key id is compared with, so that
    /// it costs what a known one costs.
    pub fn of_nothing() -> Self {
        Self([0; 32])
    }
}

impl fmt::Debug for SecretHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretHash(withheld)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::SeededRandom;

    #[test]
    fn a_key_is_pal_a_key_id_and_a_secret() {
        let key = ApiKey::generate(&mut SeededRandom::new(1));
        let text = key.reveal();
        let parts: Vec<&str> = text.split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "pal");
        assert_eq!((parts[1].len(), parts[2].len()), (12, 64));
        assert!(
            text.bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'_'))
        );
        assert_eq!(key.id().as_str(), parts[1]);

        // The same source makes the same key, another source another.
        assert_eq!(ApiKey::generate(&mut SeededRandom::new(1)).reveal(), text);
        assert_ne!(ApiKey::generate(&mut SeededRandom::new(2)).reveal(), text);
    }

    #[test]
    fn a_key_parses_back_and_nothing_else_does() {
        let key = ApiKey::generate(&mut SeededRandom::new(1));
        let text = key.reveal();
        let parsed = ApiKey::parse(&text).unwrap();
        assert_eq!(parsed.id(), key.id());
        assert!(parsed.hash().matches(&key.hash()));

        let upper = text.to_uppercase();
        let short = &text[..text.len() - 1];
        let long = format!("{text}0");
        let spaced = format!(" {text}");
        for bad in [
            "",
            "pal_",
            "pal__",
            "key_000000000000_00",
            &upper,
            short,
            &long,
            &spaced,
            "pal_../../etc_00",
        ] {
            assert!(ApiKey::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn nothing_shows_a_secret_but_reveal() {
        let key = ApiKey::generate(&mut SeededRandom::new(1));
        let secret = key.reveal()[17..].to_owned();
        let shown = format!("{key:?} {:?} {:?}", key.hash(), key.id());
        for start in 0..secret.len() - 8 {
            assert!(!shown.contains(&secret[start..start + 8]), "{shown}");
        }
        assert!(shown.contains(key.id().as_str()));
    }

    #[test]
    fn a_hash_matches_its_own_secret_only() {
        let key = ApiKey::generate(&mut SeededRandom::new(1));
        let other = ApiKey::generate(&mut SeededRandom::new(2));
        assert!(key.hash().matches(&key.hash()));
        assert!(!key.hash().matches(&other.hash()));
        // What the database stores comes back as what it was.
        let stored = key.hash().to_bytes();
        assert_eq!(stored.len(), 32);
        assert!(
            SecretHash::from_bytes(&stored)
                .unwrap()
                .matches(&key.hash())
        );
        assert!(SecretHash::from_bytes(&stored[..31]).is_none());
        // And is not the secret.
        let secret = key.reveal()[17..].to_owned();
        assert_ne!(
            stored
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            secret
        );
    }

    #[test]
    fn the_operating_system_gives_a_different_key_every_time() {
        let mut os = crate::random::OsRandom;
        assert_ne!(
            ApiKey::generate(&mut os).reveal(),
            ApiKey::generate(&mut os).reveal()
        );
    }
}
