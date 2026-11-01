"""Attack scenarios against a real test server, with the real bot running (see live.sh).
Usage: live.py spam|nuke|logchan|purge|all. Needs DISCORD_TOKEN, TEST_GUILD_ID, DATABASE_URL; nuke also needs
ATTACKER_TOKEN, a second bot on the test server with Administrator. Stdlib + psql only.
Exits non-zero if the bot did not react. Never prints tokens."""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "https://discord.com/api/v10"
GUILD = os.environ["TEST_GUILD_ID"]


def call(method, path, token=None, body=None, auth=True, soft=False):
    """One REST call; waits out 429s. Returns parsed JSON; soft calls return None on 4xx."""
    headers = {"Content-Type": "application/json", "User-Agent": "DiscordBot (tommybot-e2e, 1)"}
    if auth:
        headers["Authorization"] = f"Bot {token or os.environ['DISCORD_TOKEN']}"
    data = json.dumps(body).encode() if body is not None else None
    while True:
        req = urllib.request.Request(API + path, data, headers, method=method)
        try:
            with urllib.request.urlopen(req) as r:
                raw = r.read()
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(json.load(e).get("retry_after", 1) + 0.1)
                continue
            if soft and e.code < 500:
                return None
            # Unauthenticated paths carry a webhook token: keep it out of the output.
            raise SystemExit(f"{method} {path if auth else '/webhooks/...'}: HTTP {e.code} {e.read()[:200]!r}")


def sql(q):
    return subprocess.run(["psql", os.environ["DATABASE_URL"], "-Atc", q],
                          check=True, capture_output=True, text=True).stdout.strip()


def set_config(section, value):
    """Merges one section into the newest guild config, then tells the bot to reload."""
    sql(f"""insert into guild_config (guild_id, version, config)
            select {GUILD}, coalesce(max(version), 0) + 1,
                   coalesce((select config from guild_config where guild_id = {GUILD}
                             order by version desc limit 1), '{{}}') || '{json.dumps({section: value})}'
            from guild_config where guild_id = {GUILD};
            select pg_notify('config_changed', '{GUILD}');""")
    time.sleep(2)


def logging_cfg():
    return json.loads(sql(f"select coalesce(config->'logging', '{{}}') from guild_config "
                          f"where guild_id = {GUILD} order by version desc limit 1") or "{}")


def text_channel(name):
    chans = call("GET", f"/guilds/{GUILD}/channels")
    return next(c["id"] for c in chans if c["type"] == 0 and c["name"] == name)


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what)
    return ok


def snowflake_now():
    return str((int(time.time() * 1000) - 1420070400000) << 22)


def log_messages(kind, since):
    """Messages after `since` in the log channel for `kind`: its route, else the main one."""
    log = logging_cfg()
    chan = log.get("routes", {}).get(kind) or log.get("fallback")
    return call("GET", f"/channels/{chan}/messages?after={since}&limit=50", soft=True) or []


def logged(kind, since, title=None):
    return any(title in (None, e.get("title")) for m in log_messages(kind, since) for e in m["embeds"])


def spam():
    """Webhook floods #general; automod must delete the flood and the new webhook."""
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True})
    since = snowflake_now()
    chan = text_channel("general")
    # The bot never deletes its own webhooks, so the attacker makes it when it is on the server.
    owner = os.environ.get("ATTACKER_TOKEN")
    hook = (call("POST", f"/channels/{chan}/webhooks", owner, {"name": "e2e-spammer"}, soft=True)
            or call("POST", f"/channels/{chan}/webhooks", body={"name": "e2e-spammer"}))
    url = f"/webhooks/{hook['id']}/{hook['token']}?wait=true"
    ids = []
    for _ in range(12):
        sent = call("POST", url, body={"content": "SPAM SPAM SPAM buy now"}, auth=False, soft=True)
        if sent is None:
            break
        ids.append(sent["id"])
    # The bot's deletes queue behind Discord's rate limit, so give it a while.
    for _ in range(10):
        time.sleep(3)
        if call("GET", f"/webhooks/{hook['id']}/{hook['token']}", auth=False, soft=True) is None:
            break
    recent = {m["id"] for m in call("GET", f"/channels/{chan}/messages?limit=50")}
    left = [i for i in ids if i in recent]
    hook_gone = call("GET", f"/webhooks/{hook['id']}/{hook['token']}", auth=False, soft=True) is None
    call("DELETE", f"/webhooks/{hook['id']}", soft=True)
    for i in left:
        call("DELETE", f"/channels/{chan}/messages/{i}", soft=True)
    # The first few messages stay below the heat limit and survive.
    print(f"cleaned: webhook e2e-spammer and {len(left)} leftover messages in #general")
    return check(hook_gone and len(left) <= 6 and logged("auto_mod", since),
                 f"spam: webhook deleted={hook_gone}, {len(left)}/{len(ids)} messages left, automod logged")


