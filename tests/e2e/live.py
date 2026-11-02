"""Attack scenarios against a real test server, with the real bot running (see live.sh).
Usage: live.py spam|lock|mention_lock|nuke|purge|emojis|edit|all. Needs DISCORD_TOKEN, TEST_GUILD_ID, DATABASE_URL; nuke also needs
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
    # Logs only ever go to the bot's own private log channels, never #general or a test channel.
    routed = {value.get("fallback"), *value.get("routes", {}).values(), *value.get("events", {}).values(),
              *(c.get("channel") for c in value.get("custom", []))} - {None} if section == "logging" else set()
    stray = routed - set(bot_log_channels().values())
    if stray:
        raise SystemExit(f"refusing to route logs outside the bot's log channels: {sorted(stray)}")
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


# Default, as the wizard's Auto Setup makes it: everything in tommybot-logs, staff actions in mod-logs.
LOG_ROUTES = {"tommybot-logs": "fallback", "mod-logs": "mod_command"}


def bot_log_channels():
    """Name -> id of the channels the bot's setup made in its private Logs and Staff categories."""
    chans = call("GET", f"/guilds/{GUILD}/channels")
    cats = {c["id"] for c in chans if c["type"] == 4 and c["name"] in ("Logs", "Staff")}
    return {c["name"]: c["id"] for c in chans if c["type"] == 0 and c.get("parent_id") in cats}


def use_bot_log_channels():
    """Routes logs as a real server set up by the wizard does, into the channels the bot made; never creates any."""
    have = bot_log_channels()
    missing = sorted(set(LOG_ROUTES) - set(have))
    if missing:
        raise SystemExit(f"missing bot log channels {missing}: run the wizard's Auto Setup in the dashboard")
    set_config("logging", {"fallback": have["tommybot-logs"], "routes": {"mod_command": have["mod-logs"]}})


def text_channel(name):
    chans = call("GET", f"/guilds/{GUILD}/channels")
    return next(c["id"] for c in chans if c["type"] == 0 and c["name"] == name)


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what)
    return ok


def snowflake_now():
    return str((int(time.time() * 1000) - 1420070400000) << 22)


def log_messages(kind, since, event=None):
    """Messages after `since` in the log channel the bot uses: the event's, else the kind's, else the main one."""
    log = logging_cfg()
    chan = log.get("events", {}).get(event) or log.get("routes", {}).get(kind) or log.get("fallback")
    return call("GET", f"/channels/{chan}/messages?after={since}&limit=50", soft=True) or []


def logged(kind, since, title=None):
    return any(title in (None, e.get("title")) for m in log_messages(kind, since) for e in m["embeds"])


def spam():
    """Webhook floods #general; automod must delete the flood and the new webhook."""
    # New servers start with every heat filter off, so switch on the two a flood trips.
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True,
                           "filters": {"regular": {"enabled": True}, "similar": {"enabled": True}}})
    since = snowflake_now()
    chan = text_channel("general")
    # Hooks of an immune owner (TommyBot itself) are exempt, so the attacker makes this one.
    hook = call("POST", f"/channels/{chan}/webhooks", os.environ.get("ATTACKER_TOKEN"),
                {"name": "e2e-spammer"})
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
    # Once heat trips, the messages that built it go too, so nothing of the flood is left.
    print(f"cleaned: webhook e2e-spammer and {len(left)} leftover messages in #general")
    # Hooks of verified bots are exempt (popular bots), and Discord flags the attacker as one.
    owner = call("GET", f"/users/{hook['user']['id']}")
    if owner.get("public_flags", 0) & 1 << 16:
        print("SKIP spam webhook delete: the attacker is a verified bot, so its hook is exempt")
        return check(not left, f"spam: {len(left)}/{len(ids)} messages left")
    return check(hook_gone and not left and logged("auto_mod", since, "Webhooks Deleted"),
                 f"spam: webhook deleted={hook_gone}, {len(left)}/{len(ids)} messages left, automod logged")


