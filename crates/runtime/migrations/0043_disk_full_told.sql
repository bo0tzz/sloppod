-- Whether the session has been told its workspace volume is nearly full and can't grow any more (#479), so it's
-- told once rather than at every disk report. Cleared once its use drops below the threshold again, so a workspace
-- that fills up anew is worth saying again.
alter table sessions add column disk_full_told boolean not null default false;
