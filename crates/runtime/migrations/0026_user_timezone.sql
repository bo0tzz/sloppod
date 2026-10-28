-- The user's timezone, as an IANA name, for the times agents are told and the pods' clocks (#253).
alter table user_settings add column timezone text;
