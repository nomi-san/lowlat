// Applications this host holds its guests to, on Windows alone
// (docs/impl-plan-windows.md W5.5).
//
// What one is -- a program to start, its executables, whether the guests' keys
// and pointer go to the input driver, where its first window goes -- is read
// from the file `LOWLAT_APPLICATIONS` names. It is started by the desktop's
// shell, so unelevated whatever this host runs as, and its first window is
// found by the executables its process runs, never by the process this host
// started: a launcher already running takes a start over and the process
// started exits.

#if WINDOWS
using System.Runtime.InteropServices;
using System.Text.Json.Nodes;

namespace LowlatHost;

/// One application the guests can be held to. `Place` is null to leave its
/// window where it opens, `center` for the middle of its own monitor, or an
/// output's identity from the enumeration for the middle of that output.
internal sealed record Application(
    string Name,
    string Program,
    string Arguments,
    string Directory,
    string[] Executables,
    bool VirtualInput,
    string? Place);

internal static class Applications
{
    /// The applications `LOWLAT_APPLICATIONS` names, a JSON file:
    /// `{"applications": [{"name", "program", "arguments", "directory",
    /// "executables": [...], "virtualInput", "place"}]}`. **One this host could
    /// not hold to is left out and said so**: a program or an executable that
    /// is not a full path, none listed, or more than a scope holds.
    public static Application[] Load()
    {
        var path = Environment.GetEnvironmentVariable("LOWLAT_APPLICATIONS");
        if (string.IsNullOrWhiteSpace(path))
        {
            return [];
        }
        JsonNode? root;
        try
        {
            root = JsonNode.Parse(File.ReadAllText(path));
        }
        catch (Exception error)
        {
            Console.WriteLine($"applications: {path} not read: {error.Message}");
            return [];
        }
        var found = new List<Application>();
        foreach (var entry in root?["applications"]?.AsArray() ?? [])
        {
            var name = entry?["name"]?.GetValue<string>() ?? "";
            var program = entry?["program"]?.GetValue<string>() ?? "";
            var executables = (entry?["executables"]?.AsArray() ?? [])
                .Select(each => each?.GetValue<string>() ?? "")
                .ToArray();
            if (name.Length == 0 || !Path.IsPathFullyQualified(program) || executables.Length == 0
                || executables.Length > Sizes.ScopePaths
                || executables.Any(each => !Path.IsPathFullyQualified(each)))
            {
                Console.WriteLine(
                    $"applications: '{name}' left out: a name, a program by its full path and 1 to "
                    + $"{Sizes.ScopePaths} executables by theirs are needed");
                continue;
            }
            var directory = entry?["directory"]?.GetValue<string>() ?? "";
            found.Add(new Application(
                name,
                program,
                entry?["arguments"]?.GetValue<string>() ?? "",
                directory.Length > 0 ? directory : Path.GetDirectoryName(program) ?? "",
                executables,
                entry?["virtualInput"]?.GetValue<bool>() ?? false,
                entry?["place"]?.GetValue<string>()));
        }
        return [.. found];
    }
}

/// Starting a program through the desktop's shell, as a double-click on it
/// would: **the shell's child, so unelevated** -- an application a held guest
/// reaches must not carry this host's administrator -- with the signed-in
/// user's own environment. The shell's documented interfaces, called through
/// their tables as every other call here crosses: its windows, the desktop's
/// view, the view's automation object, and that object's start.
internal static unsafe partial class Shell
{
    private const uint ClsctxLocalServer = 4;
    private const int SwcDesktop = 8;
    private const int SwfoNeedDispatch = 1;
    private const uint SvgioBackground = 0;
    private const ushort VtI4 = 3;
    private const ushort VtBstr = 8;
    private const int SwShowNormal = 1;

