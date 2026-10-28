-- Finds the messages a session already received by the id their UI gave them, so sending one again doesn't run it
-- twice (#215).
create index session_events_by_message on session_events (session_id, (payload->>'messageId'))
    where payload->>'sessionUpdate' = 'user_message_chunk';
