"""Attack scenarios against a real test server, with the real bot running (see live.sh).
Usage: live.py spam|lock|mention_lock|nuke|panic|verify_delete|dm_timing|purge|emojis|edit|restart|all. Needs DISCORD_TOKEN, TEST_GUILD_ID, DATABASE_URL; nuke, panic, verify_delete and dm_timing also need
ATTACKER_TOKEN, a second bot on the test server with Administrator. Stdlib + psql only.
Exits non-zero if the bot did not react. Never prints tokens."""
import functools
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


@functools.cache
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
    """A log with this title arrived, and it carries the date and time (`<t:...:F>`)."""
    return any(title in (None, e.get("title")) and ":F>" in e.get("description", "")
               for m in log_messages(kind, since) for e in m["embeds"])


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


def attacker_id():
    return call("GET", "/users/@me", os.environ["ATTACKER_TOKEN"])["id"]


def bot_role_of(user):
    """The managed role Discord made for this bot, or None."""
    return next((r for r in call("GET", f"/guilds/{GUILD}/roles")
                 if (r.get("tags") or {}).get("bot_id") == user), None)


@functools.cache
def quarantine_role():
    return next(r["id"] for r in call("GET", f"/guilds/{GUILD}/roles") if r["name"] == "Quarantine")


def antinuke_on(**extra):
    """Anti-nuke on (new servers start with it off), quarantining into the server's Quarantine role."""
    current = json.loads(sql(f"select coalesce(config->'antinuke', '{{}}') from guild_config "
                             f"where guild_id = {GUILD} order by version desc limit 1") or "{}")
    set_config("antinuke", {**current, "enabled": True, "quarantine_role": quarantine_role(), **extra})


def free_attacker(user, perms):
    """Undoes the containment so the scenario can run again without a new invite."""
    role = bot_role_of(user)
    if role and int(role["permissions"]) != perms:
        call("PATCH", f"/guilds/{GUILD}/roles/{role['id']}", body={"permissions": str(perms)})
    call("DELETE", f"/guilds/{GUILD}/members/{user}/roles/{quarantine_role()}", soft=True)
    sql(f"delete from quarantines where guild_id = {GUILD} and user_id = {user}")
    print("cleaned: attacker bot role permissions back, quarantine removed")


def contained(user):
    """(still a member, its bot role has no permissions, holds the Quarantine role)."""
    member = call("GET", f"/guilds/{GUILD}/members/{user}", soft=True)
    role = bot_role_of(user)
    return (member is not None, role is not None and int(role["permissions"]) == 0,
            member is not None and quarantine_role() in member["roles"])


def sanitized(since):
    """The bot put the Quarantine overwrite on a new channel, with its audit reason."""
    entries = call("GET", f"/guilds/{GUILD}/audit-logs?action_type=13&limit=50")["audit_log_entries"]
    return any(int(e["id"]) > int(since) and "Sanitizing the channel" in (e.get("reason") or "")
               for e in entries)


def attack_channels(n, attacker):
    """The attacker makes n channels, waits so the bot can sanitize them, then deletes them."""
    made = [call("POST", f"/guilds/{GUILD}/channels", attacker, {"name": f"nuke-{i}"}, soft=True)
            for i in range(n)]
    time.sleep(3)
    for c in filter(None, made):
        call("DELETE", f"/channels/{c['id']}", attacker, soft=True)


def clean_nuke_channels():
    restored = [c for c in call("GET", f"/guilds/{GUILD}/channels") if c["name"].startswith("nuke-")]
    for c in restored:
        call("DELETE", f"/channels/{c['id']}", soft=True)
    print(f"cleaned: {len(restored)} nuke-* channels")


