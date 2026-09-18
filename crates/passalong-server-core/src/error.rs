//! The API's errors: one variant per stable error code of `docs/api/`.

use std::fmt;

/// A refusal, as the API reports it. `docs/api/README.md` lists the codes;
/// the client decides from the code alone whether to retry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// No key, a malformed one, an unknown one, or a wrong secret: which of
    /// them is not said.
    Unauthenticated,
    /// The key's time is up. A client stops retrying and says so.
    KeyExpired,
    /// The key was revoked.
    KeyRevoked,
    /// A read-only key tried to write.
    ForbiddenRole,
    /// The request was made under a data key that is not the workspace's.
    KeyIdMismatch,
    /// Encryption is being changed; ordinary writes wait.
    RewriteInProgress,
    /// Another key holds the rewrite session's lease.
    LeaseHeld,
    /// `commitRewrite` before every source item was staged.
    RewriteIncomplete {
        /// Items staged so far.
        staged: usize,
        /// Items the source generation holds.
        source: usize,
    },
    /// `beginRewrite` names the new key id of a rewrite that was aborted:
    /// a duplicate of an old request, not a new attempt.
    RewriteEnded,
    /// The workspace is full.
    QuotaExceeded,
    /// Larger than `limits.max_item_bytes`.
    ItemTooLarge,
    /// The content's size differs from what `beginUpload` announced.
    ContentMismatch,
    /// No such item, upload, or session.
    NotFound,
    /// An identifier that does not parse.
    InvalidId(String),
    /// A request that makes no sense in the workspace's state.
    InvalidRequest(String),
    /// Too many failed authentications from the caller's address.
    RateLimited,
    /// The control database or the data directory cannot be used. The
    /// server fails closed: it refuses, and never answers from memory.
    ServiceUnavailable,
}

impl ApiError {
    /// The stable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::KeyExpired => "KEY_EXPIRED",
            Self::KeyRevoked => "KEY_REVOKED",
            Self::ForbiddenRole => "FORBIDDEN_ROLE",
            Self::KeyIdMismatch => "KEY_ID_MISMATCH",
            Self::RewriteInProgress => "REWRITE_IN_PROGRESS",
            Self::LeaseHeld => "LEASE_HELD",
            Self::RewriteIncomplete { .. } => "REWRITE_INCOMPLETE",
            Self::RewriteEnded => "REWRITE_ENDED",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::ItemTooLarge => "ITEM_TOO_LARGE",
            Self::ContentMismatch => "CONTENT_MISMATCH",
            Self::NotFound => "NOT_FOUND",
            Self::InvalidId(_) => "INVALID_ID",
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::RateLimited => "RATE_LIMITED",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
        }
    }

    /// The HTTP status the code travels with.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthenticated | Self::KeyExpired | Self::KeyRevoked => 401,
            Self::ForbiddenRole => 403,
            Self::KeyIdMismatch
            | Self::RewriteInProgress
            | Self::LeaseHeld
            | Self::RewriteIncomplete { .. }
            | Self::RewriteEnded => 409,
            Self::QuotaExceeded | Self::ItemTooLarge => 413,
            Self::ContentMismatch => 422,
            Self::NotFound => 404,
            Self::InvalidId(_) | Self::InvalidRequest(_) => 400,
            Self::RateLimited => 429,
            Self::ServiceUnavailable => 503,
        }
    }

    /// Whether sending the same request again later can succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RewriteInProgress
                | Self::LeaseHeld
                | Self::RateLimited
                | Self::ServiceUnavailable
        )
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("no valid API key"),
            Self::KeyExpired => f.write_str("this API key has expired"),
            Self::KeyRevoked => f.write_str("this API key was revoked"),
            Self::ForbiddenRole => f.write_str("this key is read-only"),
            Self::KeyIdMismatch => f.write_str("the workspace's data key is another one"),
            Self::RewriteInProgress => f.write_str("the workspace's encryption is being changed"),
            Self::LeaseHeld => f.write_str("another key holds the rewrite session"),
            Self::RewriteIncomplete { staged, source } => {
                write!(f, "{staged} of {source} items are staged")
            }
            Self::RewriteEnded => {
                f.write_str("that rewrite was aborted; begin again under a new key")
            }
            Self::QuotaExceeded => f.write_str("the workspace is full"),
            Self::ItemTooLarge => f.write_str("the item is larger than this server accepts"),
            Self::ContentMismatch => f.write_str("the content is not what was announced"),
            Self::NotFound => f.write_str("not found"),
            Self::InvalidId(reason) | Self::InvalidRequest(reason) => f.write_str(reason),
            Self::RateLimited => f.write_str("too many failed authentications from this address"),
            Self::ServiceUnavailable => f.write_str("the server cannot reach its storage"),
        }
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_code_a_status_and_a_retry_rule() {
        let cases = [
            (ApiError::ForbiddenRole, "FORBIDDEN_ROLE", 403, false),
            (ApiError::KeyIdMismatch, "KEY_ID_MISMATCH", 409, false),
            (
                ApiError::RewriteInProgress,
                "REWRITE_IN_PROGRESS",
                409,
                true,
            ),
            (ApiError::LeaseHeld, "LEASE_HELD", 409, true),
            (
                ApiError::RewriteIncomplete {
                    staged: 1,
                    source: 2,
                },
                "REWRITE_INCOMPLETE",
                409,
                false,
            ),
            (ApiError::RewriteEnded, "REWRITE_ENDED", 409, false),
            (ApiError::QuotaExceeded, "QUOTA_EXCEEDED", 413, false),
            (ApiError::ItemTooLarge, "ITEM_TOO_LARGE", 413, false),
            (ApiError::ContentMismatch, "CONTENT_MISMATCH", 422, false),
            (ApiError::NotFound, "NOT_FOUND", 404, false),
            (ApiError::RateLimited, "RATE_LIMITED", 429, true),
            (ApiError::InvalidId("x".into()), "INVALID_ID", 400, false),
            (
                ApiError::InvalidRequest("x".into()),
                "INVALID_REQUEST",
                400,
                false,
            ),
            (ApiError::Unauthenticated, "UNAUTHENTICATED", 401, false),
            (ApiError::KeyExpired, "KEY_EXPIRED", 401, false),
            (ApiError::KeyRevoked, "KEY_REVOKED", 401, false),
            (
                ApiError::ServiceUnavailable,
                "SERVICE_UNAVAILABLE",
                503,
                true,
            ),
        ];
        for (err, code, status, retry) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.http_status(), status, "{code}");
            assert_eq!(err.retryable(), retry, "{code}");
            assert!(!err.to_string().is_empty());
        }
    }
}
