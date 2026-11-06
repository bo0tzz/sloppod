-- A highlight the agent took back (#439): it leaves the user's list and what the session waits on, and stays in the chat
-- with why. Only while the user hasn't marked it seen.
alter table highlights
    add column retracted_at timestamptz,
    add column retraction_reason text;
