// The host, as this application drives it.
//
// Everything below is a call across the C boundary. What the library owns is
// capture, encode, transport and input; what this owns is admission policy and
// getting messages to and from the service.

using System.Text.Json.Nodes;

namespace LowlatHost;

internal sealed class Host
{
    public uint Capacity { get; } = 4;

    private IntPtr handle;

    public void Create()
    {
        unsafe
        {
            var info = new CreateInfo { Size = (uint)sizeof(CreateInfo) };
            IntPtr created;
            var made = Native.lowlat_create(&info, &created);
            if (made != Status.Ok)
            {
                throw new InvalidOperationException($"create: {Text.Describe(made)}");
            }
            handle = created;
        }
    }

    public void Start()
    {
        unsafe
        {
            var cfg = new HostConfig
            {
                Size = (uint)sizeof(HostConfig),
                BasePort = 8000,
                MaxGuests = Capacity,
                Codec = (uint)Codec.H264,
                // **The right default.** A conversion target is allocated on
                // the device the display is on, and an encoder belonging to
                // another cannot take it.
                Encoder = (uint)Encoder.FollowDisplay,
                CgLevel = (uint)CgLevel.Sensitive,
                ExclusiveHoldMs = 500,
                ExclusivePointer = false,
                ServerCount = 1,
                Video = new HostVideoConfig
                {
                    Size = (uint)sizeof(HostVideoConfig),
                    // A ceiling over whatever the display runs at, not a target.
                    Fps = 60,
                    BitrateMbps = 10.0,
                    MinBitrateMbps = 1.0,
                    FullFps = true,
                },
            };
            Text.Put(((Span<byte>)cfg.Servers)[..Sizes.Server], "3.145.150.90:3478");
            // Empty: whichever output this host would pick on its own, which is
            // the one at the desktop's corner.
            Text.Put(((Span<byte>)cfg.Video.Output)[..Sizes.Output], "");

            var started = Native.lowlat_host_start(handle, &cfg);
            if (started != Status.Ok)
            {
                throw new InvalidOperationException($"host_start: {Text.Describe(started)}");
            }
        }
        Console.WriteLine("host: started");
    }

    public void Stop() => Native.lowlat_host_stop(handle);

    public void Destroy()
    {
        Native.lowlat_destroy(handle);
        handle = IntPtr.Zero;
    }

    /// Every connected guest, with what it may drive and the attempt it came
    /// from.
    public List<Guest> Roster()
    {
        var found = new List<Guest>();
        unsafe
        {
            uint count = 0;
            if (Native.lowlat_host_get_guests(handle, null, &count) != Status.Ok || count == 0)
            {
                return found;
            }
            var room = new Guest[count];
            fixed (Guest* into = room)
            {
                if (Native.lowlat_host_get_guests(handle, into, &count) != Status.Ok)
                {
                    return found;
                }
            }
            found.AddRange(room.Take((int)count));
        }
        return found;
    }

    /// What one guest is doing.
    public Metrics Metrics(uint guest)
    {
        unsafe
        {
            var metrics = new Metrics { Size = (uint)sizeof(Metrics) };
            return Native.lowlat_host_get_metrics(handle, guest, &metrics) == Status.Ok
                ? metrics
                : default;
        }
    }

    /// What the stream is running at, read back rather than remembered: a
    /// guest may have changed it, and a display may have moved by itself.
    public (uint fps, double bitrateMbps, double minBitrateMbps, bool fullFps, string output)
        VideoConfig()
    {
        unsafe
        {
            var cfg = new HostVideoConfig { Size = (uint)sizeof(HostVideoConfig) };
            if (Native.lowlat_host_get_video_config(handle, &cfg) != Status.Ok)
            {
                return (0, 0, 0, true, "");
            }
            return (
                cfg.Fps,
                cfg.BitrateMbps,
                cfg.MinBitrateMbps,
                cfg.FullFps,
                Text.Take(((ReadOnlySpan<byte>)cfg.Output)[..Sizes.Output]));
        }
    }

    public Status SetVideoConfig(
        uint fps, double bitrateMbps, double minBitrateMbps, bool fullFps, string output)
    {
        unsafe
        {
            var cfg = new HostVideoConfig
            {
                Size = (uint)sizeof(HostVideoConfig),
                Fps = fps,
                BitrateMbps = bitrateMbps,
                MinBitrateMbps = minBitrateMbps,
                FullFps = fullFps,
            };
            Text.Put(((Span<byte>)cfg.Output)[..Sizes.Output], output);
            return Native.lowlat_host_set_video_config(handle, &cfg);
        }
    }

