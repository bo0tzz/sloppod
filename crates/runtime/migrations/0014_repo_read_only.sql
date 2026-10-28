-- Review sessions' repository grants said `access: "r"`; read-only access is `ro` (ADR 2).
update session_grants
set params = jsonb_set(params, '{access}', '"ro"')
where capability = 'repo' and params->>'access' = 'r';
