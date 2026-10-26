create table sessions (
    id uuid primary key,
    title text not null,
    repo text not null,
    branch text not null,
    phase text not null default 'starting',
    agent_session_id text,
    agent_state jsonb,
    pending_prompt text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    stopped_at timestamptz
);

create table session_events (
    seq bigserial primary key,
    session_id uuid not null references sessions on delete cascade,
    payload jsonb not null,
    created_at timestamptz not null default now()
);
create index session_events_by_session on session_events (session_id, seq);

create table transcripts (
    session_id uuid not null references sessions on delete cascade,
    name text not null,
    content bytea not null,
    updated_at timestamptz not null default now(),
    primary key (session_id, name)
);

create table proposals (
    id uuid primary key,
    session_id uuid not null references sessions on delete cascade,
    kind text not null,
    status text not null,
    payload jsonb not null,
    result jsonb,
    error text,
    reason text,
    created_at timestamptz not null default now(),
    decided_at timestamptz
);
create index proposals_by_session on proposals (session_id, created_at);
