//! Identifiers, parsed strictly so that each is safe as a path component.

use std::fmt;

use crate::error::ApiError;
use crate::random::RandomSource;

fn is_lower_hex(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An item's id, `<ts>-<key>`: 8 lower-case hex digits of creation time, a
/// dash, and the 12 lower-case hex digits of the content key, as in the
/// passalong client's item schema. Nothing else parses, so an id is always
/// safe as a path component, and string order is chronological.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(String);

impl ItemId {
    /// Parses an id.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidId`] for anything but the exact form.
    pub fn parse(text: &str) -> Result<Self, ApiError> {
        let ok = text.len() == 21
            && text.as_bytes()[8] == b'-'
            && is_lower_hex(&text[..8])
            && is_lower_hex(&text[9..]);
        if ok {
            Ok(Self(text.to_owned()))
        } else {
            Err(ApiError::InvalidId(format!("`{text}` is not an item id")))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The content key: identical content has the same one, which is what
    /// deduplication compares. In an encrypted workspace it is keyed by the
    /// client, and means nothing to the server beyond equality.
    pub fn content_key(&self) -> &str {
        &self.0[9..]
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The id of a workspace's data key: lower-case hex, at most 64 digits. The
/// server compares key ids and never sees a key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(String);

impl KeyId {
    /// Parses a key id.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidId`] unless 1 to 64 lower-case hex digits.
    pub fn parse(text: &str) -> Result<Self, ApiError> {
        if text.len() <= 64 && is_lower_hex(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ApiError::InvalidId("not a key id".to_owned()))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An upload's id, and its idempotency key: 32 lower-case hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UploadId(String);

impl UploadId {
    /// A new id from 16 random bytes.
    pub fn generate(rng: &mut dyn RandomSource) -> Self {
        let mut bytes = [0_u8; 16];
        rng.fill(&mut bytes);
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Parses an id a client sent back.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidId`] unless 32 lower-case hex digits.
    pub fn parse(text: &str) -> Result<Self, ApiError> {
        if text.len() == 32 && is_lower_hex(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ApiError::InvalidId("not an upload id".to_owned()))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A workspace's id: 16 lower-case hex digits, minted by the server. It names
/// the workspace's directory and its rows in the control database.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// A new id from 8 random bytes.
    pub fn generate(rng: &mut dyn RandomSource) -> Self {
        let mut bytes = [0_u8; 8];
        rng.fill(&mut bytes);
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Parses an id the control database or a directory name gave.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidId`] unless 16 lower-case hex digits.
    pub fn parse(text: &str) -> Result<Self, ApiError> {
        if text.len() == 16 && is_lower_hex(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ApiError::InvalidId("not a workspace id".to_owned()))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The public id of an API key: what logs and the audit trail show. It is
/// minted by the server, never parsed from a request without a lookup, and
/// holds nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApiKeyId(String);

impl ApiKeyId {
    /// Wraps a key id the control database returned.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_id_is_eight_hex_a_dash_and_twelve_hex() {
        let id = ItemId::parse("6aa52107-2cf24dba5fb0").unwrap();
        assert_eq!(id.as_str(), "6aa52107-2cf24dba5fb0");
        assert_eq!(id.content_key(), "2cf24dba5fb0");
        assert_eq!(id.to_string(), "6aa52107-2cf24dba5fb0");
    }

    #[test]
    fn anything_else_is_not_an_item_id() {
        for bad in [
            "",
            "6aa52107",
            "6aa52107-2cf24dba5fb",
            "6aa52107-2cf24dba5fb00",
            "6AA52107-2cf24dba5fb0",
            "6aa52107_2cf24dba5fb0",
            "6aa5210g-2cf24dba5fb0",
            "../a5210-2cf24dba5fb0",
            "6aa52107-2cf24dba5f/0",
            " 6aa52107-2cf24dba5fb0",
            "6aa52107-2cf2-4dba5fb",
        ] {
            let err = ItemId::parse(bad).unwrap_err();
            assert_eq!(err.code(), "INVALID_ID", "{bad:?}");
        }
    }

    #[test]
    fn item_ids_sort_by_time_because_the_time_comes_first() {
        let old = ItemId::parse("00000001-ffffffffffff").unwrap();
        let new = ItemId::parse("00000002-000000000000").unwrap();
        assert!(old < new);
    }

    #[test]
    fn a_key_id_is_lower_case_hex_of_bounded_length() {
        assert_eq!(KeyId::parse("0a1b").unwrap().as_str(), "0a1b");
        for bad in ["", "0A1b", "xyz", "../", &"a".repeat(65)] {
            assert_eq!(
                KeyId::parse(bad).unwrap_err().code(),
                "INVALID_ID",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_upload_id_is_thirty_two_hex_digits_from_the_random_source() {
        let mut rng = crate::random::SeededRandom::new(7);
        let a = UploadId::generate(&mut rng);
        let b = UploadId::generate(&mut rng);
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert_eq!(UploadId::parse(a.as_str()).unwrap(), a);
        assert_eq!(UploadId::parse("../x").unwrap_err().code(), "INVALID_ID");
    }

    #[test]
    fn an_api_key_id_is_what_logs_show_and_nothing_secret() {
        let id = ApiKeyId::new("7f3k9q2m");
        assert_eq!(id.as_str(), "7f3k9q2m");
    }
}
