-- When a session that hit its usage limit carries on by itself; none while it isn't waiting for one (#251).
alter table sessions add column limited_until timestamptz;
-- How long a review round's reviewer has waited for its usage limit; the round's time limit doesn't count it.
alter table reviews add column paused_seconds bigint not null default 0;
