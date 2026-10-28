-- When something last happened in a session (an event was recorded), for ordering the session list. Unlike
-- updated_at, phase changes (e.g. every session reconnecting after a deploy) don't move it.
alter table sessions add column active_at timestamptz;
update sessions set active_at = coalesce(
    (select max(created_at) from session_events e where e.session_id = sessions.id),
    created_at
);
alter table sessions alter column active_at set not null, alter column active_at set default now();