    private static readonly Guid ClsidShellWindows = new("9BA05972-F6A8-11CF-A442-00A0C90A8F39");
    private static readonly Guid IidShellWindows = new("85CB6900-4D95-11CF-960C-0080C7F4EE85");
    private static readonly Guid IidServiceProvider = new("6d5140c1-7436-11ce-8034-00aa006009fa");
    private static readonly Guid SidTopLevelBrowser = new("4C96BE40-915C-11CF-99D3-00AA004AE837");
    private static readonly Guid IidShellBrowser = new("000214E2-0000-0000-C000-000000000046");
    private static readonly Guid IidDispatch = new("00020400-0000-0000-C000-000000000046");
    private static readonly Guid IidShellFolderViewDual = new("E7A1AF80-4D96-11CF-960C-0080C7F4EE85");
    private static readonly Guid IidShellDispatch2 = new("A4C6892C-3BA9-11d2-9DEA-00C04FB16162");

    /// A VARIANT as the platform lays it out on 64 bits: the type, three
    /// reserved words, and a union two pointers wide.
    [StructLayout(LayoutKind.Sequential)]
    private struct Variant
    {
        public ushort Vt;
        private ushort reserved1, reserved2, reserved3;
        public long Value;
        private long record;
    }

    [LibraryImport("ole32.dll")]
    private static partial int CoCreateInstance(Guid* clsid, nint outer, uint context, Guid* iid, nint* instance);

    /// Start `program` with `arguments` in `directory`, or say why not. On a
    /// thread of its own in a single-threaded apartment, the shell's own kind.
    public static string? Launch(string program, string arguments, string directory)
    {
        string? failed = null;
        var thread = new Thread(() => failed = LaunchHere(program, arguments, directory));
        if (OperatingSystem.IsWindows())
        {
            thread.SetApartmentState(ApartmentState.STA);
        }
        thread.Start();
        thread.Join();
        return failed;
    }

    private static void* Slot(nint self, int index) => (*(void***)self)[index];

    private static int Query(nint self, Guid iid, nint* found) =>
        ((delegate* unmanaged<nint, Guid*, nint*, int>)Slot(self, 0))(self, &iid, found);

    private static void Release(nint self)
    {
        if (self != 0)
        {
            _ = ((delegate* unmanaged<nint, uint>)Slot(self, 2))(self);
        }
    }

    private static string? LaunchHere(string program, string arguments, string directory)
    {
        nint windows = 0, desktop = 0, provider = 0, browser = 0, view = 0;
        nint background = 0, folderView = 0, automation = 0, shell = 0;
        try
        {
            var clsid = ClsidShellWindows;
            var iid = IidShellWindows;
            var hr = CoCreateInstance(&clsid, 0, ClsctxLocalServer, &iid, &windows);
            if (hr < 0)
            {
                return $"no shell's windows, 0x{hr:x8}";
            }
            // The desktop's own window among the shell's (its location the
            // desktop, CSIDL 0), with its automation object.
            var location = new Variant { Vt = VtI4, Value = 0 };
            var root = default(Variant);
            var hwnd = 0;
            hr = ((delegate* unmanaged<nint, Variant*, Variant*, int, int*, int, nint*, int>)Slot(windows, 15))(
                windows, &location, &root, SwcDesktop, &hwnd, SwfoNeedDispatch, &desktop);
            if (hr != 0 || desktop == 0)
            {
                return $"no desktop window, 0x{hr:x8}";
            }
            if ((hr = Query(desktop, IidServiceProvider, &provider)) < 0)
            {
                return $"the desktop serves nothing, 0x{hr:x8}";
            }
            var service = SidTopLevelBrowser;
            var browserIid = IidShellBrowser;
            hr = ((delegate* unmanaged<nint, Guid*, Guid*, nint*, int>)Slot(provider, 3))(
                provider, &service, &browserIid, &browser);
            if (hr < 0)
            {
                return $"no desktop browser, 0x{hr:x8}";
            }
            hr = ((delegate* unmanaged<nint, nint*, int>)Slot(browser, 15))(browser, &view);
            if (hr < 0 || view == 0)
            {
                return $"no desktop view, 0x{hr:x8}";
            }
            var dispatchIid = IidDispatch;
            hr = ((delegate* unmanaged<nint, uint, Guid*, nint*, int>)Slot(view, 15))(
                view, SvgioBackground, &dispatchIid, &background);
            if (hr < 0)
            {
                return $"no view's background, 0x{hr:x8}";
            }
            if ((hr = Query(background, IidShellFolderViewDual, &folderView)) < 0)
            {
                return $"no folder view, 0x{hr:x8}";
            }
            hr = ((delegate* unmanaged<nint, nint*, int>)Slot(folderView, 7))(folderView, &automation);
            if (hr < 0 || automation == 0)
            {
                return $"no shell automation, 0x{hr:x8}";
            }
            if ((hr = Query(automation, IidShellDispatch2, &shell)) < 0)
            {
                return $"no shell start, 0x{hr:x8}";
            }
            var file = Marshal.StringToBSTR(program);
            var args = Marshal.StringToBSTR(arguments);
            var dir = Marshal.StringToBSTR(directory);
            var verb = Marshal.StringToBSTR("open");
            try
            {
                hr = ((delegate* unmanaged<nint, nint, Variant, Variant, Variant, Variant, int>)Slot(shell, 31))(
                    shell,
                    file,
                    new Variant { Vt = VtBstr, Value = args },
                    new Variant { Vt = VtBstr, Value = dir },
                    new Variant { Vt = VtBstr, Value = verb },
                    new Variant { Vt = VtI4, Value = SwShowNormal });
            }
            finally
            {
                Marshal.FreeBSTR(file);
                Marshal.FreeBSTR(args);
                Marshal.FreeBSTR(dir);
                Marshal.FreeBSTR(verb);
            }
            return hr < 0 ? $"the shell would not start it, 0x{hr:x8}" : null;
        }
        finally
        {
            Release(shell);
            Release(automation);
            Release(folderView);
            Release(background);
            Release(view);
            Release(browser);
            Release(provider);
            Release(desktop);
            Release(windows);
        }
    }
}

