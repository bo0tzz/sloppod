-- Work graphs (ADR 30): an owner's items, what blocks what, the links that carry each item out, and every change.
create table work_graphs (
    id uuid primary key,
    owner text not null,
    name text not null,
    created_at timestamptz not null default now(),
    unique (owner, name)
);

-- An item's `part of` edge is its parent: an item is part of at most one other. Its rank orders it among its siblings.
create table work_items (
    id uuid primary key,
    graph_id uuid not null references work_graphs on delete cascade,
    parent_id uuid references work_items on delete cascade,
    title text not null,
    description text not null default '',
    state text not null default 'open' check (state in ('open', 'active', 'done', 'dropped')),
    -- Whether the rules set the state, following links and parts, rather than someone by hand.
    state_by_rules boolean not null default false,
    rank double precision not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);
create index on work_items (graph_id);
create index on work_items (parent_id);

-- `blocks` edges: the blocked item can't start until the blocker is done or dropped.
create table work_edges (
    graph_id uuid not null references work_graphs on delete cascade,
    blocker_id uuid not null references work_items on delete cascade,
    blocked_id uuid not null references work_items on delete cascade,
    created_at timestamptz not null default now(),
    primary key (blocker_id, blocked_id),
    check (blocker_id <> blocked_id)
);
create index on work_edges (blocked_id);

-- What carries an item out or relates to it. `target` is `owner/repo#n` for issues and pull requests, an id otherwise.
create table work_links (
    id uuid primary key,
    item_id uuid not null references work_items on delete cascade,
    kind text not null check (kind in ('issue', 'pull_request', 'session', 'proposal')),
    target text not null,
    role text not null check (role in ('completes', 'related')),
    status text not null default 'open' check (status in ('open', 'finished', 'abandoned')),
    created_at timestamptz not null default now(),
    unique (item_id, kind, target)
);

-- Every change to a graph, which can be undone by an inverse change (`undoes`). `automatic` marks state changes the
-- rules made, which follow their causes rather than being undone. Sessions are purged, so `session_id` is no reference.
create table work_events (
    id uuid primary key,
    graph_id uuid not null references work_graphs on delete cascade,
    item_id uuid references work_items on delete cascade,
    actor text not null,
    session_id uuid,
    kind text not null,
    before jsonb,
    after jsonb,
    automatic boolean not null default false,
    undoes uuid references work_events on delete set null,
    at timestamptz not null default now()
);
create index on work_events (graph_id, at);
create index on work_events (item_id, at);
