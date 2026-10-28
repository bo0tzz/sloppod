-- Review sessions (ADR 24): a session of kind 'review' reviews one proposal across its rounds.
alter table sessions add column kind text not null default 'work';
alter table proposals add column review_session_id uuid references sessions on delete set null;
create table reviews (
    proposal_id uuid not null references proposals on delete cascade,
    round int not null,
    head_sha text not null,
    -- The author's replies to the previous round's findings.
    replies text,
    -- Null while the round runs; then 'pass' or 'changes'.
    verdict text,
    summary text,
    -- [{path, line?, severity, comment}]
    findings jsonb not null default '[]',
    created_at timestamptz not null default now(),
    finished_at timestamptz,
    primary key (proposal_id, round)
);