def nuke():
    """A second bot mass-creates and deletes channels: the bot clears its role, quarantines it,
    sanitizes the new channels and reverts; the attacker is not kicked (AVVIK-82, 83)."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP nuke: ATTACKER_TOKEN not set")
        return True
    user = attacker_id()
    role = bot_role_of(user)
    if role is None:
        raise SystemExit("the attacker bot is not on the test server: invite it again")
    perms = int(role["permissions"]) or 8  # 0: a run before left it cleared
    antinuke_on()
    before = sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}")
    since = snowflake_now()
    # Soft: once the bot contains the attacker its calls fail, which is the point.
    attack_channels(6, attacker)
    # The incident row is written after the revert, which can take ~40s.
    after = wait_for(f"select count(*) from nuke_incidents where guild_id = {GUILD}",
                     lambda n: n > int(before), tries=30)
    member, cleared, quarantined = contained(user)
    # The report is sent just after the incident row is written.
    for _ in range(20):
        report = [e for _, e in titled("anti_nuke", since, "Anti Nuke Report:")]
        if report:
            break
        time.sleep(3)
    text = json.dumps(report)
    clean_nuke_channels()
    free_attacker(user, perms)
    return check(after > int(before) and member and cleared and quarantined and sanitized(since)
                 and "bot role cleared" in text and "Bot Role Cleansing" in text,
                 f"nuke: incidents {before} -> {after}, still a member={member}, role cleared={cleared}, "
                 f"quarantined={quarantined}, sanitized={sanitized(since)}, report={bool(report)}")


def titled(kind, since, title):
    return [(m, e) for m in log_messages(kind, since) for e in m["embeds"] if e.get("title") == title]


def panic():
    """Panic (and heat panic) on: a nuke trips panic mode: anti-nuke log, Panic Mode Report with the
    quarantined attacker, a role lock for about 60 s, then the unlock (AVVIK-85, 86, 87)."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP panic: ATTACKER_TOKEN not set")
        return True
    user = attacker_id()
    role = bot_role_of(user)
    if role is None:
        raise SystemExit("the attacker bot is not on the test server: invite it again")
    perms = int(role["permissions"]) or 8  # 0: a run before left it cleared
    antinuke_on()
    # Backups off, so the panic never loads a backup over the test server.
    set_config("backups", {"enabled": False})
    set_config("panic", {"executors": 1, "window_secs": 600, "duration_secs": 60, "heat_algorithm": True,
                         "heat_executors": 2, "lock_on_trigger": True, "unlock_when_ending": True})
    since = snowflake_now()
    try:
        attack_channels(4, attacker)
        locked = wait_for(f"select count(*) from lock_snapshots where guild_id = {GUILD}", lambda n: n > 0)
        time.sleep(3)
        anti = titled("anti_nuke", since, "Anti Nuke Report:")
        trig = titled("panic", since, "Panic Mode Triggered! [Anti Nuke]")
        rep = titled("panic", since, "Panic Mode Report:")
        lock_log = titled("panic", since, "Panic Mode Lockdown")
        body = json.dumps([e for _, e in rep])
        counts = "Members Quarantined:** 1" in body and f"<@{user}>" in body and "fragments" in body
        # The unlock is a scheduler job about 60 s after the start.
        ended, waited = [], 0
        while not ended and waited < 120:
            time.sleep(5)
            waited += 5
            ended = titled("panic", since, "Panic Mode Ended")
        unlocked = int(sql(f"select count(*) from lock_snapshots where guild_id = {GUILD}")) == 0
        secs = None
        if lock_log and ended:
            ts = lambda m: (int(m["id"]) >> 22) / 1000
            secs = round(ts(ended[0][0]) - ts(lock_log[0][0]))
    finally:
        set_config("panic", {"executors": 0})
        clean_nuke_channels()
        free_attacker(user, perms)
    print(f"panic: report body {body[:400]}")
    return check(locked and anti and trig and rep and counts and lock_log and ended and unlocked
                 and secs is not None and 50 <= secs <= 90,
                 f"panic: locked={bool(locked)}, anti-nuke log={bool(anti)}, triggered={bool(trig)}, "
                 f"report={bool(rep)} counts={counts}, lockdown log={bool(lock_log)}, ended={bool(ended)} "
                 f"after {secs}s, unlocked={unlocked}")


