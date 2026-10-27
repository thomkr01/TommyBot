"""Attack scenarios against a real test server, with the real bot running (see live.sh).
Usage: live.py spam|nuke|all. Needs DISCORD_TOKEN, TEST_GUILD_ID, DATABASE_URL; nuke also needs
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


def text_channel(name):
    chans = call("GET", f"/guilds/{GUILD}/channels")
    return next(c["id"] for c in chans if c["type"] == 0 and c["name"] == name)


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what)
    return ok


def spam():
    """Webhook floods #general; automod must delete the flood and the new webhook."""
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True})
    chan = text_channel("general")
    hook = call("POST", f"/channels/{chan}/webhooks", body={"name": "e2e-spammer"})
    url = f"/webhooks/{hook['id']}/{hook['token']}?wait=true"
    ids = []
    for _ in range(12):
        sent = call("POST", url, body={"content": "SPAM SPAM SPAM buy now"}, auth=False, soft=True)
        if sent is None:
            break
        ids.append(sent["id"])
    time.sleep(5)
    recent = {m["id"] for m in call("GET", f"/channels/{chan}/messages?limit=50")}
    left = [i for i in ids if i in recent]
    hook_gone = call("GET", f"/webhooks/{hook['id']}/{hook['token']}", auth=False, soft=True) is None
    call("DELETE", f"/webhooks/{hook['id']}", soft=True)
    for i in left:
        call("DELETE", f"/channels/{chan}/messages/{i}", soft=True)
    # The first few messages stay below the heat limit and survive.
    return check(hook_gone and len(left) <= 6,
                 f"spam: webhook deleted={hook_gone}, {len(left)}/{len(ids)} messages left")


def nuke():
    """A second bot mass-creates and deletes channels; antinuke must open an incident."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP nuke: ATTACKER_TOKEN not set")
        return True
    before = sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}")
    # Soft: once the bot quarantines the attacker its calls fail, which is the point.
    made = [call("POST", f"/guilds/{GUILD}/channels", attacker, {"name": f"nuke-{i}"}, soft=True)
            for i in range(6)]
    for c in filter(None, made):
        call("DELETE", f"/channels/{c['id']}", attacker, soft=True)
    time.sleep(8)
    after = sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}")
    # Restored channels come back under the same names; clean them up.
    for c in call("GET", f"/guilds/{GUILD}/channels"):
        if c["name"].startswith("nuke-"):
            call("DELETE", f"/channels/{c['id']}", soft=True)
    return check(int(after) > int(before), f"nuke: incidents {before} -> {after}")


SCENARIOS = {"spam": spam, "nuke": nuke}

if __name__ == "__main__":
    picked = sys.argv[1:] or ["all"]
    names = list(SCENARIOS) if picked == ["all"] else picked
    results = [SCENARIOS[n]() for n in names]
    sys.exit(0 if all(results) else 1)
