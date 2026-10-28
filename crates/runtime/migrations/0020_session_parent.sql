-- A session spawned by another, its parent (ADR 27): a sub-session for a piece of the parent's work.
alter table sessions add column parent_id uuid references sessions (id);
create index sessions_by_parent on sessions (parent_id) where parent_id is not null;
