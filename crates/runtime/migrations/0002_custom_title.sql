-- A title the person chose isn't replaced by the agent's automatic one.
alter table sessions add column custom_title boolean not null default false;
