-- Thomas 2026-11-13 23:31: TommyBot acts on a Discord AutoMod rule's hits only while the rule is
-- in a state a trusted person approved. `fingerprint` hashes everything that decides hits
-- (automod `discord_api::rule_fingerprint`). `approved_by` 0 is the one approval at this update;
-- the row with rule_id 0 marks a guild whose existing rules got it.
CREATE TABLE automod_rule_approved (
    guild_id BIGINT NOT NULL,
    rule_id BIGINT NOT NULL,
    fingerprint TEXT NOT NULL,
    approved_by BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, rule_id)
);
