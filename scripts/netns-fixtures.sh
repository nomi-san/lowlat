#!/bin/bash
# Network namespace fixtures: the punch against a real kernel.
#
# The simulator proves the state machine against translation we described. This
# proves it against translation somebody else implemented, which is the only
# thing that checks the description. Six topologies, each stating the outcome it
# expects, and half of them expect failure.
#
# Requires root, and skips rather than fails when it cannot have it.
#
#   sudo scripts/netns-fixtures.sh [topology]
#
# Build the endpoint first:
#   CARGO_TARGET_DIR=/tmp/lowlat-target cargo build -p lowlat-sim --bin punch --release

set -uo pipefail

PUNCH=${PUNCH:-/tmp/lowlat-target/release/punch}
RUN=${RUN:-/tmp/lowlat-netns}
TIMEOUT_MS=12000
VERBOSE=${VERBOSE:-}

# One namespace per role. Named so a leaked one is obvious in `ip netns list`.
NAMESPACES="llnet llr2 llr3 llgwa llgwb llcgn llha llhb"

LEFT_PORT=5000
RIGHT_PORT=6000
LEFT_PUBLIC=203.0.113.1
RIGHT_PUBLIC=203.0.113.5
SERVER=203.0.113.254
SERVER_PORT=3478

LEFT_UFRAG=aaaa
LEFT_PWD=passwordforaaaa
RIGHT_UFRAG=bbbb
RIGHT_PWD=passwordforbbbb

pass=0
fail=0

log() { printf '%s\n' "$*"; }

cleanup() {
    for ns in $NAMESPACES; do
        ip netns del "$ns" 2>/dev/null
    done
    # KEEP leaves the endpoint output behind, which is the only way to see what
    # a topology actually did when it did the wrong thing.
    [[ -n ${KEEP:-} ]] || rm -rf "$RUN"
    return 0
}

# ns... -> create each with loopback up
mkns() {
    for ns in "$@"; do
        ip netns add "$ns" || return 1
        ip -n "$ns" link set lo up || return 1
    done
}

# ns_a if_a cidr_a ns_b if_b cidr_b
wire() {
    ip link add "$2" type veth peer name "$5" || return 1
    ip link set "$2" netns "$1" || return 1
    ip link set "$5" netns "$4" || return 1
    ip -n "$1" addr add "$3" dev "$2" || return 1
    ip -n "$1" link set "$2" up || return 1
    ip -n "$4" addr add "$6" dev "$5" || return 1
    ip -n "$4" link set "$5" up || return 1
}

forward() { ip netns exec "$1" sysctl -qw net.ipv4.ip_forward=1; }

# ns -> an empty nat table with both hooks
nat_table() {
    ip netns exec "$1" nft add table ip nat || return 1
    ip netns exec "$1" nft add chain ip nat pre '{ type nat hook prerouting priority -100 ; }' || return 1
    ip netns exec "$1" nft add chain ip nat post '{ type nat hook postrouting priority 100 ; }' || return 1
}

# ns ext_if port -> conntrack does the filtering, address and port dependent.
#
# The explicit port is not decoration. A bare masquerade reallocates the source
# port per destination, which is address-and-port-dependent mapping: the kernel
# default is a symmetric translator, not a cone. A fixture built on it looks
# like a port restricted cone, fails to punch, and confirms the opposite of what
# it was written to check. Pinning the external port is what makes the mapping
# endpoint independent, and with one socket behind the translator that is
# exactly what a cone does.
nat_port_restricted() {
    nat_table "$1" || return 1
    ip netns exec "$1" nft add rule ip nat post oifname "$2" meta l4proto udp masquerade to ":$3"
}

# ns ext_if -> a fresh mapping per destination, which makes the advertised
# address useless to the peer. This is the kernel default plus explicit
# randomisation, so it is the one topology that needs no special arrangement.
nat_symmetric() {
    nat_table "$1" || return 1
    ip netns exec "$1" nft add rule ip nat post oifname "$2" masquerade random
}

# ns ext_if inner_ip port -> anyone may use the mapping once it exists.
nat_full_cone() {
    nat_table "$1" || return 1
    ip netns exec "$1" nft add rule ip nat post oifname "$2" meta l4proto udp masquerade to ":$4" || return 1
    ip netns exec "$1" nft add rule ip nat pre iifname "$2" udp dport "$4" dnat to "$3:$4"
}

