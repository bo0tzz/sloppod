-- The model the user's sub-sessions default to (#283): a model id, `parent` for their parent's, or null for the
-- agent's cheaper model (Sonnet for Claude Code).
alter table user_settings add column sub_session_model text;
