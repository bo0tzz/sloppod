-- Which of the previous round's findings a review round found no longer hold (ADR 24).
alter table reviews add column resolved jsonb not null default '[]';

-- Findings from before severities and categories, {path?, line?, severity: high|medium|low, comment}, in the shape
-- reviews now use, so they still read, and a round still running can carry them forward by id.
update reviews r set findings = (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
        'id', r.round || '.' || f.n,
        'severity', case f.finding->>'severity' when 'low' then 'p2' else 'p1' end,
        'category', 'correctness',
        'title', left(split_part(f.finding->>'comment', E'\n', 1), 80),
        'explanation', f.finding->>'comment',
        'path', f.finding->'path',
        'line', f.finding->'line'
    )) order by f.n), '[]'::jsonb)
    from jsonb_array_elements(r.findings) with ordinality as f(finding, n)
)
where exists (select 1 from jsonb_array_elements(r.findings) as f(finding) where not f.finding ? 'id');