/// An application's windows, as this host finds and places them.
internal static unsafe partial class AppWindows
{
    private const uint GwOwner = 4;
    private const int GwlExStyle = -20;
    private const long WsExToolWindow = 0x80;
    private const uint ProcessQueryLimited = 0x1000;
    private const uint TokenQuery = 0x8;
    private const int TokenElevation = 20;
    private const uint DwmwaCloaked = 14;
    private const uint MonitorNearest = 2;
    private const uint SwpNoSize = 0x1, SwpNoZOrder = 0x4, SwpNoActivate = 0x10;
    private static readonly nint PerMonitorAwareV2 = -4;

    /// How long a launcher may take to show the application's first window.
    private static readonly TimeSpan Patience = TimeSpan.FromSeconds(60);

    [StructLayout(LayoutKind.Sequential)]
    private struct Rect
    {
        public int Left, Top, Right, Bottom;
    }

    /// A monitor's bounds, working area and device name, `\\.\DISPLAYn`: the
    /// name an output's enumeration gives as its own.
    [StructLayout(LayoutKind.Sequential)]
    private struct MonitorInfo
    {
        public uint Size;
        public Rect Monitor;
        public Rect Work;
        public uint Flags;
        public fixed char Device[32];
    }

    [LibraryImport("user32.dll")]
    private static partial int EnumWindows(delegate* unmanaged<nint, nint, int> each, nint data);

    [LibraryImport("user32.dll")]
    private static partial int EnumDisplayMonitors(
        nint dc, Rect* clip, delegate* unmanaged<nint, nint, Rect*, nint, int> each, nint data);

    [LibraryImport("user32.dll")]
    private static partial int IsWindowVisible(nint window);

    [LibraryImport("user32.dll")]
    private static partial nint GetWindow(nint window, uint relation);

