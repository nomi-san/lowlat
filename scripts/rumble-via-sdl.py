#!/usr/bin/env python3
"""Rumble a pad through SDL3's HIDAPI driver: the road a game takes.

    rumble-via-sdl.py LIBSDL3 [/dev/hidrawN]

LIBSDL3 is a path to libSDL3.so.0 (the game launcher ships one under its
own directory; LD_LIBRARY_PATH may have to name that directory for the
library's own dependencies). Every joystick SDL sees is listed with its
path; the one named, or every one when none is, is opened and rumbled at
full strength for 1.5 s. A virtual pad of this host's answers with the
same output report a real pad is written, which the peer then receives
(docs/05-host.md section 7.2) -- run `cargo test -p lowlat-inject --lib
whatever_is_written -- --ignored --nocapture` beside this to watch it
arrive.
"""
import ctypes
import sys
import time


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    sdl = ctypes.CDLL(sys.argv[1])
    for hint in (b"SDL_JOYSTICK_HIDAPI", b"SDL_JOYSTICK_HIDAPI_PS4", b"SDL_JOYSTICK_HIDAPI_PS5"):
        sdl.SDL_SetHint(hint, b"1")
    sdl.SDL_GetJoysticks.restype = ctypes.POINTER(ctypes.c_uint32)
    sdl.SDL_GetJoysticks.argtypes = [ctypes.POINTER(ctypes.c_int)]
    sdl.SDL_GetJoystickNameForID.restype = ctypes.c_char_p
    sdl.SDL_GetJoystickPathForID.restype = ctypes.c_char_p
    sdl.SDL_OpenJoystick.restype = ctypes.c_void_p
    sdl.SDL_GetJoystickSerial.restype = ctypes.c_char_p
    sdl.SDL_GetJoystickSerial.argtypes = [ctypes.c_void_p]
    sdl.SDL_RumbleJoystick.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_uint16, ctypes.c_uint32]
    sdl.SDL_RumbleJoystick.restype = ctypes.c_bool
    sdl.SDL_CloseJoystick.argtypes = [ctypes.c_void_p]
    sdl.SDL_GetError.restype = ctypes.c_char_p
    if not sdl.SDL_Init(0x200):
        print(f"SDL_Init: {(sdl.SDL_GetError() or b'').decode()}")
        return 1
    want = sys.argv[2].encode() if len(sys.argv) > 2 else None
    time.sleep(0.5)
    count = ctypes.c_int(0)
    ids = sdl.SDL_GetJoysticks(ctypes.byref(count))
    for i in range(count.value):
        jid = ids[i]
        name = (sdl.SDL_GetJoystickNameForID(jid) or b"").decode()
        path = (sdl.SDL_GetJoystickPathForID(jid) or b"").decode()
        print(f"{jid}: {name} path={path}", flush=True)
    for i in range(count.value):
        jid = ids[i]
        path = sdl.SDL_GetJoystickPathForID(jid) or b""
        if want and path != want:
            continue
        joystick = sdl.SDL_OpenJoystick(jid)
        if not joystick:
            print(f"{jid}: open failed: {(sdl.SDL_GetError() or b'').decode()}")
            continue
        serial = (sdl.SDL_GetJoystickSerial(joystick) or b"").decode()
        print(f"opened {jid} serial={serial}", flush=True)
        time.sleep(0.3)
        ok = sdl.SDL_RumbleJoystick(joystick, 0xFFFF, 0xFFFF, 1500)
        print(f"rumble -> {'ok' if ok else (sdl.SDL_GetError() or b'').decode()}", flush=True)
        time.sleep(2.0)
        sdl.SDL_CloseJoystick(joystick)
    sdl.SDL_Quit()
    return 0


if __name__ == "__main__":
    sys.exit(main())
