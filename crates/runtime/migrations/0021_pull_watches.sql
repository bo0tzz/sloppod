-- What the runtime last saw of each pull request it opened, so it tells the session only what changed, across
-- restarts (#107).
create table pull_watches (
    repo text not null,
    number bigint not null,
    session_id uuid not null references sessions (id) on delete cascade,
    observed jsonb not null,
    updated_at timestamptz not null default now(),
    primary key (repo, number)
);
