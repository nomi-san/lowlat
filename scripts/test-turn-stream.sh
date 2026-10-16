#!/usr/bin/env bash
# Isolated Linux coturn fixture. Only disposable keys/credentials and a local
# BUD peer; no account, captured media, host settings or production TURN used.
set -euo pipefail
task_tmp=$(mktemp -d)
task_pid=
cleanup() {
    if [[ -n "$task_pid" ]]; then kill "$task_pid" 2>/dev/null || true; wait "$task_pid" 2>/dev/null || true; fi
    rm -f "$task_tmp"/*
    rmdir "$task_tmp"
}
trap cleanup EXIT
task_ip=$(hostname -I | awk '{print $1}')
test -n "$task_ip"
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj '/CN=lowlat fixture CA' \
    -keyout "$task_tmp/ca.key" -out "$task_tmp/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=relay.test' \
    -keyout "$task_tmp/server.key" -out "$task_tmp/server.csr" >/dev/null 2>&1
cat >"$task_tmp/extensions" <<'EOF'
subjectAltName=DNS:relay.test
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -in "$task_tmp/server.csr" -CA "$task_tmp/ca.pem" -CAkey "$task_tmp/ca.key" \
    -CAcreateserial -days 2 -extfile "$task_tmp/extensions" -out "$task_tmp/server.pem" >/dev/null 2>&1
openssl x509 -in "$task_tmp/ca.pem" -outform DER -out "$task_tmp/ca.der"
cat >"$task_tmp/turn.conf" <<EOF
listening-ip=$task_ip
relay-ip=$task_ip
listening-port=34780
tls-listening-port=53490
min-port=50000
max-port=50100
realm=lowlat.fixture
user=fixture:fixture-password
lt-cred-mech
fingerprint
cert=$task_tmp/server.pem
pkey=$task_tmp/server.key
no-cli
no-dtls
no-multicast-peers
pidfile=$task_tmp/turn.pid
log-file=$task_tmp/turn.log
simple-log
EOF
turnserver -c "$task_tmp/turn.conf" >"$task_tmp/stdout.log" 2>&1 &
task_pid=$!
task_ready=false
for _ in $(seq 1 30); do
    kill -0 "$task_pid"
    if (exec 3<>"/dev/tcp/$task_ip/34780") 2>/dev/null; then task_ready=true; break; fi
    sleep 0.1
done
test "$task_ready" = true
export LOWLAT_TEST_TURN_ADDR="$task_ip:34780"
export LOWLAT_TEST_TURN_TLS_PORT=53490
export LOWLAT_TEST_TURN_CA="$task_tmp/ca.der"
if ! cargo test -p lowlat-net --features turn-stream --test turn_stream_bud -- --ignored --nocapture; then
    tail -n 60 "$task_tmp/stdout.log" "$task_tmp/turn.log" 2>/dev/null || true
    exit 1
fi
