-- Sessions hold grants (ADR 2) instead of a fixed repository, and may have none (ADR 1).
create table session_grants (
    id uuid primary key default gen_random_uuid(),
    session_id uuid not null references sessions on delete cascade,
    capability text not null,
    params jsonb not null default '{}',
    created_at timestamptz not null default now()
);
create index session_grants_by_session on session_grants (session_id);
insert into session_grants (session_id, capability)
    select id, capability from sessions, unnest(array['model', 'github', 'packages']) capability;
insert into session_grants (session_id, capability, params)
    select id, 'repo', jsonb_build_object('repo', repo, 'access', 'rw', 'branch', branch) from sessions;
alter table sessions drop column repo, drop column branch;

-- Everything belongs to the user a request came from (ADR 23).
alter table sessions add column owner text not null default 'default';
alter table sessions alter column owner drop default;
create index sessions_by_owner on sessions (owner, updated_at);
alter table proposals add column decided_by text;

-- Accounts and keys used on a user's behalf, encrypted with the runtime's master key (ADR 21).
create table credentials (
    id uuid primary key,
    owner text not null,
    kind text not null,
    name text not null,
    config jsonb not null default '{}',
    secret bytea not null,
    is_default boolean not null default false,
    created_at timestamptz not null default now(),
    unique (owner, kind, name)
);

-- Secrets minted for one session, e.g. its memini key (ADR 22).
create table session_secrets (
    session_id uuid not null references sessions on delete cascade,
    name text not null,
    secret bytea not null,
    primary key (session_id, name)
);
