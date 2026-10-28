-- What the session's pod is doing while its agent starts, and why the last start failed, so the UI can say (#59).
-- Both are cleared once the agent is ready.
alter table sessions
    add column agent_status text,
    add column agent_error text;
