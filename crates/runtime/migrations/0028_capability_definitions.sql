-- Capabilities an owner defines themselves (ADR 2, 2026-10-29): so far, MCP servers the runtime serves to the
-- sessions granted them, with a credential attached outside the pod.
create table capability_definitions (
    id uuid primary key,
    owner text not null,
    name text not null,
    description text not null,
    mcp_url text not null,
    credential_id uuid references credentials (id) on delete set null,
    created_at timestamptz not null default now(),
    unique (owner, name)
);
