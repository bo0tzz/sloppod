-- A sub-session's workspace can start as a copy of its parent's volume (#478): the session it was copied from while
-- that copy still has to be taken over — the child's supervisor puts the checkout on the child's own branch and clears
-- what the parent left of its identity — and null once the child's agent has come up on it, after which the copy is
-- the session's own.
alter table sessions add column workspace_copied_from uuid;

-- A copy that never provisioned — a storage class that can't clone, or a clone that keeps failing — is given up on:
-- the session waits for a fresh placement with this set, cleared once that placement has happened, so a deletion or a
-- placement that fails is tried again rather than leaving the session with no objects at all.
alter table sessions add column workspace_fresh boolean not null default false;
