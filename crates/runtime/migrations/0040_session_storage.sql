-- What a session's workspace volume holds, as its supervisor last said (#455), so a parked session still shows it.
alter table sessions add column disk_used bigint;
alter table sessions add column disk_size bigint;
alter table sessions add column disk_seen_at timestamptz;
-- When the session's pod-side objects (its egress policy, its keys) were removed, while it has no pod: parked once
-- its pod is gone, or stopped. Cleared when it's placed again.
alter table sessions add column released_at timestamptz;
-- A stopped session's volume is removed a while after it stopped, unless the user keeps it.
alter table sessions add column volume_kept boolean not null default false;
