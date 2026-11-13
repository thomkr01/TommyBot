-- Thomas 2026-11-13 22:49: only permit holders act through TommyBot. A Discord AutoMod rule last
-- created or edited by someone without one triggers no TommyBot action until a trusted edit.
CREATE TABLE automod_rule_untrusted (
    guild_id BIGINT NOT NULL,
    rule_id BIGINT NOT NULL,
    actor BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, rule_id)
);