def owner_dms(since):
    """Messages the bot sent the server owner after `since` (the bot can read its own DM channel)."""
    owner = call("GET", f"/guilds/{GUILD}")["owner_id"]
    dm = call("POST", "/users/@me/channels", body={"recipient_id": owner})
    return [m["content"] for m in call("GET", f"/channels/{dm['id']}/messages?after={since}&limit=50")]


def verify_delete():
    """The attacker deletes the verification channel: a new one with the panel, the attacker
    quarantined, the owner told. Three new channels in 10 minutes, the fourth delete stops (AVVIK-92).
    Run once per bot start: the 3-per-10-minutes count lives in the bot's memory."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP verify_delete: ATTACKER_TOKEN not set")
        return True
    user = attacker_id()
    perms = int(bot_role_of(user)["permissions"]) or 8
    free_attacker(user, perms)
    set_config("antinuke", {"enabled": False, "quarantine_role": quarantine_role()})
    me = call("GET", "/users/@me")["id"]
    chan = call("POST", f"/guilds/{GUILD}/channels", body={"name": "verify-e2e"})
    set_config("verification", {"enabled": True, "channel": chan["id"]})
    since = snowflake_now()
    made, stopped, quarantined, still_admin = [], False, False, None
    current = chan["id"]
    try:
        for i in range(4):
            # The attacker deletes while it still can; once it is contained the test bot does it.
            call("DELETE", f"/channels/{current}", attacker, soft=True)
            by_attacker = call("GET", f"/channels/{current}", soft=True) is None
            if i > 0 and still_admin is None:
                still_admin = by_attacker
            if not by_attacker:
                call("DELETE", f"/channels/{current}")
            new = None
            for _ in range(15):
                time.sleep(2)
                c = json.loads(sql(f"select coalesce(config->'verification'->>'channel', 'null') from guild_config "
                                   f"where guild_id = {GUILD} order by version desc limit 1"))
                if c and str(c) != current:
                    new = str(c)
                    break
            if i == 0:
                # Under Discord's rate limits the quarantine can take a while.
                quarantined = wait_for(f"select count(*) from quarantines where guild_id = {GUILD} "
                                       f"and user_id = {user}", lambda n: n == 1, tries=15) == 1
            if new is None:
                stopped = i == 3
                break
            panel = any(m["author"]["id"] == me and m.get("components")
                        for m in call("GET", f"/channels/{new}/messages?limit=5"))
            made.append((new, panel))
            current = new
        time.sleep(10)
        dms = owner_dms(since)
        logs = [e.get("description", "") for m in log_messages("anti_nuke", since) for e in m["embeds"]]
    finally:
        set_config("verification", {"enabled": False})
        for c in call("GET", f"/guilds/{GUILD}/channels"):
            if c["name"] == "verify-e2e":
                call("DELETE", f"/channels/{c['id']}", soft=True)
        free_attacker(user, perms)
        print("cleaned: verify-e2e channels, verification off")
    stop_dm = any("after 3 new ones" in d for d in dms)
    print(f"verify_delete: owner DMs {[d[:90] for d in dms]}")
    print(f"verify_delete: the quarantined attacker could still delete channels: {still_admin}")
    # A quarantined bot also loses its bot role's permissions, so it cannot delete again.
    return check(still_admin is False and len(made) == 3 and all(p for _, p in made) and stopped and quarantined
                 and any("was quarantined" in d for d in dms) and stop_dm
                 and any("after 3 new ones" in l for l in logs),
                 f"verify_delete: {len(made)} new channels (panels {[p for _, p in made]}), 4th stopped={stopped}, "
                 f"attacker quarantined={quarantined}, owner DM quarantine/stop="
                 f"{any('was quarantined' in d for d in dms)}/{stop_dm}")


def dm_timing():
    """How long Discord takes to refuse a DM to a bot, on the same two calls the bot makes before
    a kick or ban (AVVIK-81). The bot waits at most 3 s; the real time to a member who gets the DM
    is measured in the later round with an alt account."""
    attacker = os.environ.get("ATTACKER_TOKEN")
    if not attacker:
        print("SKIP dm_timing: ATTACKER_TOKEN not set")
        return True
    times = []
    for _ in range(5):
        t = time.monotonic()
        dm = call("POST", "/users/@me/channels", body={"recipient_id": attacker_id()}, soft=True)
        sent = dm and call("POST", f"/channels/{dm['id']}/messages", body={"content": "e2e dm timing"}, soft=True)
        times.append((time.monotonic() - t, bool(sent)))
    worst = max(t for t, _ in times)
    print(f"dm_timing: {[round(t, 2) for t, _ in times]} s, sent={[s for _, s in times]}")
    return check(not any(s for _, s in times) and worst < 3, f"dm_timing: DM to a bot refused in at most {worst:.2f} s")


def wait_for(q, want, tries=20):
    """Polls a SQL count until `want(count)` holds; returns the last count."""
    for _ in range(tries):
        n = int(sql(q))
        if want(n):
            break
        time.sleep(2)
    return n


def can_send(chan):
    """@everyone is not denied Send Messages in this channel."""
    everyone = [o for o in call("GET", f"/channels/{chan}")["permission_overwrites"] if o["id"] == GUILD]
    return not everyone or int(everyone[0]["deny"]) & (1 << 11) == 0


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
    back = can_send(chan)
    return check(locked > 0 and denied and unlocked == 0 and back,
                 f"lock: locked={locked > 0}, send denied={denied}, timed unlock={unlocked == 0 and back}")


def restart():
    """The bot dies mid timed lock and is down when the unlock is due: after the restart it must still unlock."""
    chan = text_channel("general")
    locks = f"select count(*) from lock_snapshots where guild_id = {GUILD}"
    sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
            values ({GUILD}, 'lock', '{{"kind": "channel", "channel": {chan}, "duration_s": 10}}', now())""")
    locked = wait_for(locks, lambda n: n > 0)
    time.sleep(2)  # let the overwrite land
    pid_file = os.environ["BOT_PID_FILE"]
    os.kill(int(open(pid_file).read()), 9)  # a crash, not a clean shutdown
    time.sleep(15)  # the unlock comes due while the bot is down
    held = int(sql(locks))
    log = open("target/live-bot.log", "a")
    bot = subprocess.Popen(["./target/release/tommybot"], stdout=log, stderr=log)
    open(pid_file, "w").write(str(bot.pid))
    unlocked = wait_for(locks, lambda n: n == 0, 30)
    back = can_send(chan)
    return check(locked > 0 and held > 0 and unlocked == 0 and back,
                 f"restart: locked={locked > 0}, still locked while down={held > 0}, unlocked after restart={unlocked == 0 and back}")


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


