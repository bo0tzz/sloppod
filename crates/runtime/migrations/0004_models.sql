-- Per-user defaults for new sessions, and each session's current model (sessions always run in auto mode).
create table user_settings (
    owner text primary key,
    model text,
    updated_at timestamptz not null default now()
);
alter table sessions add column model text;
