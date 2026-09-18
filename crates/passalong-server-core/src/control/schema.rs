//! The control database's schema, as the steps that made it.
//!
//! Step `n` takes a database from version `n` to `n + 1`; step 0 creates
//! version 1 from nothing. A step is never edited once a release has run it:
//! operators' databases were made by it, and
//! `tests/fixtures/schema_v1.sql` holds version 1 as it was, for the tests
//! that migrate from it. A new version is a new step at the end.

/// The steps, in order. The schema version is how many there are.
pub(crate) const STEPS: &[&str] = &[V1, V2, V3];

const V1: &str = "
CREATE TABLE schema_version (version INTEGER NOT NULL);
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL,
    plain INTEGER,
    next_generation INTEGER NOT NULL,
    -- The seal readers go by; during a rewrite, the one before it.
    seal_key TEXT,
    seal_header BLOB,
    -- The open rewrite session; rw_kind is NULL when there is none.
    rw_kind TEXT,
    rw_next_key TEXT,
    rw_next_header BLOB,
    rw_holder TEXT,
    rw_lease_expires_at INTEGER,
    rw_staged_generation INTEGER
) STRICT;
CREATE TABLE generations (
    workspace TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    used_bytes INTEGER NOT NULL,
    PRIMARY KEY (workspace, generation)
) STRICT;
CREATE TABLE uploads (
    workspace TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    upload_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    item_id TEXT NOT NULL,
    meta BLOB NOT NULL,
    size INTEGER NOT NULL,
    expected_key TEXT,
    in_rewrite INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (workspace, upload_id)
) STRICT;
CREATE TABLE tombstones (
    workspace TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    upload_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    item_id TEXT NOT NULL,
    created INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (workspace, upload_id)
) STRICT;
CREATE TABLE ended_rewrites (
    workspace TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL,
    PRIMARY KEY (workspace, key_id)
) STRICT;
";

/// Workspaces get a name, a quota, and a birthday; API keys and the audit
/// trail arrive. A workspace from version 1 gets its id as its name, and a
/// NULL quota, which means the configured default.
const V2: &str = "
ALTER TABLE workspaces ADD COLUMN name TEXT;
ALTER TABLE workspaces ADD COLUMN quota_bytes INTEGER;
ALTER TABLE workspaces ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
UPDATE workspaces SET name = id;
CREATE UNIQUE INDEX workspaces_name ON workspaces (name);
CREATE TABLE api_keys (
    key_id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- SHA-256 of the secret. The secret itself is nowhere.
    secret_hash BLOB NOT NULL,
    label TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    revoked_at INTEGER,
    last_used_at INTEGER
) STRICT;
CREATE INDEX api_keys_workspace ON api_keys (workspace);
CREATE TABLE audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    action TEXT NOT NULL,
    workspace TEXT,
    key_id TEXT,
    detail TEXT
) STRICT;
";

/// A remembered upload outcome says whether its item was staged for a
/// rewrite. Outcomes from before are of the workspace's items or, if they
/// were not, are forgotten within the hour anyway.
const V3: &str = "
ALTER TABLE tombstones ADD COLUMN staged INTEGER NOT NULL DEFAULT 0;
";