def nuke():
    """A second bot mass-creates and deletes channels; antinuke must open an incident."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP nuke: ATTACKER_TOKEN not set")
        return True
    # New servers start with anti-nuke off.
    current = json.loads(sql(f"select coalesce(config->'antinuke', '{{}}') from guild_config "
                             f"where guild_id = {GUILD} order by version desc limit 1") or "{}")
    set_config("antinuke", {**current, "enabled": True})
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


def wait_for(q, want, tries=20):
    """Polls a SQL count until `want(count)` holds; returns the last count."""
    for _ in range(tries):
        n = int(sql(q))
        if want(n):
            break
        time.sleep(2)
    return n


def lock():
    """A timed channel lock via the scheduler: #general loses Send Messages, then gets it back."""
    chan = text_channel("general")
    locks = f"select count(*) from lock_snapshots where guild_id = {GUILD}"
    sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
            values ({GUILD}, 'lock', '{{"kind": "channel", "channel": {chan}, "duration_s": 15}}', now())""")
    locked = wait_for(locks, lambda n: n > 0)
    send = 1 << 11
    # The snapshot is saved before the overwrite lands (crash safety), so poll the overwrite too.
    for _ in range(10):
        everyone = [o for o in call("GET", f"/channels/{chan}")["permission_overwrites"] if o["id"] == GUILD]
        denied = bool(everyone) and int(everyone[0]["deny"]) & send != 0
        if denied:
            break
        time.sleep(1)
    unlocked = wait_for(locks, lambda n: n == 0)
    everyone = [o for o in call("GET", f"/channels/{chan}")["permission_overwrites"] if o["id"] == GUILD]
    back = not everyone or int(everyone[0]["deny"]) & send == 0
    return check(locked > 0 and denied and unlocked == 0 and back,
                 f"lock: locked={locked > 0}, send denied={denied}, timed unlock={unlocked == 0 and back}")


def mention_lock():
    """Auto lockdown: a webhook pings past the mention limit; the server locks, then we lift it."""
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True,
                           "auto_lockdown": {"enabled": True, "mentions": True,
                                             "mention_count": 4, "mention_window_ms": 10000}})
    chan = text_channel("general")
    me = call("GET", "/users/@me")["id"]
    hook = call("POST", f"/channels/{chan}/webhooks", body={"name": "e2e-pinger"})
    url = f"/webhooks/{hook['id']}/{hook['token']}?wait=true"
    # Discord lists each mentioned user once per message, so every message counts one ping.
    for _ in range(5):
        call("POST", url, body={"content": f"hi <@{me}>"}, auth=False, soft=True)
    locks = f"select count(*) from lock_snapshots where guild_id = {GUILD}"
    locked = wait_for(locks, lambda n: n > 0, 10)
    call("DELETE", f"/webhooks/{hook['id']}", soft=True)
    # An unlock job without a lock name lifts every lock, like the dashboard's button.
    sql(f"insert into scheduled_jobs (guild_id, kind, run_at) values ({GUILD}, 'unlock', now())")
    unlocked = wait_for(locks, lambda n: n == 0)
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True})
    return check(locked > 0 and unlocked == 0,
                 f"mention lock: locked={locked > 0}, lifted={unlocked == 0}")


def emojis():
    """Custom emojis: every icon is uploaded, a lock log uses them, and Discord takes them as button emojis."""
    app = call("GET", "/applications/@me")["id"]
    have = {e["name"]: e["id"] for e in call("GET", f"/applications/{app}/emojis")["items"]}
    icons = {f[:-4] for f in os.listdir("assets/icons") if f.endswith(".svg")}
    missing = sorted(icons - set(have))
    chan = text_channel("general")
    since = snowflake_now()
    locked = lock()
    # The lock shows up as a Discord overwrite log in discord-logs; any bot log channel counts.
    logs = [m for c in bot_log_channels().values()
            for m in call("GET", f"/channels/{c}/messages?after={since}&limit=20", soft=True) or []]
    text = json.dumps([m["embeds"] for m in logs])
    in_log = "<:channel_text:" in text and "<:clock:" in text
    row = [{"type": 2, "style": 2, "custom_id": f"e2e-{n}", "emoji": {"id": have.get(n), "name": n}} for n in
           ("page_first", "page_prev", "page_next", "page_last", "close")]
    msg = call("POST", f"/channels/{chan}/messages", body={
        "content": f"{' '.join(f'<:{n}:{have.get(n)}>' for n in ('success', 'error', 'right_sort'))} e2e emoji check",
        "components": [{"type": 1, "components": row}]}, soft=True)
    buttons = bool(msg) and all(c.get("emoji", {}).get("id") for c in msg["components"][0]["components"])
    if msg:
        call("DELETE", f"/channels/{chan}/messages/{msg['id']}", soft=True)
    return check(locked and not missing and in_log and buttons,
                 f"emojis: uploaded {len(icons) - len(missing)}/{len(icons)}, in lock log={in_log}, buttons={buttons}")


def purge():
    """A bulk delete must log, then post its messages as a bulk-<code>.txt that replies to that log."""
    since = snowflake_now()
    chan = text_channel("general")
    ids = [call("POST", f"/channels/{chan}/messages", body={"content": f"e2e purge {i}"})["id"]
           for i in range(3)]
    time.sleep(1)
    call("POST", f"/channels/{chan}/messages/bulk-delete", body={"messages": ids})
    time.sleep(3)
    msgs = log_messages("message", since, "message_delete_bulk")
    hit = next((m for m in msgs if m["attachments"]), None)
    files = hit["attachments"] if hit else []
    # The file replies to the embed it belongs to, and that embed names the same file.
    parent = hit and next((m for m in msgs if m["id"] == hit.get("message_reference", {}).get("message_id")), None)
    noted = bool(parent) and f"3/3` saved in `{files[0]['filename']}`" in json.dumps(parent["embeds"])
    # The CDN refuses urllib's default user agent.
    get = urllib.request.Request(files[0]["url"], headers={"User-Agent": "tommybot-e2e"}) if files else None
    text = urllib.request.urlopen(get).read().decode() if get else ""
    print("cleaned: nothing left, the test messages were the ones purged")
    return check(noted and all(f"e2e purge {i}" in text for i in range(3)),
                 f"purge: file attached={bool(files)} with all 3 messages, embed it replies to names it={noted}")

def edit():
    """An edit that sneaks a masked invite into a clean message: automod must delete it and log."""
    set_config("automod", {"enabled": True, "monitor_webhooks": True})
    since = snowflake_now()
    chan = text_channel("general")
    # Hooks of a whitelisted bot (TommyBot itself) are exempt, so the attacker makes this one.
    hook = call("POST", f"/channels/{chan}/webhooks", os.environ.get("ATTACKER_TOKEN"), {"name": "e2e-editor"})
    base = f"/webhooks/{hook['id']}/{hook['token']}"
    sent = call("POST", base + "?wait=true", body={"content": "e2e edit: nothing to see"}, auth=False)
    time.sleep(2)
    first = call("GET", f"/channels/{chan}/messages/{sent['id']}", soft=True) is not None
    call("PATCH", f"{base}/messages/{sent['id']}", body={"content": "e2e edit: [docs](https://discord.gg/e2etest)"},
         auth=False, soft=True)
    gone = False
    for _ in range(10):
        time.sleep(2)
        if call("GET", f"/channels/{chan}/messages/{sent['id']}", soft=True) is None:
            gone = True
            break
    call("DELETE", f"/webhooks/{hook['id']}", soft=True)
    if not gone:
        call("DELETE", f"/channels/{chan}/messages/{sent['id']}", soft=True)
    set_config("automod", {"enabled": True, "anti_spam": True, "monitor_webhooks": True})
    print("cleaned: webhook e2e-editor and its message")
    return check(first and gone and logged("auto_mod", since),
                 f"edit: clean message kept={first}, deleted after masked-invite edit={gone}, automod logged")


SCENARIOS = {"spam": spam, "lock": lock, "mention_lock": mention_lock, "nuke": nuke,
             "purge": purge, "emojis": emojis, "edit": edit}

if __name__ == "__main__":
    picked = sys.argv[1:] or ["all"]
    names = list(SCENARIOS) if picked == ["all"] else picked
    use_bot_log_channels()
    results = [SCENARIOS[n]() for n in names]
    sys.exit(0 if all(results) else 1)
