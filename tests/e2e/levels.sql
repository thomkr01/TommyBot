-- Access levels for the mock users in mock_discord.py (run after the dashboard has migrated the DB).
delete from extra_owners where guild_id = 100000000000000001;
delete from trusted_admins where guild_id = 100000000000000001;
delete from permits where guild_id = 100000000000000001 and name like 'e2e %';
insert into extra_owners values (100000000000000001, 200000000000000002);
insert into trusted_admins values (100000000000000001, 200000000000000003);
-- Ban Sync: both mock servers belong to the owner x, and one copy shows in the Log tab.
insert into guild_owners (guild_id, owner_id) values (100000000000000001, 200000000000000001),
  (100000000000000002, 200000000000000001) on conflict (guild_id) do update set owner_id = excluded.owner_id;
insert into ban_sync_links (guild_id, user_id, source_guild, origin, reason, state) values
  (100000000000000001, 200000000000000042, 100000000000000002, 'staff_cmd', 'e2e spam', 'alerted') on conflict do nothing;
with p as (insert into permits (guild_id, name, rank, perms) values
  (100000000000000001, 'e2e editor', 3, '{dashboard_view,dashboard_edit}'),
  (100000000000000001, 'e2e viewer', 2, '{dashboard_view}'),
  (100000000000000001, 'e2e cases', 1, '{dashboard_view,cases,notes_view}') returning id, name)
insert into permit_roles select id, 100000000000000001,
  case name when 'e2e editor' then 400000000000000001 when 'e2e viewer' then 400000000000000002 else 400000000000000003 end from p;
