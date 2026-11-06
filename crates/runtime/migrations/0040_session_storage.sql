-- What a session's workspace volume holds, as its supervisor last said (#455), so a parked session still shows it.
alter table sessions add column disk_used bigint;
alter table sessions add column disk_size bigint;
alter table sessions add column disk_seen_at timestamptz;
-- When the session's pod-side objects (its egress policy, its keys) were removed, while it has no pod: parked once
-- its pod is gone, or stopped. Cleared when it's placed again.
alter table sessions add column released_at timestamptz;
-- A stopped session's volume is removed a while after it stopped, unless the user keeps it.
alter table sessions add column volume_kept boolean not null default false;
-- A stopped session's volume removal: claimed, and done. Kept apart from a deleted session's purge, so a stopped
-- session whose volume went can still be deleted and restored like any other.
alter table sessions add column volume_removal_started_at timestamptz;
alter table sessions add column volume_removed_at timestamptz;
