-- The latest of each inference credential's usage limits, as its agents and provider report them (#140).
create table credential_quotas (
    credential_id uuid primary key references credentials (id) on delete cascade,
    -- Its requests are refused, until resets_at.
    limited boolean not null,
    resets_at timestamptz,
    -- By window name, e.g. five_hour: {utilization, resets_at, status}. An observation replaces the windows it has.
    windows jsonb not null,
    observed_at timestamptz not null
);
