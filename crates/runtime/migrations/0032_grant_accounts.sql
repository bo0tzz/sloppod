-- Model grants name the accounts they act with, in order (ADR 29): those made before this get all of their owner's
-- accounts of each kind, the default first, as a grant made now would.
update session_grants g set params = g.params || jsonb_build_object('credentials', (
    select jsonb_agg(c.id::text order by c.is_default desc, c.created_at)
    from credentials c join sessions s on s.owner = c.owner
    where s.id = g.session_id
      and c.kind = case g.capability when 'model' then 'claude' else 'chatgpt' end))
where g.capability in ('model', 'codex') and not g.params ? 'credentials'
  and exists (select 1 from credentials c join sessions s on s.owner = c.owner
    where s.id = g.session_id
      and c.kind = case g.capability when 'model' then 'claude' else 'chatgpt' end);
