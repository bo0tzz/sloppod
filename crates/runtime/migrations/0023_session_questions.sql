-- Questions an agent asked with `ask_user` that the user hasn't answered yet, kept so a runtime restart doesn't lose
-- them (#39). The agent's tool returned when it asked, so nothing else holds them.
create table session_questions (
    id uuid primary key,
    session_id uuid not null references sessions (id) on delete cascade,
    questions jsonb not null,
    created_at timestamptz not null default now()
);
