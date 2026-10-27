-- Pre-approved proposal patterns (ADR 3): matching proposals are approved as they're made.
create table standing_approvals (
    id uuid primary key,
    owner text not null,
    kind text not null,
    -- What the proposal acts on, e.g. a repository as owner/name; null matches any.
    target text,
    -- One session, or null for all of the owner's sessions.
    session_id uuid references sessions on delete cascade,
    -- Edits applied when approving, e.g. {"auto_merge": true}.
    edits jsonb not null default '{}',
    expires_at timestamptz,
    created_at timestamptz not null default now(),
    revoked_at timestamptz
);
create index standing_approvals_by_owner on standing_approvals (owner) where revoked_at is null;
