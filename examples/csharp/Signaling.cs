// Signaling, which is this application's and not the library's.
//
// **Nothing here touches the shared library.** That separation is the whole
// point of the seam: the SDK has no transport, no TLS stack and no JSON parser,
// and an application supplies all three. This one uses what its own runtime
// ships with -- `ClientWebSocket` and `System.Text.Json` -- and no package.
//
// Authentication is the query string on the upgrade, not a header and not a
// message. The service closes the socket without a reply when a parameter is
// missing, so a malformed URL presents as an immediate disconnect rather than
// as an error worth reading.

using System.Net.WebSockets;
using System.Text;
using System.Text.Json.Nodes;

namespace LowlatHost;

internal sealed class Signaling : IDisposable
{
    private const int Version = 1;
    private const string AppVersion = "150-104a";
    private const uint SdkVersion = 0x0006_0000;

    private readonly ClientWebSocket socket = new();
    private readonly byte[] receiving = new byte[64 * 1024];

    public async Task ConnectAsync(string server, string sessionId, CancellationToken token)
    {
        // The root path before the query is required: without it the request
        // line is malformed and the edge answers 400 rather than upgrading.
        var url =
            $"wss://{server.TrimEnd('/')}/?session_id={Uri.EscapeDataString(sessionId)}"
            + $"&role=host&version={Version}&build={Uri.EscapeDataString(AppVersion)}"
            + $"&sdk_version={SdkVersion}";

        // **Not optional, and not a health check.** The service sits behind an
        // edge that closes an idle websocket after about a hundred seconds, so
        // a host with nothing to say is disconnected roughly every two minutes
        // and only survives by reconnecting. The traffic has to come from this
        // side; answering the edge's own pings does not count.
        socket.Options.KeepAliveInterval = TimeSpan.FromSeconds(30);

        await socket.ConnectAsync(new Uri(url), token);

        // **Resent on every connection, not only the first.** The service takes
        // this as the frame that registers the session, so a reconnect without
        // it is a connection the service has not associated with this host.
        await socket.SendAsync(
            Encoding.UTF8.GetBytes("__ping__"), WebSocketMessageType.Text, true, token);
    }

    /// One message, or null when the connection closed.
    ///
    /// **A frame can arrive in pieces**, so this reassembles until the end of
    /// the message rather than treating one read as one message.
    public async Task<JsonNode?> ReceiveAsync(CancellationToken token)
    {
        var length = 0;
        while (true)
        {
            var room = new ArraySegment<byte>(receiving, length, receiving.Length - length);
            WebSocketReceiveResult got;
            try
            {
                got = await socket.ReceiveAsync(room, token);
            }
            catch (Exception error) when (error is WebSocketException or OperationCanceledException)
            {
                return null;
            }
            if (got.MessageType == WebSocketMessageType.Close)
            {
                return null;
            }
            length += got.Count;
            if (got.EndOfMessage)
            {
                break;
            }
            if (length == receiving.Length)
            {
                throw new InvalidOperationException("a signaling message did not fit");
            }
        }
        return JsonNode.Parse(Encoding.UTF8.GetString(receiving, 0, length));
    }

    public Task SendAsync(string action, JsonNode payload, CancellationToken token)
    {
        var envelope = new JsonObject
        {
            ["version"] = Version,
            ["action"] = action,
            ["payload"] = payload,
        };
        var bytes = Encoding.UTF8.GetBytes(envelope.ToJsonString());
        return socket.SendAsync(bytes, WebSocketMessageType.Text, true, token);
    }

    /// Publish this host into the discovery listing.
    ///
    /// **On state change only, never on a timer.** The service derives liveness
    /// from the connection itself, so a periodic advertisement adds load and
    /// buys nothing.
    public Task AdvertiseAsync(string name, uint capacity, uint players, CancellationToken token)
    {
        var payload = new JsonObject
        {
            ["loader_v"] = 0,
            ["service_v"] = 0,
            ["os"] = "linux",
            ["os_v"] = ReadOrEmpty("/proc/sys/kernel/osrelease"),
            ["platform"] = "linux",
            // A string, whatever a schema might suggest.
            ["app_v"] = AppVersion,
            ["sdk_v"] = SdkVersion,
            ["device_id"] = ReadOrEmpty("/etc/machine-id"),
            ["mode"] = "desktop",
            ["name"] = name,
            ["desc"] = "",
            ["game_id"] = "",
            ["secret"] = "",
            // Read from what admission will actually grant: a listing that
            // promises more capacity than that is a listing that lies.
            ["max_players"] = capacity,
            ["players"] = players,
            ["public"] = false,
            ["guests"] = new JsonArray(),
        };
        return SendAsync("conn_update", payload, token);
    }

    /// Answer an offer, approving or declining it.
    ///
    /// **Every offer gets an answer, including the ones turned down.** Nothing
    /// in the protocol reports a host that never replied, so silence leaves a
    /// peer connecting until its own deadline expires.
    public Task AnswerAsync(
        string attemptId,
        string to,
        bool approved,
        Credentials ours,
        CancellationToken token)
    {
        var creds = new JsonObject
        {
            ["fingerprint"] = approved ? Read(ours.Fingerprint) : "",
            ["ice_ufrag"] = approved ? Read(ours.Ufrag) : "",
            ["ice_pwd"] = approved ? Read(ours.Pwd) : "",
        };
        if (approved)
        {
            creds["aes256"] = Read(ours.Aes256);
        }
        var payload = new JsonObject
        {
            ["approved"] = approved,
            ["attempt_id"] = attemptId,
            ["data"] = new JsonObject
            {
                ["ver_data"] = 1,
                ["versions"] = Versions(),
                ["creds"] = creds,
            },
            ["to"] = to,
        };
        return SendAsync("answer", payload, token);
    }

    /// Forward one local candidate, or the readiness marker.
    public Task CandidateAsync(
        string attemptId,
        string to,
        string ip,
        ushort port,
        bool fromStun,
        bool sync,
        CancellationToken token)
    {
        var payload = new JsonObject
        {
            ["attempt_id"] = attemptId,
            ["data"] = new JsonObject
            {
                // Non-zero, or the peer rejects the candidate.
                ["ver_data"] = 1,
                ["versions"] = Versions(),
                ["ip"] = ip,
                ["port"] = port,
                ["lan"] = !fromStun && !sync,
                ["from_stun"] = fromStun,
                ["sync"] = sync,
            },
            ["to"] = to,
        };
        return SendAsync("candex", payload, token);
    }

    /// **All ones, deliberately.** The number is a promise the peer holds us
    /// to, and a higher value asks it to select framing this host does not
    /// implement.
    private static JsonObject Versions() => new()
    {
        ["bud"] = 1,
        ["control"] = 1,
        ["p2p"] = 1,
        ["audio"] = 1,
        ["init"] = 1,
        ["video"] = 1,
    };

    private static string Read(Credentials.IceField field) =>
        Text.Take(((ReadOnlySpan<byte>)field)[..Sizes.Ice]);

    private static string Read(Credentials.FingerprintField field) =>
        Text.Take(((ReadOnlySpan<byte>)field)[..Sizes.Fingerprint]);

    private static string ReadOrEmpty(string path)
    {
        try
        {
            return File.ReadAllText(path).Trim();
        }
        catch (IOException)
        {
            return "";
        }
    }

    public void Dispose() => socket.Dispose();
}
