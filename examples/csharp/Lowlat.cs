// The shared library, as C# sees it.
//
// Every declaration here is written from include/lowlat.h and nothing else.
// The header is not consumed by this project -- C# cannot read one -- so the
// layouts are mirrored by hand, and the `size` field on each structure is what
// catches a mirror that has drifted: the library refuses one that says less
// than it expects.
//
// **No marshalling directives anywhere.** Every struct below is blittable:
// fixed-size fields, inline arrays, no pointers into managed memory. That is
// what lets `LibraryImport` generate the interop at compile time instead of a
// runtime marshaller walking each field.

using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace LowlatHost;

internal enum Status
{
    Ok = 0,
    Timeout = 1,
    ErrInternal = -1,
    ErrInvalidArgument = -2,
    ErrTooSmall = -3,
    ErrPoisoned = -4,
    ErrAlreadyStarted = -5,
    ErrNotStarted = -6,
    ErrAtCapacity = -100,
    ErrUnknownAttempt = -101,
    ErrAlreadyBegun = -102,
    ErrWithdrawn = -103,
    ErrIo = -104,
    ErrCrypto = -105,
    ErrUnknownGuest = -106,
    ErrNoDisplay = -200,
    ErrDisplayUnreachable = -201,
}

internal enum EventType : uint
{
    Candidate = 1,
    Ready = 2,
    Established = 3,
    Ended = 4,
    UserData = 5,
    CaptureChanged = 6,
    InputOwnerChanged = 7,
    Fatal = 8,
}

internal enum Outcome : uint
{
    ConnectivityFailed = 1,
    PeerGone = 2,
    Undeliverable = 3,
    PeerLeft = 4,
    NeverDeclared = 5,
    TransportFailed = 6,
    ControlStalled = 7,
    Kicked = 8,
}

internal enum Codec : uint { H264 = 1, Hevc = 2 }

internal enum Encoder : uint { FollowDisplay = 0, Open = 1, Vendor = 2 }

internal enum CgLevel : uint { Legacy = 0, Sensitive = 1, Relaxed = 2 }

internal enum LogLevel : uint { Error = 0, Warn = 1, Info = 2, Debug = 3, Trace = 4 }

internal static class Sizes
{
    public const int Attempt = 128;
    public const int Address = 46;
    public const int Output = 64;
    public const int Ice = 256;
    public const int Fingerprint = 112;
    public const int Servers = 4;
    public const int Server = 64;
}

/// **Plain bytes, not `bool`.** A C# `bool` is a four-byte Win32 BOOL to the
/// marshaller, and pinning it to one byte takes a `MarshalAs` -- which is
/// exactly the directive that stops a struct being blittable. A byte with a
/// property over it costs nothing and keeps every struct here plain.
[StructLayout(LayoutKind.Sequential)]
internal struct Permissions
{
    public byte KeyboardByte;
    public byte PointerByte;
    public byte GamepadByte;
    public byte Reserved;

    public bool Keyboard
    {
        get => KeyboardByte != 0;
        set => KeyboardByte = value ? (byte)1 : (byte)0;
    }

    public bool Pointer
    {
        get => PointerByte != 0;
        set => PointerByte = value ? (byte)1 : (byte)0;
    }

    public bool Gamepad
    {
        get => GamepadByte != 0;
        set => GamepadByte = value ? (byte)1 : (byte)0;
    }
}

[StructLayout(LayoutKind.Sequential)]
internal struct CreateInfo
{
    public uint Size;
}

[StructLayout(LayoutKind.Sequential)]
internal struct HostVideoConfig
{
    public uint Size;
    public uint Fps;
    public double BitrateMbps;
    public double MinBitrateMbps;
    public byte FullFpsByte;
    private byte reserved0, reserved1, reserved2;

    public bool FullFps
    {
        get => FullFpsByte != 0;
        set => FullFpsByte = value ? (byte)1 : (byte)0;
    }
    [InlineArray(Sizes.Output)] public struct OutputName { private byte first; }
    public OutputName Output;
}

[StructLayout(LayoutKind.Sequential)]
internal struct HostConfig
{
    public uint Size;
    public ushort BasePort;
    public ushort Reserved;
    public uint MaxGuests;
    public uint Codec;
    public uint Encoder;
    public uint CgLevel;
    public uint ExclusiveHoldMs;
    public byte ExclusivePointerByte;
    private byte reserved0, reserved1, reserved2;

    public bool ExclusivePointer
    {
        get => ExclusivePointerByte != 0;
        set => ExclusivePointerByte = value ? (byte)1 : (byte)0;
    }
    public uint ServerCount;
    [InlineArray(Sizes.Servers * Sizes.Server)] public struct ServerList { private byte first; }
    public ServerList Servers;
    public HostVideoConfig Video;
}

[StructLayout(LayoutKind.Sequential)]
internal struct HostStatus
{
    public uint Size;
    public uint Guests;
    public uint Width;
    public uint Height;
    public byte RunningByte;
    private byte reserved0, reserved1, reserved2;

    public bool Running => RunningByte != 0;
}

[StructLayout(LayoutKind.Sequential)]
internal struct Guest
{
    public uint Number;
    public Permissions Permissions;
    public byte OwnerByte;
    private byte reserved0, reserved1, reserved2;

