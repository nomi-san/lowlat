#!/bin/sh
# Install lowlatd where no package does: the layout the Debian package makes,
# put in place by hand, and the same units enabled. Build it first, as yourself:
#
#     cargo build --release -p lowlatd
#     sudo packaging/install.sh                # then: sudo lowlat-login --install
#     sudo packaging/install.sh --uninstall    # keeps /etc/lowlat/lowlatd.env
#
# DESTDIR stages the files under a root of its own and touches nothing else on
# this machine, which is what a distribution's own package recipe wants.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
top=$(dirname "$here")
dest=${DESTDIR:-}
binary="$top/target/release/lowlatd"
env_file=/etc/lowlat/lowlatd.env

# Every file but the configuration: where it goes, its mode, where it comes
# from. The configuration is kept apart because it is the administrator's once
# it exists.
files() {
    cat <<EOF
/usr/bin/lowlatd 755 $binary
/usr/bin/lowlat-login 755 $top/scripts/kessel-login.py
/usr/lib/systemd/system/lowlatd.service 644 $here/lowlatd.service
/usr/lib/systemd/user/lowlat-session.service 644 $here/lowlat-session.service
/usr/lib/systemd/user/lowlat-tray.service 644 $here/lowlat-tray.service
/usr/lib/udev/rules.d/70-lowlat-pads.rules 644 $here/70-lowlat-pads.rules
/usr/share/doc/lowlat/sddm-10-wayland.conf.example 644 $here/sddm-10-wayland.conf.example
/usr/share/doc/lowlat/README.md 644 $top/README.md
EOF
}

install_all() {
    if [ ! -x "$binary" ]; then
        echo "install: $binary is missing; build it first: cargo build --release -p lowlatd" >&2
        exit 1
    fi
    command -v python3 >/dev/null 2>&1 || echo "install: python3 is missing, and lowlat-login needs it" >&2
    files | while read -r to mode from; do
        install -D -m "$mode" "$from" "$dest$to"
    done
    # A login writes the session into this file, so an existing one is kept.
    if [ ! -e "$dest$env_file" ]; then
        install -D -m 640 "$here/lowlatd.env" "$dest$env_file"
    fi
    [ -z "$dest" ] || return 0
    systemctl daemon-reload
    systemctl enable lowlatd.service
    # Until it is logged in the service says so and exits, which is not a
    # failure; a restart picks up a new binary over an old install.
    systemctl restart lowlatd.service
    # For every user, the way a distribution enables its own: a login starts the
    # helper and the tray, and a session already open picks them up at its next.
    systemctl --global enable lowlat-session.service lowlat-tray.service
    udevadm control --reload-rules 2>/dev/null || true
    echo "lowlat: installed; log the host in with: sudo lowlat-login --install"
}

uninstall_all() {
    if [ -z "$dest" ]; then
        systemctl disable --now lowlatd.service 2>/dev/null || true
        systemctl --global disable lowlat-session.service lowlat-tray.service 2>/dev/null || true
    fi
    files | while read -r to mode from; do
        rm -f "$dest$to"
    done
    rmdir "$dest/usr/share/doc/lowlat" 2>/dev/null || true
    [ -z "$dest" ] || return 0
    systemctl daemon-reload
    udevadm control --reload-rules 2>/dev/null || true
    echo "lowlat: removed; $env_file is kept, and it carries the login's session"
}

case "${1:-}" in
    "") action=install_all ;;
    --uninstall) action=uninstall_all ;;
    *)
        echo "usage: $0 [--uninstall]" >&2
        exit 2
        ;;
esac
if [ -z "$dest" ] && [ "$(id -u)" -ne 0 ]; then
    echo "install: run it as root (sudo), or stage it with DESTDIR=..." >&2
    exit 1
fi
$action
