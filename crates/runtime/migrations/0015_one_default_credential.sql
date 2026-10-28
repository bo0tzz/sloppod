-- An owner has at most one default credential of a kind. Concurrent changes could leave two; the oldest stays.
update credentials c
set is_default = false
where c.is_default
  and exists (
    select 1 from credentials d
    where d.owner = c.owner and d.kind = c.kind and d.is_default
      and (d.created_at, d.id) < (c.created_at, c.id)
  );

create unique index credentials_one_default on credentials (owner, kind) where is_default;
