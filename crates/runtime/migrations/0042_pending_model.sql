-- A model the user picked that needs the session's agent started again (#461): it's moved onto it before its next
-- message is sent.
alter table sessions add column pending_model text;
