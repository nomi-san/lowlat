// The protocol this application speaks over application messages.
//
// **None of this is the library's.** The wire carries a sub-identifier and a
// body and says nothing about either; what they mean is an agreement between
// an application and the clients it serves, and two applications using the
// same identifier are speaking different languages down one channel. The SDK
// hands the body over untouched, and this is where a language is chosen.
//
// The one an established client already speaks is the one implemented here,
// because it is the one a client asks in without being told to:
//
//   client -> host   9   ""              what is the video configuration
//   host   -> client 11  JSON            this is
//   client -> host   10  ""              what outputs are there
//   host   -> client 12  JSON array      these
//   client -> host   11  JSON            use this configuration
//
// A client asks 9 and 10 on connecting and again after it acts, so both
// answers have to be cheap and neither may block.

using System.Text.Json.Nodes;

namespace LowlatHost;

internal static class AppProtocol
{
    private const uint QueryConfig = 9;
    private const uint QueryOutputs = 10;
    private const uint Config = 11;
    private const uint Outputs = 12;
    /// The client asking for the secure attention sequence -- what
    /// `Ctrl+Alt+Del` reaches on Windows.
    private const uint SecureAttention = 14;

    /// The size a client sends to mean "keep whatever the host started with".
    ///
    /// **A sentinel, not a resolution**, and it has a companion: **zero means
    /// use the client's own initial size**, which it already told the host in
    /// its session initialization. Anything else is a real request. A host
    /// reading either sentinel as a request reports a refusal for a request
    /// nobody made.
    ///
    /// **Reporting them back is what ticks the panel's options**, and that half
    /// is deliberately not built: it means remembering which of the three modes
    /// was asked for and echoing the sentinel rather than the picture's size.
    /// It changes nothing about what is streamed, and this host does not set
    /// modes at all.
    private const int KeepHostResolution = 65535;
    private const int UseClientResolution = 0;

    /// The name a reader sends for "choose for me".
    ///
    /// **The same word means two things in the two directions**, which is not a
    /// contradiction: a stream with no output has none, and a request with no
    /// output wants whichever one this host would have picked anyway.
    private const string Auto = "none";

    /// Answer one message, if it is one this speaks.
    ///
    /// Answers whether it was handled, so a body meant for something else is
    /// visible rather than silently swallowed.
    public static bool OnMessage(Host host, uint guest, uint id, string body)
    {
        switch (id)
        {
            case QueryConfig:
                host.SendUserData(guest, Config, Describe(host).ToJsonString());
                return true;
            case QueryOutputs:
                host.SendUserData(guest, Outputs, ListOutputs().ToJsonString());
                return true;
            case Config:
                Apply(host, body);
                // **Not answered.** The client asks again with 9 the moment it
                // has sent one of these, so an answer here would arrive beside
                // the one it is about to ask for.
                return true;
            case SecureAttention:
                // **Nothing to do here, and that is the platform's answer
                // rather than a gap.** What this asks for on Windows is a
                // sequence user space is not allowed to synthesise, which is
                // exactly why it needs a message of its own. Nothing on Linux
                // is protected that way: `Ctrl+Alt+Del` is an ordinary
                // combination, so a guest that wants it presses it and the
                // keys arrive through the same path as any others.
                Console.WriteLine(
                    $"app: guest {guest} asked for the secure attention sequence, "
                    + "which this platform does not have");
                return true;
            default:
                return false;
        }
    }

