# A host in C#

Phase 8's first gate: an application in another language and another runtime,
importing the shared object and nothing else, supplying its own signaling, and
taking a guest from an offer to a connected session.

**It imports `liblowlat.so` and no package.** The signaling is written against
what .NET ships with -- `ClientWebSocket` and `System.Text.Json` -- because a
seam proven by borrowing the library's own signaling is not proven at all. The
SDK has no transport, no TLS stack and no JSON parser, and this is what
supplying all three looks like.

## Running it

The shared library is the SDK crate's (`lowlat-sdk`, built as `liblowlat.so`
on Linux and `lowlat.dll` on Windows); the program finds it on the loader's
path.

```sh
cargo build --release -p lowlat-sdk
export LD_LIBRARY_PATH=$PWD/target/release

# The boundary alone, with no service behind it. Synthesises an offer and
# drives every call a real session makes.
LOWLAT_OFFLINE=1 dotnet run --project examples/csharp

# Against the real service.
LOWLAT_SESSION=<session id> dotnet run --project examples/csharp
```

On Windows, from PowerShell:

```powershell
cargo build --release -p lowlat-sdk
$env:PATH = "$PWD\target\release;$env:PATH"
$env:LOWLAT_OFFLINE = '1'; dotnet run --project examples/csharp
Remove-Item Env:LOWLAT_OFFLINE
$env:LOWLAT_SESSION = '<session id>'; dotnet run --project examples/csharp
```

**Ctrl+C closes the host**: every connected guest is told the host closed -- the
disconnect status a client shows as such -- and given up to two seconds to go
before the host stops. A stop alone tells a peer nothing, so each would wait out
its own deadline to learn the host had gone.

On Linux, capture needs the elevated capability, so `lowlat_can_host` answers
`LOWLAT_ERR_DISPLAY_UNREACHABLE` as an ordinary user. Run as root for a session
that carries pictures; everything else works either way. On Windows an ordinary
user hosts the desktop as it is; the lock screen and an elevation prompt are on
desktops only the system account reaches, so a host that must stream them runs
as the system account in the console session
([impl-plan-windows.md](../../docs/impl-plan-windows.md) W2).

## Holding guests to one application (Windows)

`LOWLAT_APPLICATIONS` names a JSON file of applications
(`{"applications": [{"name", "program", "arguments", "directory", "executables":
[...], "virtualInput", "place"}]}`); in a session, key `1` to `9` holds every
guest, the owner too, to that application and starts it, and `0` lets them go
([05 §7.4](../../docs/05-host.md), [06 §3](../../docs/06-api.md)). The guests
are held before the start, so from its first window. The program is started by
the desktop's shell, so unelevated, and its first window is found by the
executables its process runs and moved to the middle of its monitor (`center`)
or of an output named by its enumeration identity. `virtualInput` switches the
guests' keyboard and mouse to the input driver for it, which a game under an
anti-cheat that refuses injected pointer moves needs. Run such a host as the
signed-in user, elevated: the system account shows the held guests black, and
the input driver admits an administrator only elevated. `LOWLAT_HOLD=N` with
`LOWLAT_OFFLINE` holds and starts application `N` with no session.

## What is where

| File | |
|---|---|
| `Lowlat.cs` | the boundary, mirrored from `include/lowlat.h` by hand |
| `Signaling.cs` | Kessel, written against .NET's own sockets and JSON |
| `Host.cs` | the calls an application makes, and the event pump |
| `Program.cs` | admission policy, the reconnect loop, and the wiring |
| `Applications.cs` | the applications guests are held to, their start by the shell, their first window (Windows) |

## Three things worth reading before writing another integration

**No marshalling directives anywhere.** Every struct is blittable: fixed-size
fields, inline arrays, no pointers into managed memory. A C# `bool` is a
four-byte Win32 BOOL to the marshaller and pinning it to one byte takes a
`MarshalAs`, which is exactly the directive that stops a struct being
blittable -- so the mirrors use `byte` with a property over it. That is what lets
`LibraryImport` generate the interop at compile time instead of a runtime
marshaller walking each field.

**Two loops, not one.** Signaling arrives when the service sends it and events
arrive when the library raises them. Polling one from inside the other's wait
is what makes a host that answers late.

**An event that says a guest ended is not the guest being reaped.** The loop
has stopped, but the attempt stays registered until the application ends it,
and until then it holds that guest's number, its seat and its port. A live run
found this the obvious way: a peer that had disconnected was still on the
roster and still counted against capacity.

**A dropped signaling connection is not a dropped session.** The event pump
outlives a reconnect because guests do. The service's edge closes an idle
socket after about a hundred seconds, so the connection carries a keepalive and
the loop reconnects with bounded backoff; without both, a host is in the
listing for two minutes at a time.
