"""Minimal fake of the Discord REST API the dashboard calls, so the e2e run needs no token.
Point the dashboard at it with DISCORD_API_BASE=http://127.0.0.1:9100. Stdlib only."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

GUILD = {"id": "100000000000000001", "name": "E2E Server", "icon": None}
USER = {"id": "200000000000000001", "username": "e2e-owner"}
sent = []  # messages the dashboard posted, for assertions


class H(BaseHTTPRequestHandler):
    def reply(self, body, code=200):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/users/@me":
            self.reply(USER)
        elif p == "/users/@me/guilds":
            self.reply([GUILD])
        elif p == "/sent":
            self.reply(sent)
        elif p == f"/guilds/{GUILD['id']}":
            self.reply({**GUILD, "owner_id": USER["id"], "approximate_member_count": 1})
        elif p.endswith("/channels"):
            self.reply([{"id": "300000000000000001", "name": "general", "type": 0, "position": 0}])
        elif p.endswith("/roles"):
            self.reply([{"id": GUILD["id"], "name": "@everyone", "position": 0, "permissions": "0"}])
        elif p.endswith("/auto-moderation/rules"):
            self.reply([])
        elif "/members/" in p:
            self.reply({"roles": []})
        else:
            self.reply({}, 404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n)
        if self.path == "/oauth2/token":
            self.reply({"access_token": "mock-token"})
        else:
            sent.append({"path": self.path, "body": body.decode()})
            self.reply({"id": "1"})

    do_PUT = do_POST

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", 9100), H).serve_forever()
