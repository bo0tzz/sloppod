-- A grant can run out (ADR 2): past its expiry it stops counting, wherever a session's grants are read.
alter table session_grants add column expires_at timestamptz;
