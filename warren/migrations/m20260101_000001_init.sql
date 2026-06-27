CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS agents (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,
    class       TEXT NOT NULL,
    type        TEXT,
    model       TEXT NOT NULL,
    authtoken   TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS agents_class_type_idx ON agents (class, type);

CREATE TABLE IF NOT EXISTS requests (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_class  TEXT NOT NULL,
    target_type   TEXT,
    payload       JSONB NOT NULL,
    response      JSONB,
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending','approved','rejected','responded')),
    claimed_by    UUID REFERENCES agents(id),
    claimed_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    responded_at  TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS requests_inbox_idx
    ON requests (target_class, target_type, status)
    WHERE status = 'approved' AND claimed_by IS NULL;

CREATE INDEX IF NOT EXISTS requests_status_idx ON requests (status, created_at DESC);

CREATE TABLE IF NOT EXISTS memos (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_class  TEXT NOT NULL,
    target_type   TEXT,
    payload       JSONB NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending','approved','rejected')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS memos_inbox_idx
    ON memos (target_class, target_type, status)
    WHERE status = 'approved';

CREATE INDEX IF NOT EXISTS memos_status_idx ON memos (status, created_at DESC);

CREATE TABLE IF NOT EXISTS memo_acks (
    memo_id         UUID NOT NULL REFERENCES memos(id) ON DELETE CASCADE,
    agent_id        UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    acknowledged_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (memo_id, agent_id)
);

CREATE TABLE IF NOT EXISTS admin_sessions (
    token       TEXT PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS admin_sessions_expires_idx ON admin_sessions (expires_at);
