-- Messages the user took back out of a session's queue, by the id their UI gave them, so a UI that sends one again
-- after losing its connection doesn't run it, across restarts (#82).
create table session_removed_messages (
    session_id uuid not null references sessions (id) on delete cascade,
    message_id uuid not null,
    primary key (session_id, message_id)
);
