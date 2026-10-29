-- Links to sessions and proposals name their ids in canonical form (ADR 30, stage 3), so the runtime finds them by
-- text. Links made before held the id as it was written; those Postgres reads as a uuid are rewritten, and one that
-- would then name what another link of the same item already names is a duplicate, and goes.
create temporary table work_link_ids as
select id, item_id, kind,
       case when target ~* '^\{?[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}\}?$'
            then target::uuid::text end as canonical
from work_links
where kind in ('session', 'proposal');

delete from work_links l using work_link_ids c
where l.id = c.id and c.canonical is not null and l.target <> c.canonical
  and exists (select 1 from work_links o where o.item_id = c.item_id and o.kind = c.kind and o.target = c.canonical);

update work_links l set target = c.canonical from work_link_ids c
where l.id = c.id and c.canonical is not null and l.target <> c.canonical;

drop table work_link_ids;

-- Executed pull request and stack proposals whose pull requests have been linked to their session's items. One without
-- a row is linked by the runtime's next pass, so a failure or a restart in between doesn't lose the links.
create table work_followed_proposals (
    proposal_id uuid primary key,
    at timestamptz not null default now()
);
