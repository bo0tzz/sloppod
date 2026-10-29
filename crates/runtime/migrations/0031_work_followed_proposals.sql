-- Executed pull request and stack proposals whose pull requests have been linked to their session's items (ADR 30,
-- stage 3). One without a row is linked by the runtime's next pass, however long it's been, so a failure or a restart
-- in between doesn't lose the links. Those carried out before this are marked as they are: they predate following.
create table work_followed_proposals (
    proposal_id uuid primary key,
    at timestamptz not null default now()
);

insert into work_followed_proposals (proposal_id)
select id from proposals where kind in ('github.pr.create', 'github.stack.create') and status = 'executed';
