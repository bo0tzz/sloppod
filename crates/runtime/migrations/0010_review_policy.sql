-- Whether proposals are reviewed by another model before a human sees them (ADR 24): 'off' or 'required'.
alter table sessions add column review text not null default 'off';
alter table user_settings add column review text not null default 'off';