def section(name):
    """The newest saved config section, or None when the config has no such key."""
    raw = sql(f"select config->'{name}' from guild_config where guild_id = {GUILD} order by version desc limit 1")
    return json.loads(raw) if raw else None


def restore_section(name, old):
    """Puts a section back as `section` read it: the old value, or no key at all."""
    if old is not None:
        return set_config(name, old)
    sql(f"""insert into guild_config (guild_id, version, config)
            select {GUILD}, max(version) + 1,
                   (select config from guild_config where guild_id = {GUILD} order by version desc limit 1) - '{name}'
            from guild_config where guild_id = {GUILD};
            select pg_notify('config_changed', '{GUILD}');""")
    time.sleep(2)


def incidents():
    return int(sql(f"select count(*) from nuke_incidents where guild_id = {GUILD}"))


def wait_report(since, tries=20):
    for _ in range(tries):
        report = [e for _, e in titled("anti_nuke", since, "Anti Nuke Report:")]
        if report:
            return report
        time.sleep(3)
    return []


def attacker_setup():
    attacker = os.environ["ATTACKER_TOKEN"]
    user = attacker_id()
    role = bot_role_of(user)
    if role is None:
        raise SystemExit("the attacker bot is not on the test server: invite it again")
    return attacker, user, role, int(role["permissions"]) or 8