def nuke():
    """A second bot mass-creates and deletes channels; antinuke must open an incident."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP nuke: ATTACKER_TOKEN not set")
        return True
    before = sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}")
    since = snowflake_now()
    # Soft: once the bot quarantines the attacker its calls fail, which is the point.
    made = [call("POST", f"/guilds/{GUILD}/channels", attacker, {"name": f"nuke-{i}"}, soft=True)
            for i in range(6)]
    for c in filter(None, made):
        call("DELETE", f"/channels/{c['id']}", attacker, soft=True)
    # The kick lands within seconds; the incident row only after the revert, which can take ~40s.
    for _ in range(30):
        time.sleep(3)
        after = sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}")
        if int(after) > int(before):
            break
    # Restored channels come back under the same names; clean them up.
    restored = [c for c in call("GET", f"/guilds/{GUILD}/channels") if c["name"].startswith("nuke-")]
    for c in restored:
        call("DELETE", f"/channels/{c['id']}", soft=True)
    print(f"cleaned: {len(restored)} restored nuke-* channels")
    # A bot cannot be quarantined (managed role), so the attacker gets kicked: re-invite it to rerun.
    print("not reversible here: the attacker bot was kicked, re-invite it to rerun")
    return check(int(after) > int(before) and logged("anti_nuke", since),
                 f"nuke: incidents {before} -> {after}, antinuke logged")


def logchan():
    """Deleting a routed log channel must drop it from the config and say so in the main log channel."""
    since = snowflake_now()
    chan = call("POST", f"/guilds/{GUILD}/channels", body={"name": "e2e-logs"})["id"]
    log = logging_cfg()
    log.setdefault("routes", {})["sticker"] = chan
    set_config("logging", log)
    call("DELETE", f"/channels/{chan}")
    time.sleep(4)
    gone = chan not in logging_cfg().get("routes", {}).values()
    print("cleaned: e2e-logs channel deleted by the test; the bot removed its route")
    return check(gone and logged("server", since, "Log Channel Deleted"),
                 f"logchan: route removed={gone}, notice logged")


def purge():
    """A bulk delete must log with the deleted messages attached as messages.txt."""
    since = snowflake_now()
    chan = text_channel("general")
    ids = [call("POST", f"/channels/{chan}/messages", body={"content": f"e2e purge {i}"})["id"]
           for i in range(3)]
    time.sleep(1)
    call("POST", f"/channels/{chan}/messages/bulk-delete", body={"messages": ids})
    time.sleep(3)
    msgs = [m for m in log_messages("message", since)
            if any(a["filename"] == "messages.txt" for a in m["attachments"])]
    files = [a for m in msgs for a in m["attachments"]]
    noted = any("3/3" in json.dumps(m["embeds"]) for m in msgs)
    # The CDN refuses urllib's default user agent.
    get = urllib.request.Request(files[0]["url"], headers={"User-Agent": "tommybot-e2e"}) if files else None
    text = urllib.request.urlopen(get).read().decode() if get else ""
    print("cleaned: nothing left, the test messages were the ones purged")
    return check(noted and all(f"e2e purge {i}" in text for i in range(3)),
                 f"purge: messages.txt attached={bool(files)} with all 3 messages, embed says 3/3={noted}")


SCENARIOS = {"spam": spam, "nuke": nuke, "logchan": logchan, "purge": purge}

if __name__ == "__main__":
    picked = sys.argv[1:] or ["all"]
    names = list(SCENARIOS) if picked == ["all"] else picked
    # Log checks need a main log channel; a fresh database has none.
    log = logging_cfg()
    if not log.get("fallback"):
        set_config("logging", {**log, "fallback": text_channel("tommybot-logs")})
    results = [SCENARIOS[n]() for n in names]
    sys.exit(0 if all(results) else 1)
