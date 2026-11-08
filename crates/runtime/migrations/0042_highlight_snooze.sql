-- A highlight the user put off until later (#467): it stops counting as waiting on them and comes back to the "for you"
-- list at this time. Only what the UI sets; the agent isn't told, and a snoozed highlight is still the agent's to retract
-- (it counts as not seen).
alter table highlights
    add column snoozed_until timestamptz;
