-- The agent a session runs, by adapter name (ADR 9).
alter table sessions add column agent text not null default 'claude-code';
