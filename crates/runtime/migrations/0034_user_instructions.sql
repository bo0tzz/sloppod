-- The user's own instructions for their sessions, like a user-level CLAUDE.md (#342).
alter table user_settings add column instructions text;
