#!/usr/bin/env python3
"""Log in to the signaling service and hand lowlatd its session.

The service authenticates a host by a session token it hands out at login,
and logging in is a person's job: it may need a second factor, and a new
address has to be confirmed from the account's mail. So this is a script a
person runs, not a step the service takes. The token it obtains stays valid
for as long as lowlatd keeps its signaling connection, which is what the
daemon does on its own; a machine that has been off for a week logs in again.

    lowlat-login                      # print what to put in lowlatd.env
    sudo lowlat-login --install       # write it there and restart lowlatd
    sudo lowlat-login --logout        # revoke the session lowlatd.env carries

Installed as lowlat-login; scripts/kessel-login.py in a checkout.

The API host comes from KESSEL_API_SERVER when set. Standard library only.
"""

import argparse
import getpass
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

DEFAULT_API = "https://kessel-api.parsec.app"
ENV_FILE = "/etc/lowlat/lowlatd.env"
UNIT = "lowlatd"

# The scopes the established host application asks for. Only ws.host is what
# the daemon uses; the other two let this same session read and revoke itself.
SCOPES = ["ws.host", "ws.client", "api.writer"]


def api_base():
    host = os.environ.get("KESSEL_API_SERVER", "").strip() or DEFAULT_API
    if "://" not in host:
        host = "https://" + host
    return host.rstrip("/")


def call(method, path, body=None, token=None):
    """One request; answers (status, parsed body or None)."""
    data = None
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(api_base() + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, parse(response.read())
    except urllib.error.HTTPError as error:
        return error.code, parse(error.read())
    except urllib.error.URLError as error:
        sys.exit("kessel-login: cannot reach %s: %s" % (api_base(), error.reason))


def parse(raw):
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return {"error": raw.decode(errors="replace")[:200]}


def reason(body):
    """What the service said went wrong, in its own words."""
    if not isinstance(body, dict):
        return "no reason given"
    codes = ",".join(code.get("type", "?") for code in body.get("codes", []) if isinstance(code, dict))
    text = body.get("error", "")
    return "%s%s" % (codes + ": " if codes else "", text or "no reason given")


def login():
    email = input("email: ").strip()
    password = getpass.getpass("password: ")
    tfa = input("two-factor code (empty if none): ").strip()
    body = {"email": email, "password": password, "session_scopes": SCOPES}
    if tfa:
        body["tfa"] = tfa
    status, answer = call("POST", "/v2/auth", body)
    # A created session is answered 201. Taking only 200 threw a successful
    # login away and left the session it created on the account.
    if status in (200, 201) and isinstance(answer, dict) and answer.get("session_id"):
        return answer
    if status == 403 and "ip_unverified" in reason(answer):
        sys.exit(
            "kessel-login: the service sent a mail to confirm this address; "
            "confirm it and run this again (a two-factor code skips the check)"
        )
    sys.exit("kessel-login: login refused, status=%d %s" % (status, reason(answer)))


def account_name(token):
    status, answer = call("GET", "/me", token=token)
    if status == 200 and isinstance(answer, dict):
        return answer.get("data", {}).get("name", "")
    return ""


def read_env():
    try:
        with open(ENV_FILE) as handle:
            return handle.read()
    except OSError as error:
        sys.exit("kessel-login: cannot read %s: %s" % (ENV_FILE, error))


def install(token):
    """Put the token on the KESSEL_SESSION line, keeping everything else."""
    lines = read_env().splitlines()
    replaced = False
    for at, line in enumerate(lines):
        if line.startswith("KESSEL_SESSION="):
            lines[at] = "KESSEL_SESSION=" + token
            replaced = True
    if not replaced:
        lines.append("KESSEL_SESSION=" + token)
    with open(ENV_FILE, "w") as handle:
        handle.write("\n".join(lines) + "\n")
    subprocess.run(["systemctl", "restart", UNIT], check=True)


def installed_token():
    for line in read_env().splitlines():
        if line.startswith("KESSEL_SESSION="):
            return line[len("KESSEL_SESSION="):].strip()
    return ""


def need_root(what):
    if os.geteuid() != 0:
        sys.exit("kessel-login: %s needs to run as root (sudo)" % what)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--install", action="store_true", help="write the session into %s and restart %s" % (ENV_FILE, UNIT))
    parser.add_argument("--logout", action="store_true", help="revoke the session %s carries" % ENV_FILE)
    args = parser.parse_args()

    if args.logout:
        need_root("--logout")
        token = installed_token()
        if not token:
            sys.exit("kessel-login: %s carries no session" % ENV_FILE)
        status, answer = call("DELETE", "/auth/sessions", token=token)
        if status != 204:
            sys.exit("kessel-login: revoke refused, status=%d %s" % (status, reason(answer)))
        print("kessel-login: session revoked; %s still carries it, log in again before starting %s" % (ENV_FILE, UNIT))
        return

    if args.install:
        need_root("--install")

    session = login()
    token = session["session_id"]
    name = account_name(token)
    print("kessel-login: logged in as %s, host peer %s" % (name or "?", session.get("host_peer_id", "?")))
    if args.install:
        install(token)
        print("kessel-login: written to %s, %s restarted" % (ENV_FILE, UNIT))
    else:
        print("KESSEL_SESSION=" + token)


if __name__ == "__main__":
    main()