def g1_stale():
    """D1: a nuker released by hand with its quarantines row left behind is contained again."""
    attacker, user, role, perms = attacker_setup()
    antinuke_on()
    try:
        before = incidents()
        since = snowflake_now()
        attack_channels(6, attacker)
        first = wait_for(f"select count(*) from nuke_incidents where guild_id = {GUILD}",
                         lambda n: n > before, tries=30)
        held = contained(user)
        # Its report lands after the incident row; the control must not count it.
        first_report = bool(wait_report(since))
        clean_nuke_channels()
        # Control: still held, so its calls fail and nothing new is written.
        since = snowflake_now()
        attack_channels(6, attacker)
        time.sleep(20)
        control = not wait_report(since, tries=1)
        clean_nuke_channels()
        # Released by hand, row kept (not free_attacker).
        call("PATCH", f"/guilds/{GUILD}/roles/{role['id']}", body={"permissions": str(perms)})
        call("DELETE", f"/guilds/{GUILD}/members/{user}/roles/{quarantine_role()}", soft=True)
        time.sleep(2)
        rows = int(sql(f"select count(*) from quarantines where guild_id = {GUILD} and user_id = {user}"))
        released = contained(user)
        since = snowflake_now()
        attack_channels(6, attacker)
        # Within 10 minutes the bot extends the same nuke case (record_nuke, NUKE_GAP), so the
        # incident row count stays; a second Anti Nuke Report is the full contain.
        report = wait_report(since, tries=30)
        again = contained(user)
        second = incidents()
    finally:
        clean_nuke_channels()
        free_attacker(user, perms)
    print(f"g1_stale: first contain {held}, control no-op={control}, row kept={rows}, released {released}, "
          f"incidents {before} -> {first} -> {second}, contained again {again}, second report="
          f"{[e.get('description', '')[:40] for e in report]}")
    return check(first > before and all(held) and first_report and control and rows == 1
                 and released == (True, False, False) and all(again) and bool(report),
                 "g1_stale: a hand-released nuker with a leftover row is contained and reported again")


def quarantine_overwrite_of(chan):
    q = quarantine_role()
    o = next((o for o in call("GET", f"/channels/{chan}")["permission_overwrites"] if o["id"] == q), None)
    return o and (int(o["allow"]), int(o["deny"]))


def sanitized_channel(chan, since):
    """An overwrite create (13) or update (14) audit entry on `chan` with the sanitize reason."""
    for t in (13, 14):
        for e in call("GET", f"/guilds/{GUILD}/audit-logs?action_type={t}&limit=50")["audit_log_entries"]:
            if (int(e["id"]) > int(since) and e.get("target_id") == chan
                    and "Sanitizing the channel" in (e.get("reason") or "")):
                return True
    return False


