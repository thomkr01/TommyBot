-- Verification's join quarantine (and its time-limit quarantine) is marked, so code no longer reads
-- "nothing saved" as "join quarantine": a staff or Raider List quarantine of a roleless member is a
-- real one, and an autorole landing first does not turn a join quarantine into a real one.
-- Existing rows with nothing saved keep how they were treated until now, unless a quarantine case
-- (anti-nuke, staff, the Raider List) or a Raider List quarantine in joingate_actions shows a real
-- one since the row was written. Verification writes neither; action_log cannot tell the kinds
-- apart (every quarantine logs there), and joingate_actions keeps only a guild's newest 1000 rows.
ALTER TABLE quarantines ADD COLUMN on_join BOOLEAN NOT NULL DEFAULT false;
UPDATE quarantines q SET on_join = true
WHERE cardinality(q.saved_roles) = 0
  AND NOT EXISTS (SELECT 1 FROM cases c
                  WHERE c.guild_id = q.guild_id AND c.user_id = q.user_id AND c.kind = 'quarantine'
                    AND c.created_at >= q.created_at - interval '1 minute')
  AND NOT EXISTS (SELECT 1 FROM joingate_actions j
                  WHERE j.guild_id = q.guild_id AND j.user_id = q.user_id
                    AND j.action = 'Quarantine' AND j.ok
                    AND j.created_at >= q.created_at - interval '1 minute');