    [LibraryImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    private static partial nint GetWindowLongPtr(nint window, int index);

    [LibraryImport("user32.dll")]
    private static partial uint GetWindowThreadProcessId(nint window, uint* pid);

    [LibraryImport("user32.dll")]
    private static partial int GetWindowRect(nint window, Rect* rect);

    [LibraryImport("user32.dll")]
    private static partial nint MonitorFromWindow(nint window, uint flags);

    [LibraryImport("user32.dll", EntryPoint = "GetMonitorInfoW")]
    private static partial int GetMonitorInfo(nint monitor, MonitorInfo* info);

    [LibraryImport("user32.dll")]
    private static partial int SetWindowPos(nint window, nint after, int x, int y, int cx, int cy, uint flags);

    [LibraryImport("user32.dll")]
    private static partial nint SetThreadDpiAwarenessContext(nint context);

    [LibraryImport("dwmapi.dll")]
    private static partial int DwmGetWindowAttribute(nint window, uint attribute, void* value, uint size);

    [LibraryImport("kernel32.dll")]
    private static partial nint OpenProcess(uint access, int inherit, uint pid);

    [LibraryImport("kernel32.dll", EntryPoint = "QueryFullProcessImageNameW")]
    private static partial int QueryFullProcessImageName(nint process, uint flags, char* name, uint* size);

    [LibraryImport("kernel32.dll")]
    private static partial int CloseHandle(nint handle);

    [LibraryImport("advapi32.dll")]
    private static partial int OpenProcessToken(nint process, uint access, nint* token);

    [LibraryImport("advapi32.dll")]
    private static partial int GetTokenInformation(nint token, int kind, void* value, uint size, uint* written);

    [UnmanagedCallersOnly]
    private static int Each(nint window, nint data)
    {
        ((List<nint>)GCHandle.FromIntPtr(data).Target!).Add(window);
        return 1;
    }

    [UnmanagedCallersOnly]
    private static int EachMonitor(nint monitor, nint dc, Rect* bounds, nint data)
    {
        ((List<nint>)GCHandle.FromIntPtr(data).Target!).Add(monitor);
        return 1;
    }

    /// The monitor the system names `\\.\` and `name`, or nothing.
    private static nint Named(string name)
    {
        var all = new List<nint>();
        var handle = GCHandle.Alloc(all);
        try
        {
            _ = EnumDisplayMonitors(0, null, &EachMonitor, GCHandle.ToIntPtr(handle));
        }
        finally
        {
            handle.Free();
        }
        foreach (var monitor in all)
        {
            var info = new MonitorInfo { Size = (uint)sizeof(MonitorInfo) };
            if (GetMonitorInfo(monitor, &info) != 0
                && string.Equals(new string(info.Device), @"\\.\" + name, StringComparison.OrdinalIgnoreCase))
            {
                return monitor;
            }
        }
        return 0;
    }

    /// Wait on a thread of its own for the application's first window -- shown,
    /// on the screen, a window of its own (no owner, not a tool window), its
    /// process running one of the executables -- and place it where asked,
    /// saying where it was, where it went, and whether its process runs
    /// elevated, which it must not.
    public static Thread Follow(Application app, IReadOnlyList<(string id, string name)> outputs)
    {
        var thread = new Thread(() => FollowHere(app, outputs)) { IsBackground = true };
        thread.Start();
        return thread;
    }

    private static void FollowHere(Application app, IReadOnlyList<(string id, string name)> outputs)
    {
        // Every position in the displays' own pixels, whatever their scale.
        _ = SetThreadDpiAwarenessContext(PerMonitorAwareV2);
        var deadline = DateTime.UtcNow + Patience;
        while (DateTime.UtcNow < deadline)
        {
            if (First(app) is { } found)
            {
                var (window, pid) = found;
                Rect at;
                _ = GetWindowRect(window, &at);
                Console.WriteLine(
                    $"applications: {app.Name}: its window 0x{window:x}, process {pid} elevated={Elevated(pid)}, "
                    + $"at {at.Left},{at.Top} {at.Right - at.Left}x{at.Bottom - at.Top}");
                Place(app, window, at, outputs);
                return;
            }
            Thread.Sleep(100);
        }
        Console.WriteLine($"applications: {app.Name}: no window of it within {Patience.TotalSeconds:0} s");
    }

    private static (nint, uint)? First(Application app)
    {
        var all = new List<nint>();
        var handle = GCHandle.Alloc(all);
        try
        {
            _ = EnumWindows(&Each, GCHandle.ToIntPtr(handle));
        }
        finally
        {
            handle.Free();
        }
        foreach (var window in all)
        {
            if (IsWindowVisible(window) == 0 || GetWindow(window, GwOwner) != 0
                || ((long)GetWindowLongPtr(window, GwlExStyle) & WsExToolWindow) != 0)
            {
                continue;
            }
            var cloaked = 0;
            if (DwmGetWindowAttribute(window, DwmwaCloaked, &cloaked, 4) >= 0 && cloaked != 0)
            {
                continue;
            }
            uint pid = 0;
            _ = GetWindowThreadProcessId(window, &pid);
            if (Image(pid) is { } image && app.Executables.Any(each => Same(each, image)))
            {
                return (window, pid);
            }
        }
        return null;
    }

    /// Two paths compared as the file system compares names: case aside, and
    /// either separator.
    private static bool Same(string a, string b) =>
        string.Equals(a.Replace('/', '\\'), b.Replace('/', '\\'), StringComparison.OrdinalIgnoreCase);

    private static string? Image(uint pid)
    {
        var process = OpenProcess(ProcessQueryLimited, 0, pid);
        if (process == 0)
        {
            return null;
        }
        try
        {
            var name = stackalloc char[32768];
            uint size = 32768;
            return QueryFullProcessImageName(process, 0, name, &size) != 0 ? new string(name, 0, (int)size) : null;
        }
        finally
        {
            _ = CloseHandle(process);
        }
    }

    /// 1 elevated, 0 not, -1 where its token cannot be read.
    private static int Elevated(uint pid)
    {
        var process = OpenProcess(ProcessQueryLimited, 0, pid);
        if (process == 0)
        {
            return -1;
        }
        nint token = 0;
        try
        {
            if (OpenProcessToken(process, TokenQuery, &token) == 0)
            {
                return -1;
            }
            int elevated = 0;
            uint written = 0;
            return GetTokenInformation(token, TokenElevation, &elevated, 4, &written) != 0 ? elevated : -1;
        }
        finally
        {
            if (token != 0)
            {
                _ = CloseHandle(token);
            }
            _ = CloseHandle(process);
        }
    }

    /// The window moved to the middle of its own monitor's working area, or of
    /// the output named, never past its top-left corner: a nudge as it opens,
    /// which the application may move again.
    private static void Place(Application app, nint window, Rect at, IReadOnlyList<(string id, string name)> outputs)
    {
        if (app.Place is null)
        {
            Console.WriteLine($"applications: {app.Name}: left where it opened");
            return;
        }
        // **The output's own monitor, asked of the system by its name**: the
        // enumeration's positions are the space absolute input is expressed
        // against, measured from the desktop's corner, not the screen's
        // coordinates a window is placed in.
        var monitor = app.Place == "center"
            ? MonitorFromWindow(window, MonitorNearest)
            : outputs.FirstOrDefault(each => each.id == app.Place) is { id: not null } output
                ? Named(output.name)
                : 0;
        var info = new MonitorInfo { Size = (uint)sizeof(MonitorInfo) };
        if (monitor == 0 || GetMonitorInfo(monitor, &info) == 0)
        {
            Console.WriteLine($"applications: {app.Name}: no monitor for '{app.Place}', left where it opened");
            return;
        }
        var area = info.Work;
        var width = at.Right - at.Left;
        var height = at.Bottom - at.Top;
        var x = Math.Max(area.Left, area.Left + (area.Right - area.Left - width) / 2);
        var y = Math.Max(area.Top, area.Top + (area.Bottom - area.Top - height) / 2);
        var placed = SetWindowPos(window, 0, x, y, 0, 0, SwpNoSize | SwpNoZOrder | SwpNoActivate) != 0;
        Rect now;
        _ = GetWindowRect(window, &now);
        Console.WriteLine(
            $"applications: {app.Name}: {(placed ? "placed" : "not placed")} at {now.Left},{now.Top} ({app.Place})");
    }
}
#endif
