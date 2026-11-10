-- A sub-session's workspace can start as a copy of its parent's volume (#478): the session it was copied from while
-- that copy still has to be taken over — the child's supervisor puts the checkout on the child's own branch and clears
-- what the parent left of its identity — and null once the child's agent has come up on it, after which the copy is
-- the session's own.
alter table sessions add column workspace_copied_from uuid;

-- A copy that never provisioned — a storage class that can't clone, or a clone that keeps failing — is given up on in
-- two stages, so a retry never undoes what a previous one did: 'dropping' while what the copy left is still in the
-- cluster (the removal is asked for again until the cluster says it's all gone, and nothing is placed onto it), then
-- 'placing' while a fresh workspace is brought up and the bookkeeping finished. From 'placing' on the removal is never
-- repeated, so the volume a retry sees is the session's own. Null when the session isn't waiting for anything.
alter table sessions add column workspace_copy_stage text;
