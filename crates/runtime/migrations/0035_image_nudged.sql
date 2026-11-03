-- The session image a session's agent was last asked to make room for (#230, #416), so a runtime restart doesn't ask
-- again, and the session's pod is replaced at its first quiet moment rather than after a long quiet.
alter table sessions add column image_nudged text;
