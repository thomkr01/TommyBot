-- Verification's join quarantine (and its time-limit quarantine) is marked, so code no longer reads
-- "nothing saved" as "join quarantine": a staff or Raider List quarantine of a roleless member is a
-- real one, and an autorole landing first does not turn a join quarantine into a real one.
-- Existing rows with nothing saved keep how they were treated until now.
ALTER TABLE quarantines ADD COLUMN on_join BOOLEAN NOT NULL DEFAULT false;
UPDATE quarantines SET on_join = true WHERE cardinality(saved_roles) = 0;
