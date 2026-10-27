-- Deleting a session parks it and marks it deleted; it can be restored until it's purged a week later (#118). A purge
-- claims the session first, so a restore can't race it, and is retried until it has removed everything.
alter table sessions
    add column deleted_at timestamptz,
    add column purge_started_at timestamptz,
    add column purged_at timestamptz;