    public bool Owner => OwnerByte != 0;
}

[StructLayout(LayoutKind.Sequential)]
internal struct Output
{
    [InlineArray(Sizes.Output)] public struct Name { private byte first; }
    public Name Id;
    public Name Connector;
    public uint Width;
    public uint Height;
    public uint X;
    public uint Y;
}

[StructLayout(LayoutKind.Sequential)]
internal struct AttemptInfo
{
    public uint Size;
    public uint Reserved;
    [InlineArray(Sizes.Attempt)] public struct AttemptId { private byte first; }
    [InlineArray(Sizes.Ice)] public struct IceField { private byte first; }
    public AttemptId Id;
    public IceField Ufrag;
    public IceField Pwd;
    public IceField Aes256;
    public Permissions Permissions;
    public byte OwnerByte;
    private byte reserved0, reserved1, reserved2;

    public bool Owner
    {
        get => OwnerByte != 0;
        set => OwnerByte = value ? (byte)1 : (byte)0;
    }
}

[StructLayout(LayoutKind.Sequential)]
internal struct Candidate
{
    public uint Size;
    public ushort Port;
    public byte SyncByte;
    public byte Reserved;

    public bool Sync
    {
        get => SyncByte != 0;
        set => SyncByte = value ? (byte)1 : (byte)0;
    }
    [InlineArray(Sizes.Address)] public struct AddressField { private byte first; }
    public AddressField Address;
}

[StructLayout(LayoutKind.Sequential)]
internal struct Credentials
{
    public uint Size;
    public ushort Port;
    public ushort Reserved;
    [InlineArray(Sizes.Ice)] public struct IceField { private byte first; }
    [InlineArray(Sizes.Fingerprint)] public struct FingerprintField { private byte first; }
    public IceField Ufrag;
    public IceField Pwd;
    public FingerprintField Fingerprint;
    public IceField Aes256;
}

// The tagged union. Laid out by hand as a fixed block, because C# unions of
// structs containing inline arrays are more trouble than reading the bytes.
[StructLayout(LayoutKind.Sequential)]
internal struct Event
{
    public uint Kind;
    public uint Dropped;
    [InlineArray(192)] public struct Body { private byte first; }
    public Body Payload;
}

internal static partial class Native
{
    private const string Library = "lowlat";

    [LibraryImport(Library)]
    internal static partial uint lowlat_abi_version();

    [LibraryImport(Library)]
    internal static partial IntPtr lowlat_status_string(int status);

    [LibraryImport(Library)]
    internal static partial Status lowlat_can_host();

    [LibraryImport(Library)]
    internal static partial Status lowlat_set_log_callback(IntPtr fn, IntPtr opaque);

    [LibraryImport(Library)]
    internal static partial Status lowlat_set_log_level(uint level);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_get_outputs(Output* outputs, uint* count);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_create(CreateInfo* info, IntPtr* handle);

    [LibraryImport(Library)]
    internal static partial void lowlat_destroy(IntPtr handle);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_start(IntPtr handle, HostConfig* cfg);

    [LibraryImport(Library)]
    internal static partial Status lowlat_host_stop(IntPtr handle);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_get_status(IntPtr handle, HostStatus* status);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_poll_events(
        IntPtr handle, uint timeoutMs, Event* ev, byte* body, uint* bodyLen);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_new_attempt(IntPtr handle, AttemptInfo* info);

    [LibraryImport(Library)]
    internal static unsafe partial void lowlat_host_add_candidate(
        IntPtr handle, byte* attemptId, Candidate* candidate);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_begin_p2p(
        IntPtr handle, byte* attemptId, Credentials* ours);

    [LibraryImport(Library)]
    internal static unsafe partial void lowlat_host_end_connection(IntPtr handle, byte* attemptId);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_get_guests(
        IntPtr handle, Guest* guests, uint* count);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_send_user_data(
        IntPtr handle, uint guestId, uint id, byte* data, uint len);

    [LibraryImport(Library)]
    internal static unsafe partial Status lowlat_host_set_permissions(
        IntPtr handle, uint guestId, Permissions* perms);

    [LibraryImport(Library)]
    internal static partial Status lowlat_host_kick_guest(IntPtr handle, uint guestId, int reason);
}

internal static class Text
{
    /// Copy a string into one of the library's fixed arrays, terminated.
    public static void Put(Span<byte> into, string value)
    {
        into.Clear();
        var bytes = System.Text.Encoding.UTF8.GetBytes(value);
        var room = Math.Min(bytes.Length, into.Length - 1);
        bytes.AsSpan(0, room).CopyTo(into);
    }

    /// Read one back, stopping at the terminator.
    public static string Take(ReadOnlySpan<byte> from)
    {
        var end = from.IndexOf((byte)0);
        return System.Text.Encoding.UTF8.GetString(end < 0 ? from : from[..end]);
    }

    public static string Describe(Status status)
    {
        var text = Native.lowlat_status_string((int)status);
        return text == IntPtr.Zero ? $"{(int)status}" : Marshal.PtrToStringUTF8(text) ?? "";
    }
}