# ns ext_if inner_ip port -> only addresses the inside has sent to, on any port.
# The dynamic set is what expresses address-dependent filtering; conntrack alone
# cannot, because it keys on the port as well.
nat_restricted_cone() {
    nat_table "$1" || return 1
    ip netns exec "$1" nft add set ip nat contacted \
        '{ type ipv4_addr ; flags dynamic,timeout ; timeout 120s ; }' || return 1
    ip netns exec "$1" nft add rule ip nat post oifname "$2" \
        meta l4proto udp update @contacted "{ ip daddr }" masquerade to ":$4" || return 1
    ip netns exec "$1" nft add rule ip nat pre iifname "$2" ip saddr @contacted \
        udp dport "$4" dnat to "$3:$4"
}

# Start the reflexive server. It only ever reports the address it saw.
start_server() {
    ip netns exec llnet "$PUNCH" server --bind "$SERVER:$SERVER_PORT" \
        >"$RUN/server.out" 2>&1 &
    echo $! >"$RUN/server.pid"
    sleep 0.3
}

# ns bind_addr publish await ufrag pwd peer_ufrag peer_pwd seed out
start_peer() {
    ip netns exec "$1" "$PUNCH" peer \
        --bind "$2" \
        --server "$SERVER:$SERVER_PORT" \
        --publish "$3" --await "$4" \
        --local-ufrag "$5" --local-pwd "$6" \
        --remote-ufrag "$7" --remote-pwd "$8" \
        --seed "$9" --timeout-ms "$TIMEOUT_MS" $VERBOSE \
        >"${10}" 2>&1 &
}

# left_bind right_bind -> run both endpoints and wait
run_pair() {
    start_peer llha "$1" "$RUN/a.cand" "$RUN/b.cand" \
        "$LEFT_UFRAG" "$LEFT_PWD" "$RIGHT_UFRAG" "$RIGHT_PWD" 161 "$RUN/a.out"
    local a=$!
    start_peer llhb "$2" "$RUN/b.cand" "$RUN/a.cand" \
        "$RIGHT_UFRAG" "$RIGHT_PWD" "$LEFT_UFRAG" "$LEFT_PWD" 178 "$RUN/b.out"
    local b=$!
    wait "$a" 2>/dev/null
    wait "$b" 2>/dev/null
}

# name expected -> compare both outcomes against "established" or "failed"
judge() {
    local name=$1 expected=$2
    local a b
    a=$(grep -Eo '^(established|failed|timeout).*' "$RUN/a.out" | tail -1)
    b=$(grep -Eo '^(established|failed|timeout).*' "$RUN/b.out" | tail -1)

    local ok=1
    case $expected in
        established)
            [[ $a == established* && $b == established* ]] || ok=0 ;;
        failed)
            [[ $a == failed* && $b == failed* ]] || ok=0 ;;
    esac

    if [[ $ok == 1 ]]; then
        pass=$((pass + 1))
        log "  PASS $name: expected $expected, left [$a] right [$b]"
    else
        fail=$((fail + 1))
        log "  FAIL $name: expected $expected, left [$a] right [$b]"
        log "    reflexive: $(grep -h '^reflexive' "$RUN/a.out" "$RUN/b.out" | tr '\n' ' ')"
    fi
}

