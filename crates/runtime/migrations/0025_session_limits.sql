-- When a session that hit its usage limit carries on by itself; none while it isn't waiting for one (#251).
alter table sessions add column limited_until timestamptz;