    public HostStatus State()
    {
        unsafe
        {
            var state = new HostStatus { Size = (uint)sizeof(HostStatus) };
            return Native.lowlat_host_get_status(handle, &state) == Status.Ok ? state : default;
        }
    }

    public void SendUserData(uint guest, uint id, string body)
    {
        unsafe
        {
            var bytes = System.Text.Encoding.UTF8.GetBytes(body);
            fixed (byte* data = bytes)
            {
                var sent = Native.lowlat_host_send_user_data(
                    handle, guest, id, data, (uint)bytes.Length);
                Console.WriteLine(
                    sent == Status.Ok
                        ? $"app: answered guest {guest} id={id} {body}"
                        : $"app: guest {guest} could not be answered with id={id}");
            }
        }
    }

    /// Tell every guest who is in the room. Answers how many it reached.
    public uint SendRosterBody(string body)
    {
        unsafe
        {
            var bytes = System.Text.Encoding.UTF8.GetBytes(body);
            uint reached = 0;
            fixed (byte* data = bytes)
            {
                Native.lowlat_host_send_roster(handle, data, (uint)bytes.Length, &reached);
            }
            return reached;
        }
    }

    /// Register an offer. Registering is not approving: no socket is opened and
    /// no thread starts until the attempt is approved.
    public Status NewAttempt(string attemptId, JsonNode payload)
    {
        unsafe
        {
            var creds = payload["data"]?["creds"];
            var perms = payload["permissions"];
            var info = new AttemptInfo
            {
                Size = (uint)sizeof(AttemptInfo),
                Permissions = new Permissions
                {
                    // **Everything when the field is absent.** The service
                    // sends it on every offer; reading silence as a refusal
                    // would deny input to a peer whose service simply did not
                    // say, and that looks like broken input rather than policy.
                    Keyboard = perms?["keyboard"]?.GetValue<bool>() ?? true,
                    Pointer = perms?["mouse"]?.GetValue<bool>() ?? true,
                    Gamepad = perms?["gamepad"]?.GetValue<bool>() ?? true,
                },
                Owner = payload["is_owner"]?.GetValue<bool>() ?? false,
            };
            Text.Put(((Span<byte>)info.Id)[..Sizes.Attempt], attemptId);
            Text.Put(((Span<byte>)info.Ufrag)[..Sizes.Ice],
                creds?["ice_ufrag"]?.GetValue<string>() ?? "");
            Text.Put(((Span<byte>)info.Pwd)[..Sizes.Ice],
                creds?["ice_pwd"]?.GetValue<string>() ?? "");
            // Empty selects the legacy crypto path, which is a decision the
            // offer made rather than a degradation.
            Text.Put(((Span<byte>)info.Aes256)[..Sizes.Ice],
                creds?["aes256"]?.GetValue<string>() ?? "");
            return Native.lowlat_host_new_attempt(handle, &info);
        }
    }

    /// Approve, and take back the credentials to answer with.
    public Credentials BeginP2P(string attemptId, out Status status)
    {
        unsafe
        {
            var ours = new Credentials { Size = (uint)sizeof(Credentials) };
            var id = System.Text.Encoding.UTF8.GetBytes(attemptId + "\0");
            fixed (byte* name = id)
            {
                status = Native.lowlat_host_begin_p2p(handle, name, &ours);
            }
            return ours;
        }
    }

    public void AddCandidate(string attemptId, string ip, ushort port, bool sync)
    {
        unsafe
        {
            var cand = new Candidate
            {
                Size = (uint)sizeof(Candidate),
                Port = port,
                Sync = sync,
            };
            Text.Put(((Span<byte>)cand.Address)[..Sizes.Address], ip);
            var id = System.Text.Encoding.UTF8.GetBytes(attemptId + "\0");
            fixed (byte* name = id)
            {
                Native.lowlat_host_add_candidate(handle, name, &cand);
            }
        }
    }

    public void EndConnection(string attemptId)
    {
        unsafe
        {
            var id = System.Text.Encoding.UTF8.GetBytes(attemptId + "\0");
            fixed (byte* name = id)
            {
                Native.lowlat_host_end_connection(handle, name);
            }
        }
    }

