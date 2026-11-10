-- Starting sub-sessions is a grant of its own (#495): every top-level session gets it, in the shape a new session's
-- starting set writes it. Sub-sessions, reviewers and utility sessions don't; a sub-session gets it only when its
-- parent allows it. Deleted sessions are left alone.
insert into session_grants (session_id, capability)
    select s.id, 'subsessions' from sessions s
    where s.parent_id is null and s.kind = 'work' and s.deleted_at is null
      and not exists (select 1 from session_grants g
                      where g.session_id = s.id and g.capability = 'subsessions');
