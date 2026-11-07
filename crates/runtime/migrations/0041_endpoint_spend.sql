-- What model endpoints say each key has spent (#461), as last read from their `/key/info`: a session's own key (a
-- LiteLLM operator virtual key), or a key several sessions share (an endpoint credential's), named so.
create table endpoint_spend (
    key_id text primary key,
    owner text not null,
    session_id uuid references sessions on delete set null,
    shared_name text,
    spend double precision not null,
    max_budget double precision,
    observed_at timestamptz not null
);
create index endpoint_spend_by_owner on endpoint_spend (owner);

-- The most a session on an endpoint's model may spend, in dollars, where its endpoint keeps a budget per key (#461).
alter table user_settings add column session_budget double precision;
