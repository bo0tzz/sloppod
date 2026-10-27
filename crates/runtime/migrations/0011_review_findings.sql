-- Which of the previous round's findings a review round found no longer hold (ADR 24).
alter table reviews add column resolved jsonb not null default '[]';