# Two hosts, each behind its own gateway, with three routers between them.
#
# The router count is load bearing, not scenery. A mapping probe is emitted at a
# TTL chosen so it dies inside the local network, and the whole point is that it
# opens our own mapping without the peer's gateway ever seeing it. On a short
# path the probe crosses the entire fabric, arrives at the far gateway before
# that side has sent anything, and creates a translation entry in the inbound
# direction; the far side's own outbound then matches that entry as a reply, so
# no inward path is ever established and both sides time out. Real distance is
# what stops it, so the fixture has to have some.
#
# The same length makes this the real form of the TTL regression: media crosses
# more hops than a probe can, so a socket left at the probe value carries
# nothing.
build_two_sided() {
    mkns llnet llr2 llr3 llgwa llgwb llha llhb || return 1
    # Each link is its own point to point subnet. Two links sharing one prefix
    # give a router two connected routes for it and the second is never used,
    # which presents as a punch that times out for no visible reason.
    wire llgwa exta "$LEFT_PUBLIC/30" llnet neta 203.0.113.2/30 || return 1
    wire llnet mid1 10.0.1.1/30 llr2 r2a 10.0.1.2/30 || return 1
    wire llr2 r2b 10.0.2.1/30 llr3 r3a 10.0.2.2/30 || return 1
    wire llr3 r3b 203.0.113.6/30 llgwb extb "$RIGHT_PUBLIC/30" || return 1
    wire llha inta 192.168.10.2/24 llgwa lana 192.168.10.1/24 || return 1
    wire llhb intb 192.168.20.2/24 llgwb lanb 192.168.20.1/24 || return 1

    for ns in llgwa llgwb llnet llr2 llr3; do
        forward "$ns" || return 1
    done

    ip -n llnet addr add "$SERVER/32" dev lo || return 1
    ip -n llha route add default via 192.168.10.1 || return 1
    ip -n llhb route add default via 192.168.20.1 || return 1
    ip -n llgwa route add default via 203.0.113.2 || return 1
    ip -n llgwb route add default via 203.0.113.6 || return 1
    ip -n llnet route add 203.0.113.4/30 via 10.0.1.2 || return 1
    ip -n llr2 route add default via 10.0.1.1 || return 1
    ip -n llr2 route add 203.0.113.4/30 via 10.0.2.2 || return 1
    ip -n llr3 route add default via 10.0.2.1 || return 1
}

topology_port_restricted() {
    build_two_sided || return 1
    nat_port_restricted llgwa exta $LEFT_PORT || return 1
    nat_port_restricted llgwb extb $RIGHT_PORT || return 1
    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.20.2:$RIGHT_PORT
    judge "port-restricted" established
}

topology_full_cone() {
    build_two_sided || return 1
    nat_full_cone llgwa exta 192.168.10.2 $LEFT_PORT || return 1
    nat_full_cone llgwb extb 192.168.20.2 $RIGHT_PORT || return 1
    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.20.2:$RIGHT_PORT
    judge "full-cone" established
}

topology_restricted_cone() {
    build_two_sided || return 1
    nat_restricted_cone llgwa exta 192.168.10.2 $LEFT_PORT || return 1
    nat_restricted_cone llgwb extb 192.168.20.2 $RIGHT_PORT || return 1
    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.20.2:$RIGHT_PORT
    judge "restricted-cone" established
}

topology_symmetric() {
    build_two_sided || return 1
    nat_symmetric llgwa exta || return 1
    nat_symmetric llgwb extb || return 1
    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.20.2:$RIGHT_PORT
    judge "symmetric" failed
}

# Two layers on the left: a customer translator behind a carrier one. Both keep
# mappings endpoint independent, so the punch must still work.
topology_carrier_grade() {
    # One more hop than the two-sided build. A probe must die before reaching
    # any of the far side's translators, and this side has two of them, so the
    # carrier translator is one hop further from the peer than a plain gateway
    # would be.
    mkns llnet llr2 llr3 llgwa llcgn llgwb llha llhb || return 1
    wire llgwa exta 100.64.0.2/30 llcgn cgni 100.64.0.1/30 || return 1
    wire llcgn cgne "$LEFT_PUBLIC/30" llnet neta 203.0.113.2/30 || return 1
    wire llnet mid1 10.0.1.1/30 llr2 r2a 10.0.1.2/30 || return 1
    wire llr2 r2b 10.0.2.1/30 llr3 r3a 10.0.2.2/30 || return 1
    wire llr3 r3b 203.0.113.6/30 llgwb extb "$RIGHT_PUBLIC/30" || return 1
    wire llha inta 192.168.10.2/24 llgwa lana 192.168.10.1/24 || return 1
    wire llhb intb 192.168.20.2/24 llgwb lanb 192.168.20.1/24 || return 1

    for ns in llgwa llcgn llgwb llnet llr2 llr3; do
        forward "$ns" || return 1
    done

    ip -n llnet addr add "$SERVER/32" dev lo || return 1
    ip -n llha route add default via 192.168.10.1 || return 1
    ip -n llhb route add default via 192.168.20.1 || return 1
    ip -n llgwa route add default via 100.64.0.1 || return 1
    ip -n llcgn route add default via 203.0.113.2 || return 1
    ip -n llgwb route add default via 203.0.113.6 || return 1
    ip -n llnet route add 203.0.113.4/30 via 10.0.1.2 || return 1
    ip -n llr2 route add default via 10.0.1.1 || return 1
    ip -n llr2 route add 203.0.113.4/30 via 10.0.2.2 || return 1
    ip -n llr3 route add default via 10.0.2.1 || return 1

    nat_port_restricted llgwa exta $LEFT_PORT || return 1
    nat_port_restricted llcgn cgne $LEFT_PORT || return 1
    nat_port_restricted llgwb extb $RIGHT_PORT || return 1

    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.20.2:$RIGHT_PORT
    judge "carrier-grade" established
}

