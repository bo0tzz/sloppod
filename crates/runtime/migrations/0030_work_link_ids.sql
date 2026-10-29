-- Links to sessions and proposals name their ids in canonical form (ADR 30, stage 3), so the runtime finds them by
-- text. Links made before held the id as it was written. Those Postgres reads as a uuid are rewritten; where an item
-- has several that name the same id, the canonical one stays (or else the oldest) and the others go.
create temporary table work_link_ids as
select id, item_id, kind, target,
       case when target ~* '^\{?[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}\}?$'
            then target::uuid::text end as canonical
from work_links
where kind in ('session', 'proposal');

delete from work_links l using (
    select id, row_number() over (
        partition by item_id, kind, canonical order by (target = canonical) desc, id
    ) as nth
    from work_link_ids where canonical is not null
) ranked
where l.id = ranked.id and ranked.nth > 1;

update work_links l set target = c.canonical from work_link_ids c
where l.id = c.id and c.canonical is not null and l.target <> c.canonical;

drop table work_link_ids;
