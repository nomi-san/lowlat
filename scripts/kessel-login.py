#!/usr/bin/env python3
r"""Log in to the signaling service and hand lowlatd its session.

The service authenticates a host by a session token it hands out at login,
and logging in is a person's job: it may need a second factor, and a new
address has to be confirmed from the account's mail. So this is a script a
person runs, not a step the service takes. The token it obtains stays valid
for as long as lowlatd keeps its signaling connection, which is what the
daemon does on its own; a machine that has been off for a week logs in again.

    lowlat-login                      # print what to put in lowlatd.env
    sudo lowlat-login --install       # write it there and restart lowlatd
    sudo lowlat-login --logout        # revoke the session lowlatd.env carries
    lowlat-login --file PATH          # write it into PATH, restarting nothing
    lowlat-login --logout --file PATH # revoke the session PATH carries

Installed as lowlat-login on Linux; scripts/kessel-login.py in a checkout, and
copied as lowlat-login.py beside lowlatd.exe on Windows. There the file is
%ProgramData%\lowlat\lowlatd.env, --install and --logout need an elevated
shell, and the service is restarted through the service manager. --file is
for a host started by hand, as the portable folder's start-host.ps1 does: the
file is made when absent and nobody else is asked for privilege.

The API host comes from KESSEL_API_SERVER when set. Standard library only.
"""

import argparse
import getpass
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

DEFAULT_API = "https://kessel-api.parsec.app"
UNIT = "lowlatd"

# The scopes the established host application asks for. Only ws.host is what
# the daemon uses; the other two let this same session read and revoke itself.
SCOPES = ["ws.host", "ws.client", "api.writer"]


def api_base():
    host = os.environ.get("KESSEL_API_SERVER", "").strip() or DEFAULT_API
    if "://" not in host:
        host = "https://" + host
    return host.rstrip("/")


def env_file():
    """The configuration the service and the host read: one file, per platform."""
    if os.name == "nt":
        base = os.environ.get("ProgramData", r"C:\ProgramData")
        return os.path.join(base, "lowlat", "lowlatd.env")
    return "/etc/lowlat/lowlatd.env"


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


def read_env(path, missing_ok=False):
    try:
        with open(path) as handle:
            return handle.read()
    except FileNotFoundError:
        if missing_ok:
            return ""
        sys.exit("kessel-login: %s does not exist (lowlatd install writes it)" % path)
    except OSError as error:
        sys.exit("kessel-login: cannot read %s: %s (lowlatd install writes it)" % (path, error))


def write_session(path, token, missing_ok=False):
    """Put the token on the KESSEL_SESSION line, keeping everything else."""
    lines = read_env(path, missing_ok).splitlines()
    replaced = False
    for at, line in enumerate(lines):
        if line.startswith("KESSEL_SESSION="):
            lines[at] = "KESSEL_SESSION=" + token
            replaced = True
    if not replaced:
        lines.append("KESSEL_SESSION=" + token)
    # In place, so the file keeps the owner and the access install gave it.
    with open(path, "w", newline="\n") as handle:
        handle.write("\n".join(lines) + "\n")


def installed_token(path):
    for line in read_env(path).splitlines():
        if line.startswith("KESSEL_SESSION="):
            return line[len("KESSEL_SESSION="):].strip()
    return ""


def need_privilege(what):
    if os.name == "nt":
        import ctypes

        if not ctypes.windll.shell32.IsUserAnAdmin():
            sys.exit("kessel-login: %s needs an elevated shell (Run as administrator)" % what)
        return
    if os.geteuid() != 0:
        sys.exit("kessel-login: %s needs to run as root (sudo)" % what)


def sc(*args):
    """One service-manager call, bounded; it ends the program when it cannot be made."""
    try:
        return subprocess.run(["sc.exe"] + list(args), capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        sys.exit("kessel-login: sc.exe %s failed: %s" % (" ".join(args), error))


def service_state():
    """What the service manager says lowlatd is doing, or None when it is not installed."""
    result = sc("query", UNIT)
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        if line.strip().startswith("STATE"):
            return line.split(":", 1)[1].strip()
    return ""


def restart_service():
    """Restart lowlatd the way the platform starts it, within a deadline."""
    if os.name != "nt":
        subprocess.run(["systemctl", "restart", UNIT], check=True)
        return
    if service_state() is None:
        sys.exit("kessel-login: %s is not installed; run lowlatd install first" % UNIT)
    sc("stop", UNIT)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        state = service_state()
        if state is not None and "STOPPED" in state:
            break
        time.sleep(0.5)
    else:
        sys.exit("kessel-login: %s did not stop within 30 s" % UNIT)
    started = sc("start", UNIT)
    if started.returncode != 0:
        sys.exit(
            "kessel-login: %s did not start: %s"
            % (UNIT, (started.stdout + started.stderr).strip())
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    destination = parser.add_mutually_exclusive_group()
    destination.add_argument(
        "--install",
        action="store_true",
        help="write the session into %s and restart %s" % (env_file(), UNIT),
    )
    destination.add_argument(
        "--file",
        metavar="PATH",
        help="write the session into PATH, made when absent, and restart nothing",
    )
    parser.add_argument(
        "--logout",
        action="store_true",
        help="revoke the session %s carries, or the one --file names" % env_file(),
    )
    args = parser.parse_args()
    path = args.file or env_file()

    if args.logout:
        if not args.file:
            need_privilege("--logout")
        token = installed_token(path)
        if not token:
            sys.exit("kessel-login: %s carries no session" % path)
        status, answer = call("DELETE", "/auth/sessions", token=token)
        if status != 204:
            sys.exit("kessel-login: revoke refused, status=%d %s" % (status, reason(answer)))
        print(
            "kessel-login: session revoked; %s still carries it, log in again before starting %s"
            % (path, UNIT)
        )
        return

    if args.install:
        need_privilege("--install")

    session = login()
    token = session["session_id"]
    name = account_name(token)
    print("kessel-login: logged in as %s, host peer %s" % (name or "?", session.get("host_peer_id", "?")))
    if args.install:
        write_session(path, token)
        restart_service()
        print("kessel-login: written to %s, %s restarted" % (path, UNIT))
    elif args.file:
        write_session(path, token, missing_ok=True)
        print("kessel-login: written to %s" % path)
    else:
        print("KESSEL_SESSION=" + token)


if __name__ == "__main__":
    main()
