-- The control database's schema version 1, exactly as commit a7260ad
-- (PLAN-00002) created it. A fixture: never edit it. Migrations are tested
-- against the databases operators really have.
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
