-- Deleting a session parks it and marks it deleted; it can be restored until it's purged a week later (#118).
alter table sessions add column deleted_at timestamptz;