    /// Drive the whole boundary with no service behind it.
    ///
    /// **Everything except the transport.** An offer is synthesised, approved,
    /// given a candidate and ended, and every call is the same one a real
    /// session makes -- which is what lets this be run and read without
    /// credentials, and what makes it a check rather than a demonstration.
    public void DriveWithoutSignaling()
    {
        const string attempt = "offline-attempt";
        var offer = new JsonObject
        {
            ["data"] = new JsonObject
            {
                ["creds"] = new JsonObject
                {
                    ["ice_ufrag"] = "G+sZxQ==",
                    ["ice_pwd"] = "Det3D+arYViymh6I2v7UaOnrsHieoTRE",
                },
            },
            ["permissions"] = new JsonObject
            {
                ["keyboard"] = true,
                ["mouse"] = true,
                ["gamepad"] = false,
            },
            ["is_owner"] = true,
        };

        Expect(NewAttempt(attempt, offer), "new_attempt");
        var ours = BeginP2P(attempt, out var approved);
        Expect(approved, "begin_p2p");
        if (ours.Port == 0)
        {
            throw new InvalidOperationException("approval reported no bound port");
        }
        Console.WriteLine($"offline: approved, bound to port {ours.Port}");

        AddCandidate(attempt, "", 41000, sync: true);
        AddCandidate(attempt, "192.168.1.100", 41000, sync: false);

        unsafe
        {
            var state = new HostStatus { Size = (uint)sizeof(HostStatus) };
            Expect(Native.lowlat_host_get_status(handle, &state), "get_status");
            Console.WriteLine(
                $"offline: running={state.Running} guests={state.Guests} "
                + $"picture={state.Width}x{state.Height}");
            if (!state.Running || state.Guests != 1)
            {
                throw new InvalidOperationException("the host does not report the guest it seated");
            }
        }

        var guest = FirstGuest();
        var body = System.Text.Encoding.UTF8.GetBytes("hello from C#");
        unsafe
        {
            fixed (byte* data = body)
            {
                Expect(
                    Native.lowlat_host_send_user_data(handle, guest, 9, data, (uint)body.Length),
                    "send_user_data");
            }
            // There is no separate call to turn a guest's input off: that is
            // this one with every flag cleared.
            var perms = new Permissions { Keyboard = false, Pointer = true, Gamepad = false };
            Expect(Native.lowlat_host_set_permissions(handle, guest, &perms), "set_permissions");
        }
        if (FirstGuestPermissions().Keyboard)
        {
            throw new InvalidOperationException("the roster did not follow the change");
        }

        // **Zero is not a reason.** A peer carries on through a status of zero.
        if (Native.lowlat_host_kick_guest(handle, guest, 0) != Status.ErrInvalidArgument)
        {
            throw new InvalidOperationException("a status a peer ignores was accepted as a reason");
        }
        Expect(Native.lowlat_host_kick_guest(handle, guest, -15000), "kick_guest");

        // The application protocol, driven the way a client drives it: it asks
        // 9 and 10 on connecting and again after it acts.
        if (!AppProtocol.OnMessage(this, guest, 9, "") || !AppProtocol.OnMessage(this, guest, 10, ""))
        {
            throw new InvalidOperationException("a query this application speaks was not handled");
        }
        if (AppProtocol.OnMessage(this, guest, 4242, ""))
        {
            throw new InvalidOperationException("a message meant for something else was swallowed");
        }
        var described = AppProtocol.Describe(this);
        foreach (var field in new[] { "output", "encoderFPS", "resolutionX", "fullFPS" })
        {
            if (described["video"]?[0]?[field] is null)
            {
                throw new InvalidOperationException($"the description carries no {field}");
            }
        }
        // A client sending one back, which is how it asks for a different
        // output or rate.
        AppProtocol.OnMessage(this, guest, 11, described.ToJsonString());
        AppProtocol.SendRoster(this);

        // Whatever the seam raised on the way through, drained rather than
        // discarded: the queue outlives the session that filled it.
        DrainEvents();

        // **An attempt that is not reaped holds its seat.** The guest's loop
        // has stopped, but the attempt stays registered until the application
        // says so, and until then the roster still lists it and capacity still
        // counts it. A live run found this: a peer that had gone stayed in the
        // listing and its seat never came back.
        EndConnection(attempt);
        if (Roster().Count != 0)
        {
            throw new InvalidOperationException(
                "a guest whose attempt was ended is still on the roster");
        }
        Console.WriteLine("offline: the whole boundary ran");
    }