    /// Describe the stream as it actually is, at the moment of the asking.
    ///
    /// **Built per query rather than once.** A display decides its own size and
    /// a host follows it, so a description made when the process started
    /// reports a stream nobody is producing -- and the size a peer is told is
    /// the coordinate space its absolute input comes back in.
    public static JsonObject Describe(Host host)
    {
        var video = host.VideoConfig();
        var state = host.State();
        var stream = new JsonObject
        {
            // What is being captured, never what was asked for -- and **never
            // empty**. A client shown a stream with no output has nothing to
            // mark in its chooser and nothing to switch away from, so before a
            // display has opened the one this host would take is reported
            // instead: the output at the desktop's corner, then whatever is
            // listed.
            ["output"] = video.output.Length > 0 ? video.output : Preferred(),
            ["encoderMaxBitrate"] = (uint)video.bitrateMbps,
            ["encoderFPS"] = video.fps,
            // The picture the display settled on. Zero before one is open,
            // which is the honest answer rather than a configured size.
            ["resolutionX"] = state.Width,
            ["resolutionY"] = state.Height,
            ["rotated"] = false,
            ["fullFPS"] = video.fullFps,
            // The platform a host declares itself as. Zero is "not said": only
            // a client reads it and what it does with it has not been found.
            ["hostOS"] = 0,
        };
        return new JsonObject
        {
            ["virtualTablet"] = 0,
            ["virtualMicrophone"] = 0,
            // **One, because this host produces one.** A reader takes as many
            // as the array holds and keeps its own defaults for the rest, so
            // padding it out describes streams nobody is producing.
            ["video"] = new JsonArray { stream },
        };
    }

    /// The output this host takes when nobody asks for one.
    ///
    /// **The desktop's corner, then whatever is lit** -- the same rule the
    /// library follows, derived from the rectangles enumeration reports. A
    /// second answer to one question is how a chooser ends up marking a screen
    /// the stream is not on.
    private static string Preferred()
    {
        var listed = Enumeration.Outputs();
        var corner = listed.FirstOrDefault(output => output.x == 0 && output.y == 0);
        return corner.id ?? listed.FirstOrDefault().id ?? "";
    }

    /// Every output this host could be asked to capture.
    private static JsonArray ListOutputs()
    {
        var listed = new JsonArray();
        foreach (var output in Enumeration.Outputs())
        {
            // **The name is what a person picks from and the identity is what
            // comes back**, so they are allowed to differ: the size is in the
            // name because that is what distinguishes two identical monitors,
            // and it must not be in the identity, which has to survive a mode
            // change.
            listed.Add(new JsonObject
            {
                ["id"] = output.id,
                ["name"] = $"{output.connector} ({output.width}x{output.height})",
                ["adapterName"] = output.id.Split(':')[0],
            });
        }
        return listed;
    }

    /// Take what a client asked for, and act on the part of it that is ours.
    private static void Apply(Host host, string body)
    {
        JsonNode? parsed;
        try
        {
            parsed = JsonNode.Parse(body);
        }
        catch (System.Text.Json.JsonException)
        {
            Console.WriteLine("app: a configuration arrived that is not JSON, ignoring it");
            return;
        }
        var first = parsed?["video"]?[0];
        if (first is null)
        {
            Console.WriteLine("app: a configuration arrived describing no stream, ignoring it");
            return;
        }

        var running = host.VideoConfig();
        var wanted = first["output"]?.GetValue<string>() ?? "";
        var output = running.output;
        switch (wanted)
        {
            // **An empty name means no change**, which is how a client asks for
            // everything else in the message without touching the output.
            case "":
                break;
            // **Choose for me.** The selection is cleared rather than pointed
            // somewhere, so the host goes back to whichever output it would
            // have taken on its own.
            case Auto:
                Console.WriteLine("app: guest asked for whichever output this host picks");
                output = "";
                break;
            default:
                if (wanted == running.output)
                {
                    break;
                }
                // **Checked against what is really there before it is acted
                // on.** A name nothing is lighting is refused where the display
                // is opened, and that refusal ends every guest on the stream
                // including the one that asked. A guest naming something that
                // is not there must cost nothing.
                if (Enumeration.Outputs().Any(real => real.id == wanted))
                {
                    Console.WriteLine($"app: guest asked to capture {wanted}");
                    output = wanted;
                }
                else
                {
                    Console.WriteLine(
                        $"app: guest asked to capture {wanted}, which nothing here is lighting");
                }
                break;
        }

        // **Both of these are ours to change while the host runs**, which is
        // what separates them from the resolution below: a bitrate re-bases
        // the budget and reaches the encoder through a reconfigure, and a
        // frame rate changes the pacing from the next frame.
        var fps = (uint)(first["encoderFPS"]?.GetValue<int>() ?? 0);
        var bitrate = (uint)(first["encoderMaxBitrate"]?.GetValue<int>() ?? 0);
        var fullFps = first["fullFPS"]?.GetValue<bool>() ?? running.fullFps;
        var applied = host.SetVideoConfig(
            fps == 0 ? running.fps : fps,
            bitrate == 0 ? running.bitrateMbps : bitrate,
            running.minBitrateMbps,
            fullFps,
            output);
        if (applied != Status.Ok)
        {
            Console.WriteLine($"app: that configuration was refused: {Text.Describe(applied)}");
        }

        // **Said rather than done.** A display decides its own size and this
        // host follows it; asking for another is a request to whoever owns the
        // display, which is not this application either. A request quietly
        // dropped looks like a host that ignored its guest.
        foreach (var (field, current) in new[]
                 {
                     ("resolutionX", host.State().Width),
                     ("resolutionY", host.State().Height),
                 })
        {
            var asked = first[field]?.GetValue<int>() ?? 0;
            // Neither sentinel is a request, and reporting one as refused
            // would be answering a question nobody asked.
            if (asked == UseClientResolution || asked == KeepHostResolution || asked == current)
            {
                continue;
            }
            Console.WriteLine($"app: guest asked for {field}={asked}, which is the display's");
        }
    }

