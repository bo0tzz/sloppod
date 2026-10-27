-- The repository a session is about (ADR 1), checked out as its working directory. Repositories granted later add
-- push access without changing it.
alter table sessions add column repo text;
update sessions set repo = (
    select g.params->>'repo' from session_grants g
    where g.session_id = sessions.id and g.capability = 'repo'
    order by g.created_at limit 1
);