    private uint FirstGuest()
    {
        unsafe
        {
            uint count = 1;
            var one = new Guest[1];
            fixed (Guest* room = one)
            {
                Expect(Native.lowlat_host_get_guests(handle, room, &count), "get_guests");
            }
            return one[0].Number;
        }
    }

    private Permissions FirstGuestPermissions()
    {
        unsafe
        {
            uint count = 1;
            var one = new Guest[1];
            fixed (Guest* room = one)
            {
                Expect(Native.lowlat_host_get_guests(handle, room, &count), "get_guests");
            }
            return one[0].Permissions;
        }
    }

    private void DrainEvents()
    {
        var body = new byte[4096];
        while (true)
        {
            var polled = Poll(0, body, out var ev, out _);
            if (polled != Status.Ok)
            {
                return;
            }
            Console.WriteLine($"offline: event {(EventType)ev.Kind}");
        }
    }

    private static void Expect(Status status, string what)
    {
        if (status != Status.Ok)
        {
            throw new InvalidOperationException($"{what}: {Text.Describe(status)}");
        }
    }

    /// One poll, waiting up to `timeoutMs` for an event.
    ///
    /// A body that does not fit consumes nothing: the length it needed comes
    /// back and the same event is delivered by the next call with room for it.
    private Status Poll(uint timeoutMs, byte[] body, out Event ev, out uint written)
    {
        unsafe
        {
            Event taken = default;
            uint room = (uint)body.Length;
            Status polled;
            fixed (byte* into = body)
            {
                polled = Native.lowlat_host_poll_events(handle, timeoutMs, &taken, into, &room);
            }
            ev = taken;
            written = room;
            return polled;
        }
    }

    /// Take events and act on them, forwarding what belongs to the peer.
    ///
    /// **Its own loop.** Signaling arrives when the service sends it and events
    /// arrive when the library raises them; polling one from inside the other's
    /// wait is what makes a host that answers late.
    public async Task PumpEvents(
        Func<Signaling?> connection,
        Dictionary<string, string> peers,
        CancellationToken token)
    {
        var body = new byte[64 * 1024];
        while (!token.IsCancellationRequested)
        {
            // **Its own method, because C# will not take the address of a
            // local inside an async one.** The poll blocks for its timeout, so
            // it also keeps that wait out of the async machinery.
            var polled = Poll(100, body, out var ev, out var written);
            if (polled == Status.Timeout)
            {
                continue;
            }
            if (polled != Status.Ok)
            {
                Console.WriteLine($"poll: {Text.Describe(polled)}");
                continue;
            }
            if (ev.Dropped > 0)
            {
                Console.WriteLine($"poll: {ev.Dropped} event(s) were dropped before this one");
            }

            // **Whatever connection is current, which may be none.** A
            // signaling connection going away is not a session going away:
            // guests stay connected across a reconnect, and what cannot be
            // forwarded during one is a candidate the peer will not receive
            // rather than a reason to stop.
            var signaling = connection();

            switch ((EventType)ev.Kind)
            {
                case EventType.Candidate:
                {
                    var (attempt, address, port, fromStun) = Events.Candidate(ev);
                    if (signaling is not null && peers.TryGetValue(attempt, out var to))
                    {
                        await signaling.CandidateAsync(
                            attempt, to, address, port, fromStun, false, token);
                    }
                    break;
                }
                case EventType.Ready:
                {
                    var attempt = Events.Attempt(ev);
                    if (signaling is not null && peers.TryGetValue(attempt, out var to))
                    {
                        // A readiness marker rather than an address, and the
                        // peer may withhold every real candidate until it sees
                        // one.
                        await signaling.CandidateAsync(attempt, to, "", 0, false, true, token);
                    }
                    break;
                }
                case EventType.Established:
                    Console.WriteLine($"established {Events.Attempt(ev)}");
                    // **The roster is what makes a client's settings panel
                    // exist at all.** A peer cannot ask for one and finds
                    // itself in the list by number; without it a guest does
                    // not know what it is.
                    AppProtocol.SendRoster(this);
                    if (signaling is not null)
                    {
                        await signaling.AdvertiseAsync(
                            "lowlat (C#)", Capacity, AppProtocol.Guests(this), token);
                    }
                    break;
                case EventType.Ended:
                {
                    var (attempt, outcome, reason) = Events.Ended(ev);
                    Console.WriteLine($"ended {attempt}: {outcome} reason={reason}");
                    // **Reaped whatever the reason.** The guest's loop has
                    // stopped, but the attempt stays registered until the
                    // application says so -- and a registered attempt holds its
                    // guest number, its seat and its port for the life of the
                    // host. Leaving it is why a peer that has gone still shows
                    // in the roster and still counts against capacity.
                    EndConnection(attempt);
                    peers.Remove(attempt);
                    // Told after the reaping, so the roster describes the room
                    // as it is rather than as it was a moment ago.
                    AppProtocol.SendRoster(this);
                    if (signaling is not null)
                    {
                        await signaling.AdvertiseAsync(
                            "lowlat (C#)", Capacity, AppProtocol.Guests(this), token);
                    }
                    break;
                }
                case EventType.UserData:
                {
                    var (guest, id, length) = Events.UserData(ev);
                    var text = System.Text.Encoding.UTF8.GetString(
                        body, 0, (int)Math.Min(written, (uint)body.Length));
                    // **Handled, or said out loud.** A body meant for something
                    // this application does not speak is visible rather than
                    // silently swallowed.
                    if (!AppProtocol.OnMessage(this, guest, id, text))
                    {
                        Console.WriteLine($"guest {guest} said id={id} len={length}: {text}");
                    }
                    break;
                }
                case EventType.CaptureChanged:
                {
                    var (width, height, output) = Events.CaptureChanged(ev);
                    Console.WriteLine($"capturing {output} at {width}x{height}");
                    // **Nobody asked, and that is the point.** A reader asks
                    // after it acts, so a change it did not cause -- a display
                    // moving, another guest switching outputs -- reaches it
                    // only if the host says so.
                    var described = AppProtocol.Describe(this).ToJsonString();
                    foreach (var guest in Roster())
                    {
                        SendUserData(guest.Number, 11, described);
                    }
                    break;
                }
                case EventType.InputOwnerChanged:
                    Console.WriteLine($"the pointer is guest {Events.InputOwner(ev)}'s");
                    break;
                case EventType.Fatal:
                    Console.WriteLine($"fatal: reason={Events.Fatal(ev)}");
                    break;
            }
        }
    }
}