    /// Tell every guest who is in the room.
    ///
    /// **Sent whenever the room changes, not on a timer and not on request.** A
    /// peer has no way to ask, and it needs this to find itself: it matches its
    /// own number against the list and takes that entry as what it is allowed
    /// to do. A client that never receives one does not know what it is, and
    /// its settings panel does not exist.
    ///
    /// **The shape is the reader's**, down to details that look pointless from
    /// here: a version stamp of two, an always-empty external identifier, and
    /// exactly three per-stream metric blocks whether or not there are three
    /// streams. A reader that requires a field it does not find falls back to
    /// its own idea of the world, and the failure is silence rather than an
    /// error.
    public static void SendRoster(Host host)
    {
        var roster = new JsonArray();
        foreach (var guest in host.Roster())
        {
            // **Real numbers, because the boundary can now answer for them.**
            // The metric blocks exist in the reader's shape either way; filling
            // them with what the congestion controller is actually steering by
            // beats filling them with zeros.
            var metrics = host.Metrics(guest.Number);
            roster.Add(new JsonObject
            {
                ["_version"] = 2,
                ["id"] = guest.Number,
                ["userID"] = 0,
                ["name"] = $"guest {guest.Number}",
                ["externalID"] = "",
                ["has_avatar"] = false,
                ["owner"] = guest.Owner,
                ["perms"] = new JsonObject
                {
                    ["gamepad"] = guest.Permissions.Gamepad,
                    ["keyboard"] = guest.Permissions.Keyboard,
                    ["mouse"] = guest.Permissions.Pointer,
                },
                ["audio"] = Block(default),
                ["control"] = Block(default),
                ["metrics"] = new JsonArray { Block(metrics), Block(default), Block(default) },
            });
        }
        var reached = host.SendRosterBody(roster.ToJsonString());
        Console.WriteLine($"app: told {reached} guest(s) who is in the room");
    }

    private static JsonObject Block(Metrics metrics) => new()
    {
        ["packetsSent"] = metrics.Frames,
        ["fastRTs"] = metrics.Stale,
        ["slowRTs"] = 0,
        ["cgEvents"] = metrics.CgEvents,
        ["encodeLatency"] = metrics.EncodeMs,
        // The peer's own, which this host cannot know and will not invent.
        ["decodeLatency"] = 0.0,
        ["networkLatency"] = metrics.NetworkMs,
        ["bitrate"] = metrics.BitrateMbps,
    };

    /// The guests as the discovery listing carries them.
    public static JsonArray Guests(Host host)
    {
        var listed = new JsonArray();
        foreach (var guest in host.Roster())
        {
            listed.Add(new JsonObject
            {
                ["guest_id"] = guest.Number,
                ["user_id"] = 0,
                ["gamepad"] = guest.Permissions.Gamepad,
                ["keyboard"] = guest.Permissions.Keyboard,
                ["mouse"] = guest.Permissions.Pointer,
            });
        }
        return listed;
    }
}
