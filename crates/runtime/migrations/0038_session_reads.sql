-- How far a user has read a session (#48): the seq of the last event they scrolled past, kept per user and per session so
-- it follows them between their browser and their phone. It only moves forward.
create table session_reads (
    session_id uuid not null references sessions on delete cascade,
    owner text not null,
    seq bigint not null,
    updated_at timestamptz not null default now(),
    primary key (session_id, owner)
);