def g1_verify_channel():
    """D55: enforce_quarantine keeps the verification channel's readable overwrite on a ChannelUpdate."""
    old_misc, old_ver, old_anti = section("misc"), section("verification"), section("antinuke")
    antinuke_on()
    q = quarantine_role()
    made = []
    try:
        chan = call("POST", f"/guilds/{GUILD}/channels", body={
            "name": "g1-verify", "permission_overwrites": [{"id": q, "type": 0, "allow": "66560", "deny": "2048"}]})
        made.append(chan["id"])
        set_config("verification", {**(old_ver or {}), "enabled": True, "mode": "Button", "channel": chan["id"]})
        set_config("misc", {**(old_misc or {}), "enforce_quarantine_perms": True})
        start = quarantine_overwrite_of(chan["id"])
        since = snowflake_now()
        call("PATCH", f"/channels/{chan['id']}", body={"name": "g1-verify-2"})
        time.sleep(4)
        after = quarantine_overwrite_of(chan["id"])
        hit = sanitized_channel(chan["id"], since)
        since2 = snowflake_now()
        other = call("POST", f"/guilds/{GUILD}/channels", body={"name": "g1-other"})
        made.append(other["id"])
        time.sleep(4)
        other_ow = quarantine_overwrite_of(other["id"])
        other_hit = sanitized_channel(other["id"], since2)
    finally:
        # Verification off first, so deleting its channel does not make a new one.
        restore_section("verification", old_ver)
        restore_section("misc", old_misc)
        restore_section("antinuke", old_anti)
        for c in made:
            call("DELETE", f"/channels/{c}", soft=True)
        print(f"cleaned: {len(made)} g1 channels, verification/misc/antinuke sections restored")
    print(f"g1_verify_channel: verify overwrite {start} -> {after} (sanitize audit={hit}); "
          f"g1-other overwrite {other_ow} (sanitize audit={other_hit})")
    # Verification rewrites the plan's 66560/2048 to its own shape (VERIFY_ALLOW, VERIFY_DENY) when
    # it is turned on; what matters is that View stays allowed after the ChannelUpdate.
    return check(after == start and after[0] == 66560 and after[1] & 1024 == 0 and not hit and other_ow is not None and other_ow[1] != 0 and other_hit,
                 "g1_verify_channel: verification overwrite kept, other channel sanitized")


def g1_deleted_role():
    """D6: with the Quarantine role deleted in Discord, a nuking bot is still stripped and not kicked."""
    attacker, user, role, perms = attacker_setup()
    old_anti = section("antinuke")
    gone = call("POST", f"/guilds/{GUILD}/roles", body={"name": "Quarantine-g1", "permissions": "0"})
    antinuke_on(quarantine_role=gone["id"])
    call("DELETE", f"/guilds/{GUILD}/roles/{gone['id']}")
    time.sleep(2)
    try:
        since = snowflake_now()
        before = incidents()
        attack_channels(6, attacker)
        after = wait_for(f"select count(*) from nuke_incidents where guild_id = {GUILD}",
                         lambda n: n > before, tries=30)
        member = call("GET", f"/guilds/{GUILD}/members/{user}", soft=True)
        brole = bot_role_of(user)
        report = wait_report(since)
        logs = json.dumps([e for m in log_messages("anti_nuke", since) for e in m["embeds"]])
        saved = sql(f"select saved_roles from quarantines where guild_id = {GUILD} and user_id = {user}")
    finally:
        restore_section("antinuke", old_anti)
        antinuke_on()
        clean_nuke_channels()
        if call("GET", f"/guilds/{GUILD}/members/{user}", soft=True) is None:
            print("ATTENTION: the attacker bot was kicked and must be invited again")
        else:
            free_attacker(user, perms)
    roles = member["roles"] if member else None
    text = json.dumps(report)
    print(f"g1_deleted_role: incidents {before} -> {after}, member={member is not None}, roles={roles}, "
          f"bot role perms={brole and brole['permissions']}, saved roles={saved!r}, "
          f"'Quarantine role missing' logged={'Quarantine role missing' in logs}, report kicked={'kick' in text.lower()}")
    print(f"g1_deleted_role: report {text[:600]}")
    return check(after > before and member is not None and roles == [role["id"]] and brole
                 and int(brole["permissions"]) == 0 and "Quarantine role missing" in logs,
                 "g1_deleted_role: contained without a Quarantine role, not kicked")


