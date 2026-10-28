-- Messages and notices queued for a session's agent, kept so a runtime restart doesn't lose them (#39). One row per
-- session with anything queued: the whole queue, in order, as `[{id, blocks}]`.
create table session_queues (
    session_id uuid primary key references sessions (id) on delete cascade,
    messages jsonb not null,
    updated_at timestamptz not null default now()
);
