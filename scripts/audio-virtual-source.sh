#!/usr/bin/env bash
# A second sound output on a machine that has one.
#
# A host that captures the desktop's sound can only be shown to follow a source
# change if there is somewhere to change to. This creates an output that the
# desktop offers like any other -- it appears in the system's sound settings,
# applications can be moved to it, and it can be made the default -- whose
# monitor is capturable exactly like a real one.
#
#   audio-virtual-source.sh up [name]        create the output
#   audio-virtual-source.sh down [name]      remove it
#   audio-virtual-source.sh status [name]    what exists, and what is default
#   audio-virtual-source.sh tone [name] [hz] play a note into it
#   audio-virtual-source.sh quiet [name]     stop that note
#
# Defaults to `lowlat_virtual`. Nothing is played into it unless `tone` is
# asked for: switching the desktop's output to it moves the machine's own sound
# there, and a note on top of that is noise. The note is for the other case --
# proving which source a capture is on while nothing else is playing.
#
# Nothing at the desk plays it, because there is no hardware behind it. What a
# guest hears is its monitor, which is the point.
#
# To switch the whole desktop to it, which is the change a host follows:
#
#   pactl set-default-sink lowlat_virtual
#   pactl set-default-sink alsa_output.pci-0000_10_00.6.analog-stereo
#
# It leaves nothing behind: the module is unloaded by name and the note is a
# process this script owns. Nothing here needs privilege, and it touches no
# device the machine is already using.

set -u

name="${2:-lowlat_virtual}"
hz="${3:-440}"
tone_pid_file="${XDG_RUNTIME_DIR:-/tmp}/${name}.tone.pid"

# The module that created this output, found by what it was created with rather
# than by an identifier remembered somewhere: a state file that is lost or stale
# leaves an output nothing can remove.
module_of() {
    pactl list short modules 2>/dev/null |
        awk -v want="sink_name=$1" '$0 ~ want { print $1; exit }'
}

tone_running() {
    [ -f "$tone_pid_file" ] && kill -0 "$(cat "$tone_pid_file")" 2>/dev/null
}

start_tone() {
    ffmpeg -hide_banner -loglevel error -nostdin \
        -f lavfi -i "sine=frequency=$hz:sample_rate=48000" \
        -ac 2 -f pulse -name lowlat-tone -stream_name "$name tone" \
        -device "$name" - >/dev/null 2>&1 &
    echo $! >"$tone_pid_file"
    sleep 0.5
    tone_running
}

stop_tone() {
    tone_running || return 1
    kill "$(cat "$tone_pid_file")" 2>/dev/null
    rm -f "$tone_pid_file"
}

case "${1:-status}" in
up)
    if [ -n "$(module_of "$name")" ]; then
        echo "$name is already there"
    else
        # **No spaces in the description.** The server's own parser for these
        # arguments splits on them whatever they are quoted with, and the
        # result is an output called "lowlat" rather than a failure.
        pactl load-module module-null-sink \
            sink_name="$name" \
            sink_properties="device.description=${name}-virtual-output" \
            rate=48000 channels=2 >/dev/null || exit 1
        echo "created $name"
    fi
    echo "select it in the desktop's sound settings, or:"
    echo "  pactl set-default-sink $name"
    echo "capture it as $name.monitor"
    ;;

tone)
    [ -n "$(module_of "$name")" ] || { echo "$name is not there"; exit 1; }
    if tone_running; then
        echo "a note is already playing into $name"
    elif start_tone; then
        echo "playing $hz Hz into $name"
    else
        echo "the note did not start"
        exit 1
    fi
    ;;

quiet)
    stop_tone && echo "stopped the note" || echo "no note was playing"
    ;;

down)
    stop_tone && echo "stopped the note"
    module="$(module_of "$name")"
    if [ -n "$module" ]; then
        # **A capture on this monitor is moved rather than ended.** The server
        # hands the stream to the default output's monitor, which is the same
        # path a device being unplugged takes.
        pactl unload-module "$module" && echo "removed $name"
    else
        echo "$name is not there"
    fi
    ;;

status)
    module="$(module_of "$name")"
    if [ -n "$module" ]; then
        echo "$name is loaded as module $module"
    else
        echo "$name is not loaded"
    fi
    tone_running && echo "a note is playing into it" || echo "no note is playing"
    echo "--- outputs ---"
    pactl list short sinks
    echo "--- default ---"
    pactl get-default-sink
    ;;

*)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