def g2a_panic_lock():
    """D8: panic mode's end does not lift a lock staff made before panic."""
    attacker, user, role, perms = attacker_setup()
    general = text_channel("general")
    old = {s: section(s) for s in ("backups", "panic", "antinuke")}
    locks = f"select count(*) from lock_snapshots where guild_id = {GUILD}"
    antinuke_on()
    set_config("backups", {"enabled": False})
    set_config("panic", {"executors": 1, "window_secs": 600, "duration_secs": 60,
                         "lock_on_trigger": True, "unlock_when_ending": True})
    since = snowflake_now()
    # A role lock strips role permissions, it puts no overwrite on #general.
    role_perms = lambda: {r["name"]: r["permissions"] for r in call("GET", f"/guilds/{GUILD}/roles") if not r.get("managed")}
    base = role_perms()
    try:
        sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
                values ({GUILD}, 'lock', '{{"kind": "role"}}', now())""")
        wait_for(locks, lambda n: n > 0)
        time.sleep(3)
        kinds = sql(f"select string_agg(kind::text, ',') from lock_snapshots where guild_id = {GUILD}")
        attack_channels(4, attacker)
        end = ""
        for _ in range(15):
            end = sql(f"select payload from scheduled_jobs where guild_id = {GUILD} and kind = 'panic_end'")
            if end:
                break
            time.sleep(2)
        ended, waited = [], 0
        while not ended and waited < 150:
            time.sleep(5)
            waited += 5
            ended = titled("panic", since, "Panic Mode Ended")
        time.sleep(3)
        left = int(sql(locks))
        held = {n: p for n, p in role_perms().items() if base.get(n) != p}
    finally:
        sql(f"insert into scheduled_jobs (guild_id, kind, run_at) values ({GUILD}, 'unlock', now())")
        unlocked = wait_for(locks, lambda n: n == 0) == 0
        for s, v in old.items():
            restore_section(s, v)
        clean_nuke_channels()
        free_attacker(user, perms)
        time.sleep(2)
        print(f"cleaned: unlocked={unlocked}, role perms as before={role_perms() == base}, panic/backups/antinuke restored")
    print(f"g2a_panic_lock: manual lock kinds [{kinds}], panic_end payload {end}, ended={bool(ended)}, "
          f"snapshots after end={left}, roles still locked={sorted(held)}")
    return check(bool(ended) and '"locked": false' in end and left == 1 and bool(held),
                 "g2a_panic_lock: the manual lock outlived panic mode's end")


def g2a_timer():
    """D8: a timed lock lifted early takes its timer with it, so a later lock stays."""
    chan = text_channel("general")
    locks = f"select count(*) from lock_snapshots where guild_id = {GUILD}"
    timer = f"select payload->'lock' from scheduled_jobs where guild_id = {GUILD} and kind = 'unlock'"
    start = time.monotonic()
    try:
        sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
                values ({GUILD}, 'lock', '{{"kind": "channel", "channel": {chan}, "duration_s": 40}}', now())""")
        wait_for(locks, lambda n: n > 0)
        lock = ""
        for _ in range(10):
            lock = sql(timer)
            if lock:
                break
            time.sleep(1)
        sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
                values ({GUILD}, 'unlock', '{{"lock": {lock}}}', now())""")
        lifted = wait_for(locks, lambda n: n == 0) == 0
        time.sleep(2)
        timers = int(sql(f"select count(*) from scheduled_jobs where guild_id = {GUILD} and kind = 'unlock'"))
        sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
                values ({GUILD}, 'lock', '{{"kind": "channel", "channel": {chan}}}', now())""")
        relocked = wait_for(locks, lambda n: n > 0) > 0
        time.sleep(max(0, 45 - (time.monotonic() - start)))
        left = int(sql(locks))
        sends = can_send(chan)
    finally:
        sql(f"insert into scheduled_jobs (guild_id, kind, run_at) values ({GUILD}, 'unlock', now())")
        ok = wait_for(locks, lambda n: n == 0) == 0
        print(f"cleaned: #general unlocked={ok and can_send(chan)}")
    print(f"g2a_timer: timer payload {lock}, lifted early={lifted}, unlock jobs left={timers}, "
          f"relocked={relocked}, at 45 s snapshots={left} can send={sends}")
    return check(lifted and timers == 0 and relocked and left == 1 and not sends,
                 "g2a_timer: the first lock's timer did not lift the later lock")


