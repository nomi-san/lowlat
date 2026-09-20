#!/usr/bin/env python3
"""Capture the committed pad fixtures from a DualShock 4 or a DualSense on USB.

The pad-report path is tested against reports real pads produced, not against
reports the code under test wrote. For every Sony pad found on hidraw this
writes, under `<out>/<ds4|ds5>/`:

    descriptor.bin            the HID report descriptor, as the kernel exposes it
    feature-calibration.bin   the calibration feature report (DS4 0x02, DS5 0x05)
    feature-firmware.bin      the firmware feature report (DS4 0xA3, DS5 0x20)
    feature-pairing.bin       the pairing feature report (DS5 0x09; the DS4 v2
                              answers it empty over hidraw), both addresses zeroed
    input-idle.bin            the first input report read, nothing touched
    input-held.bin            with --held: the first report showing a touch
                              contact and the Cross button, waited for

A pad paired over Bluetooth writes the same set under a `bt-` prefix, in the
wireless framing (no descriptor: the host presents the USB one).

    scripts/capture-pad-fixtures.py [--held] [<out>]

Needs read access to the hidraw nodes (the seat's access rule grants it).
Run it with nothing touched, then again with --held holding a finger on each
touchpad and Cross down.
"""

import fcntl
import os
import select
import sys

VENDOR = 0x054C
PRODUCTS = {0x09CC: "ds4", 0x05C4: "ds4", 0x0CE6: "ds5"}
FEATURES = {
    "ds4": [("calibration", 0x02, 37), ("firmware", 0xA3, 49), ("pairing", 0x81, 7)],
    "ds5": [("calibration", 0x05, 41), ("firmware", 0x20, 64), ("pairing", 0x09, 20)],
}
# Contact byte (bit 7 set = no finger) and the button byte carrying Cross, in
# the USB report; the wireless report holds the same content further in.
HELD = {"ds4": (35, 5, 0x20), "ds5": (33, 8, 0x20)}
BT = {"ds4": (0x11, 3), "ds5": (0x31, 2)}


def gfeature(n):
    return (3 << 30) | (ord("H") << 8) | 0x07 | (n << 16)


def pads():
    for name in sorted(os.listdir("/sys/class/hidraw")):
        with open(f"/sys/class/hidraw/{name}/device/uevent") as f:
            fields = dict(line.strip().split("=", 1) for line in f if "=" in line)
        bus, vendor, product = (int(x, 16) for x in fields["HID_ID"].split(":"))
        if bus in (3, 5) and vendor == VENDOR and product in PRODUCTS:
            yield name, PRODUCTS[product], bus == 5


def read_feature(fd, rid, n):
    buf = bytearray(n)
    buf[0] = rid
    got = fcntl.ioctl(fd, gfeature(n), buf, True)
    return bytes(buf[:got])


def write(path, data):
    with open(path, "wb") as f:
        f.write(data)
    print(f"  {path}: {len(data)} bytes")


def main(argv):
    held = "--held" in argv
    args = [a for a in argv if a != "--held"]
    out = args[0] if args else "crates/core/tests/data/pad"
    for name, kind, wireless in pads():
        d = f"{out}/{kind}"
        prefix = f"{d}/bt-" if wireless else f"{d}/"
        os.makedirs(d, exist_ok=True)
        print(f"{name}: {kind}", "over Bluetooth" if wireless else "over USB")
        fd = os.open(f"/dev/hidraw{name[6:]}", os.O_RDWR | os.O_NONBLOCK)
        if not held:
            if not wireless:
                with open(f"/sys/class/hidraw/{name}/device/report_descriptor", "rb") as f:
                    write(f"{d}/descriptor.bin", f.read())
            for label, rid, n in FEATURES[kind]:
                if wireless and label == "pairing":
                    continue  # a checksum over addresses that are then zeroed
                data = read_feature(fd, rid, n)
                if not data:
                    print(f"  feature 0x{rid:02x} answered empty, not written")
                    continue
                if label == "pairing":
                    # The pad's address at 1..7 and the paired host's at 10..16;
                    # the constants between them stay.
                    data = data[:1] + bytes(6) + data[7:10] + bytes(6) + data[16:]
                write(f"{prefix}feature-{label}.bin", data)
        contact_at, button_at, cross = HELD[kind]
        rid, at = (BT[kind], 78) if wireless else ((0x01, 1), 64)
        (rid, shift), length = rid, at
        deadline = 60.0 if held else 2.0
        while True:
            ready, _, _ = select.select([fd], [], [], deadline)
            if not ready:
                print("  no report", "showing a held touch and Cross" if held else "")
                break
            report = os.read(fd, 128)
            if len(report) != length or report[0] != rid:
                continue
            if not held:
                write(f"{prefix}input-idle.bin", report)
                break
            usb = report[shift - 1:]
            if not usb[contact_at] & 0x80 and usb[button_at] & cross:
                write(f"{prefix}input-held.bin", report)
                break
        os.close(fd)


if __name__ == "__main__":
    main(sys.argv[1:])
