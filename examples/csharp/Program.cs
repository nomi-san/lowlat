// A host, driven entirely through the shared library's C boundary.
//
// This is Phase 8's first gate: an application in another language and another
// runtime, importing the shared object and nothing else, supplying its own
// signaling, and taking a guest from an offer to a connected session.
//
// The shape is the one every integration has:
//
//   create -> can_host -> host_start
//   signaling in  -> new_attempt / add_candidate / begin_p2p / end_connection
//   poll_events   -> candidates and state out, over signaling
//   host_stop -> destroy
//
// Run it with the session identifier the service issued:
//
//   LOWLAT_SESSION=... LOWLAT_SERVER=... dotnet run

using System.Runtime.InteropServices;
using System.Text.Json.Nodes;
using LowlatHost;

var session = Environment.GetEnvironmentVariable("LOWLAT_SESSION");
var server = Environment.GetEnvironmentVariable("LOWLAT_SERVER") ?? "kessel-ws.parsec.app";
var offline = Environment.GetEnvironmentVariable("LOWLAT_OFFLINE") is not null;

Console.WriteLine($"lowlat abi {Native.lowlat_abi_version() >> 16}.{Native.lowlat_abi_version() & 0xffff}");

// Logs before anything else, so a failure during setup is visible too.
unsafe
{
    delegate* unmanaged[Cdecl]<uint, byte*, void*, void> sink = &Logging.Line;
    Native.lowlat_set_log_callback((IntPtr)sink, IntPtr.Zero);
}
Native.lowlat_set_log_level((uint)LogLevel.Info);

// **Asked before anything is started.** A host that cannot capture fails deep
// in the stream loop, where "there is no display" and "this process may not
// read one" are indistinguishable without a log.
var able = Native.lowlat_can_host();
Console.WriteLine($"can host: {able} ({Text.Describe(able)})");

foreach (var output in Enumeration.Outputs())
{
    Console.WriteLine($"  output {output.id}  {output.width}x{output.height} at {output.x},{output.y}");
}

var host = new Host();
host.Create();
host.Start();

if (offline || session is null)
{
    // Everything except the service: the boundary is driven end to end with a
    // synthesised offer, which is what makes this runnable without credentials.
    Console.WriteLine(session is null && !offline
        ? "no LOWLAT_SESSION, running the boundary without signaling"
        : "LOWLAT_OFFLINE, running the boundary without signaling");
    host.DriveWithoutSignaling();
    host.Stop();
    host.Destroy();
    return 0;
}

using var cancel = new CancellationTokenSource();
Console.CancelKeyPress += (_, args) => { args.Cancel = true; cancel.Cancel(); };

// Who each attempt is with, so an event can be addressed back over signaling.
var peers = new Dictionary<string, string>();

// **Its own loop.** Signaling arrives when the service sends it and events
// arrive when the library raises them; polling one from inside the other's
// wait is what makes a host that answers late. It outlives a reconnect,
// because guests do: a signaling connection going away is not a session going
// away.
Signaling? current = null;
var events = Task.Run(
    () => host.PumpEvents(() => current, peers, cancel.Token), cancel.Token);

// **A dropped connection reconnects rather than exits.** The service's edge
// closes an idle socket, a network moves, a service restarts; a host that
// gives up on any of those is a host that is in the listing for two minutes.
var backoff = new Backoff();
while (!cancel.IsCancellationRequested)
{
    try
    {
        using var signaling = new Signaling();
        await signaling.ConnectAsync(server, session, cancel.Token);
        current = signaling;
        backoff.Reset();
        Console.WriteLine($"signaling: connected to {server}");
        await signaling.AdvertiseAsync("lowlat (C#)", host.Capacity, host.Guests(), cancel.Token);
        await Session.RunAsync(host, signaling, peers, cancel.Token);
        Console.WriteLine("signaling: closed");
    }
    catch (OperationCanceledException)
    {
        break;
    }
    catch (Exception error)
    {
        Console.WriteLine($"signaling: {error.Message}");
    }
    finally
    {
        current = null;
    }

    if (cancel.IsCancellationRequested)
    {
        break;
    }
    var delay = backoff.Next();
    Console.WriteLine($"signaling: reconnecting in {delay.TotalSeconds:0.0}s");
    try
    {
        await Task.Delay(delay, cancel.Token);
    }
    catch (OperationCanceledException)
    {
        break;
    }
}

cancel.Cancel();
try
{
    await events;
}
catch (OperationCanceledException)
{
    // Expected: the pump stops with the token.
}
host.Stop();
host.Destroy();
return 0;

