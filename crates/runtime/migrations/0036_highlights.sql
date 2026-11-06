-- What an agent asked the user to see with its `highlight` tool (#439): each stays open until the user marks it seen,
-- which is the UI's alone (no turn starts, the agent isn't told).
create table highlights (
    id uuid primary key,
    session_id uuid not null references sessions on delete cascade,
    text text not null,
    created_at timestamptz not null default now(),
    seen_at timestamptz
);
create index highlights_by_session on highlights (session_id, created_at);