# A real Discord user that is never a member of the test server (Discord's system account).
OUTSIDER = "643945264868098049"


def g2a_renew_404():
    """D26: a timeout renewal for a user who is not a member retries hourly instead of being dropped."""
    renew = f"from scheduled_jobs where guild_id = {GUILD} and kind = 'timeout_renew' and payload->>'user' = '{OUTSIDER}'"
    until = int(time.time()) + 30 * 86400
    try:
        sql(f"""insert into scheduled_jobs (guild_id, kind, payload, run_at)
                values ({GUILD}, 'timeout_renew', '{{"user": {OUTSIDER}, "until_secs": {until}}}', now())""")
        rows = ""
        for _ in range(15):
            time.sleep(2)
            rows = sql(f"select attempts, round(extract(epoch from run_at - now())) {renew}")
            if rows and int(rows.split("|")[1]) > 600:
                break
            if not rows:
                break
    finally:
        sql(f"delete {renew}")
        print("cleaned: timeout_renew rows for the outsider")
    print(f"g2a_renew_404: row after the run (attempts|secs to run_at): {rows!r}")
    secs = int(rows.split("|")[1]) if rows else 0
    return check(3000 < secs <= 3600, "g2a_renew_404: renewal for a non-member re-queued about 1 h out")


def g2a_reban_reason():
    """Ban Sync `outlasts`: does a second PUT ban on an already banned user replace its reason in
    real Discord (the mock does)? `ours` reads that reason to tell a copy from a local ban."""
    path = f"/guilds/{GUILD}/bans/{OUTSIDER}"
    if call("GET", path, soft=True) is not None:
        raise SystemExit("the outsider is already banned on the test server")
    def ban(why):
        # The ban reason is the audit log reason header, as twilight sends it.
        req = urllib.request.Request(API + path, b"{}", {
            "Content-Type": "application/json", "User-Agent": "DiscordBot (tommybot-e2e, 1)",
            "Authorization": f"Bot {os.environ['DISCORD_TOKEN']}", "X-Audit-Log-Reason": why}, method="PUT")
        urllib.request.urlopen(req).read()
        return call("GET", path).get("reason")
    try:
        first = ban("e2e local temp ban")
        second = ban("Ban Sync: copied from e2e (1): later copy")
    finally:
        # Without a JSON body Discord refuses this DELETE (50109) under the JSON content type.
        call("DELETE", path, body={})
        print("cleaned: outsider unbanned")
    print(f"g2a_reban_reason: reason after first ban {first!r}, after the second PUT {second!r}")
    # Informational: Ban Sync's `ours` and `outlasts` paths depend on which one Discord does.
    return check(True, f"g2a_reban_reason: second PUT {'replaced' if second != first else 'kept'} the reason")


SCENARIOS = {"spam": spam, "lock": lock, "mention_lock": mention_lock, "nuke": nuke,
             "purge": purge, "emojis": emojis, "edit": edit, "restart": restart, "panic": panic,
             "verify_delete": verify_delete, "dm_timing": dm_timing,
             "g1_stale": g1_stale, "g1_verify_channel": g1_verify_channel, "g1_deleted_role": g1_deleted_role,
             "g2a_panic_lock": g2a_panic_lock, "g2a_timer": g2a_timer, "g2a_renew_404": g2a_renew_404,
             "g2a_reban_reason": g2a_reban_reason}

if __name__ == "__main__":
    picked = sys.argv[1:] or ["all"]
    names = list(SCENARIOS) if picked == ["all"] else picked
    use_bot_log_channels()
    results = [SCENARIOS[n]() for n in names]
    sys.exit(0 if all(results) else 1)