/// Reading the tagged union.
///
/// **The tag is first**, so an application that does not recognise a type can
/// skip it without knowing anything about the rest -- which is what makes adding
/// a type additive rather than a break.
internal static class Events
{
    public static string Attempt(Event ev) =>
        Text.Take(((ReadOnlySpan<byte>)ev.Payload)[..Sizes.Attempt]);

    public static (string, string, ushort, bool) Candidate(Event ev)
    {
        var body = (ReadOnlySpan<byte>)ev.Payload;
        var address = Text.Take(body.Slice(Sizes.Attempt, Sizes.Address));
        var port = BitConverter.ToUInt16(body.Slice(Sizes.Attempt + Sizes.Address, 2));
        var fromStun = body[Sizes.Attempt + Sizes.Address + 2] != 0;
        return (Attempt(ev), address, port, fromStun);
    }

    public static (string, Outcome, int) Ended(Event ev)
    {
        var body = (ReadOnlySpan<byte>)ev.Payload;
        var outcome = (Outcome)BitConverter.ToUInt32(body.Slice(Sizes.Attempt, 4));
        var reason = BitConverter.ToInt32(body.Slice(Sizes.Attempt + 4, 4));
        return (Attempt(ev), outcome, reason);
    }

    public static (uint, uint, uint) UserData(Event ev)
    {
        var body = (ReadOnlySpan<byte>)ev.Payload;
        return (
            BitConverter.ToUInt32(body[..4]),
            BitConverter.ToUInt32(body.Slice(4, 4)),
            BitConverter.ToUInt32(body.Slice(8, 4)));
    }

    public static (uint, uint, string) CaptureChanged(Event ev)
    {
        var body = (ReadOnlySpan<byte>)ev.Payload;
        return (
            BitConverter.ToUInt32(body[..4]),
            BitConverter.ToUInt32(body.Slice(4, 4)),
            Text.Take(body.Slice(8, Sizes.Output)));
    }

    public static uint InputOwner(Event ev) =>
        BitConverter.ToUInt32(((ReadOnlySpan<byte>)ev.Payload)[..4]);

    public static int Fatal(Event ev) =>
        BitConverter.ToInt32(((ReadOnlySpan<byte>)ev.Payload)[..4]);
}