# Both hosts behind one translator, reaching each other by its public address.
topology_hairpin() {
    mkns llnet llgwa llha llhb || return 1
    wire llgwa exta "$LEFT_PUBLIC/30" llnet neta 203.0.113.2/30 || return 1
    # Separate inside subnets for the same reason the uplinks are separate: two
    # interfaces on one prefix give the gateway an unusable second route.
    wire llha inta 192.168.10.2/30 llgwa lana 192.168.10.1/30 || return 1
    wire llhb intb 192.168.11.2/30 llgwa lanb 192.168.11.1/30 || return 1

    forward llgwa || return 1
    forward llnet || return 1
    ip -n llnet addr add "$SERVER/32" dev lo || return 1
    ip -n llha route add default via 192.168.10.1 || return 1
    ip -n llhb route add default via 192.168.11.1 || return 1
    ip -n llgwa route add default via 203.0.113.2 || return 1

    nat_table llgwa || return 1
    # One rule per inside host: each keeps its own external port, which is what
    # endpoint-independent mapping means when two sockets share a translator.
    ip netns exec llgwa nft add rule ip nat post oifname exta         ip saddr 192.168.10.2 meta l4proto udp masquerade to ":$LEFT_PORT" || return 1
    ip netns exec llgwa nft add rule ip nat post oifname exta         ip saddr 192.168.11.2 meta l4proto udp masquerade to ":$RIGHT_PORT" || return 1
    # Loopback: the public address, arriving from inside, is turned back inward.
    ip netns exec llgwa nft add rule ip nat pre ip daddr "$LEFT_PUBLIC" \
        udp dport $LEFT_PORT dnat to "192.168.10.2:$LEFT_PORT" || return 1
    ip netns exec llgwa nft add rule ip nat pre ip daddr "$LEFT_PUBLIC" \
        udp dport $RIGHT_PORT dnat to "192.168.11.2:$RIGHT_PORT" || return 1
    # And the source is rewritten so the reply returns through the translator.
    # Without this the far side answers directly and the datagram arrives from
    # an address the asker never sent to, which is a filtering drop.
    ip netns exec llgwa nft add rule ip nat post ip saddr 192.168.0.0/16 \
        ip daddr 192.168.0.0/16 masquerade || return 1

    start_server
    run_pair 192.168.10.2:$LEFT_PORT 192.168.11.2:$RIGHT_PORT
    judge "hairpin" established
}

run_topology() {
    log "$1:"
    cleanup
    mkdir -p "$RUN"
    if ! "topology_$(echo "$1" | tr - _)"; then
        fail=$((fail + 1))
        log "  FAIL $1: could not build the topology"
    fi
    if [[ -f $RUN/server.pid ]]; then
        kill "$(cat "$RUN/server.pid")" 2>/dev/null
    fi
    cleanup
}

if [[ $EUID -ne 0 ]]; then
    log "skipped: network namespace fixtures need root"
    exit 0
fi
if [[ ! -x $PUNCH ]]; then
    log "skipped: no endpoint at $PUNCH; build it first"
    exit 0
fi

trap cleanup EXIT

ALL="port-restricted full-cone restricted-cone symmetric carrier-grade hairpin"
if [[ $# -gt 0 ]]; then
    topologies=("$@")
else
    read -ra topologies <<<"$ALL"
fi
for topology in "${topologies[@]}"; do
    run_topology "$topology"
done

log ""
log "netns fixtures: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
