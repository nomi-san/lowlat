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

```sh
cargo build --release -p lowlat-host
export LD_LIBRARY_PATH=$PWD/target/release

# The boundary alone, with no service behind it. Synthesises an offer and
# drives every call a real session makes.
LOWLAT_OFFLINE=1 dotnet run --project examples/csharp

# Against the real service.
LOWLAT_SESSION=<session id> dotnet run --project examples/csharp
```

Capture needs the elevated capability, so `lowlat_can_host` answers
`LOWLAT_ERR_DISPLAY_UNREACHABLE` as an ordinary user. Run as root for a session
that carries pictures; everything else works either way.

## What is where

| File | |
|---|---|
| `Lowlat.cs` | the boundary, mirrored from `include/lowlat.h` by hand |
| `Signaling.cs` | Kessel, written against .NET's own sockets and JSON |
| `Host.cs` | the calls an application makes, and the event pump |
| `Program.cs` | admission policy, the reconnect loop, and the wiring |

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

**A dropped signaling connection is not a dropped session.** The event pump
outlives a reconnect because guests do. The service's edge closes an idle
socket after about a hundred seconds, so the connection carries a keepalive and
the loop reconnects with bounded backoff; without both, a host is in the
listing for two minutes at a time.