/// One signaling connection, from connected to closed.
internal static class Session
{
    public static async Task RunAsync(
        Host host,
        Signaling signaling,
        Dictionary<string, string> peers,
        CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            var message = await signaling.ReceiveAsync(token);
            if (message is null)
            {
                return;
            }
            var action = message["action"]?.GetValue<string>() ?? "";
            var payload = message["payload"];
            if (payload is null)
            {
                continue;
            }

            switch (action)
            {
                case "offer_relay":
                {
                    var attemptId = payload["attempt_id"]!.GetValue<string>();
                    var from = payload["from"]!.GetValue<string>();
                    peers[attemptId] = from;
                    Console.WriteLine($"offer {attemptId} from {from}");

                    // Admission is this application's decision, and this
                    // application's policy is capacity alone. **Every offer
                    // gets an answer, including the ones turned down**: nothing
                    // reports a host that never replied, so silence leaves a
                    // peer connecting until its own deadline.
                    var registered = host.NewAttempt(attemptId, payload);
                    if (registered != Status.Ok)
                    {
                        Console.WriteLine($"  declined: {Text.Describe(registered)}");
                        await signaling.AnswerAsync(attemptId, from, false, default, token);
                        break;
                    }
                    var ours = host.BeginP2P(attemptId, out var approved);
                    if (approved != Status.Ok)
                    {
                        Console.WriteLine($"  could not approve: {Text.Describe(approved)}");
                        await signaling.AnswerAsync(attemptId, from, false, default, token);
                        break;
                    }
                    Console.WriteLine($"  approved, bound to port {ours.Port}");
                    await signaling.AnswerAsync(attemptId, from, true, ours, token);
                    break;
                }

                case "candex_relay":
                {
                    var attemptId = payload["attempt_id"]!.GetValue<string>();
                    var data = payload["data"];
                    if (data is not null)
                    {
                        host.AddCandidate(
                            attemptId,
                            data["ip"]?.GetValue<string>() ?? "",
                            (ushort)(data["port"]?.GetValue<int>() ?? 0),
                            data["sync"]?.GetValue<bool>() ?? false);
                    }
                    break;
                }

                case "offer_cancel_relay":
                {
                    var attemptId = payload["attempt_id"]!.GetValue<string>();
                    Console.WriteLine($"withdrawn {attemptId}");
                    host.EndConnection(attemptId);
                    peers.Remove(attemptId);
                    break;
                }
            }
        }
    }
}

/// Bounded exponential backoff with equal jitter.
///
/// **Jitter matters more than the growth rate.** Without it, every host that
/// was connected to a service that restarted wakes on the same schedule and
/// arrives together, which is the load the restart was already struggling
/// with. Half the step is fixed and half is drawn, so the wait is bounded
/// below as well as above.
internal sealed class Backoff
{
    private static readonly TimeSpan Initial = TimeSpan.FromSeconds(1);
    private static readonly TimeSpan Ceiling = TimeSpan.FromSeconds(5);

    private TimeSpan step = Initial;

    public void Reset() => step = Initial;

    public TimeSpan Next()
    {
        var half = step / 2;
        var drawn = TimeSpan.FromMilliseconds(
            Random.Shared.NextDouble() * half.TotalMilliseconds);
        var delay = half + drawn;
        step = step * 2 > Ceiling ? Ceiling : step * 2;
        return delay;
    }
}

internal static class Logging
{
    /// The one place the library calls out. Cold, fires on whichever thread
    /// logged, and must not call back in.
    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    internal static unsafe void Line(uint level, byte* message, void* opaque)
    {
        if (message is null)
        {
            return;
        }
        var text = Marshal.PtrToStringUTF8((IntPtr)message);
        Console.WriteLine($"  [{(LogLevel)level}] {text}");
    }
}

internal static class Enumeration
{
    public static List<(string id, uint width, uint height, uint x, uint y)> Outputs()
    {
        var found = new List<(string, uint, uint, uint, uint)>();
        unsafe
        {
            uint count = 0;
            if (Native.lowlat_get_outputs(null, &count) != Status.Ok || count == 0)
            {
                return found;
            }
            var listed = new Output[count];
            fixed (Output* room = listed)
            {
                if (Native.lowlat_get_outputs(room, &count) != Status.Ok)
                {
                    return found;
                }
            }
            for (var at = 0; at < count; at++)
            {
                found.Add((
                    Text.Take(((ReadOnlySpan<byte>)listed[at].Id)[..Sizes.Output]),
                    listed[at].Width,
                    listed[at].Height,
                    listed[at].X,
                    listed[at].Y));
            }
        }
        return found;
    }
}
