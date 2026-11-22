//! The application a host holds its guests to, judged on Windows where their
//! input would land (docs/05-host.md section 7.4): the window in front, and
//! the window under the pointer, each by the executable the process of its
//! root window runs.
//!
//! **Asked as input arrives rather than kept from the system's notice of a new
//! foreground.** The notice is posted, so it trails the change it reports, and
//! the question costs a third of a microsecond for the window in front and two
//! for the window under the pointer (measured 2026-11-20, the system account
//! included); what is kept is each process's answer.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, ClipCursor, EnumWindows, GA_ROOT, GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE,
    GetAncestor, GetClipCursor, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, SetForegroundWindow, ShowWindow,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WindowFromPoint,
};

use crate::event::{Bounds, Gate};
use crate::windows::Desktop;

/// The longest path the system names an image by, in UTF-16 units.
const PATH_MAX: usize = 32_768;

/// The processes whose answers are kept at once. A guest asks about the few
/// windows it meets; more than this is the list turning over.
const KNOWN_MAX: usize = 64;

/// How often a thread off the input desktop is moved to it for an answer: the
/// question is asked per input, and while the lock screen or an elevation
/// prompt has the desktop no answer is open anyway.
const REATTACH_MS: f64 = 250.0;

/// An application, as the executables its processes run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Each full path in UTF-16, either separator made the system's.
    paths: Vec<Vec<u16>>,
}

impl Scope {
    /// An application of these executables, each named by its full path as
    /// the system names the image a process runs. Letters compare without
    /// regard to case, as the file system's do, and either separator is
    /// taken.
    pub fn new<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            paths: paths
                .into_iter()
                .map(|path| normal(path.as_ref().encode_wide()))
                .collect(),
        }
    }

    /// The paths, as given.
    pub fn paths(&self) -> impl Iterator<Item = String> + '_ {
        self.paths.iter().map(|path| String::from_utf16_lossy(path))
    }

    /// Whether a process running this image is the application's.
    #[must_use]
    pub fn holds(&self, image: &[u16]) -> bool {
        let image = normal(image.iter().copied());
        self.paths.iter().any(|path| same_path(path, &image))
    }
}

fn normal(units: impl Iterator<Item = u16>) -> Vec<u16> {
    units
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .collect()
}

/// Two paths compared as the file system compares names: by the system's own
/// upper-casing, not by any locale's.
fn same_path(a: &[u16], b: &[u16]) -> bool {
    let (Ok(a_len), Ok(b_len)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    // SAFETY: two slices with their own lengths; the call reads them and
    // nothing else.
    unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) == CSTR_EQUAL }
}

/// A process this thread asked about, and its answer.
///
/// **Its handle is held for as long as the answer is kept**: a process number
/// passes to another process only once nothing holds the last one's, so the
/// answer cannot outlive the process it was given for.
#[derive(Debug)]
struct Known {
    pid: u32,
    process: Process,
    holds: bool,
}

#[derive(Debug)]
struct Process(HANDLE);

impl Process {
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: rights the system defines, not inherited; checked.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// The full path of the image it runs.
    fn image(&self) -> Option<Vec<u16>> {
        let mut path = vec![0u16; PATH_MAX];
        let mut len = u32::try_from(path.len()).ok()?;
        // SAFETY: a buffer of `len` units and its length; the call writes no
        // more and sets `len` to what it wrote.
        if unsafe {
            QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
        } == 0
        {
            return None;
        }
        path.truncate(usize::try_from(len).ok()?);
        Some(path)
    }

    fn exited(&self) -> bool {
        // SAFETY: a process handle this owns, not waited on.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle `open` gave, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// What one guest's thread asks of the application before its input goes in,
/// and what the picture asks of a window it might show: **one answer for
/// both**, so a window a guest's input reaches and a window the guest sees are
/// the application's by the same rule.
#[derive(Debug)]
pub struct Judge {
    scope: Arc<Scope>,
    known: Vec<Known>,
    /// When this thread was last moved to the input desktop for an answer.
    moved: Option<lowlat_common::clock::Time>,
}

impl Judge {
    pub fn new(scope: Arc<Scope>) -> Self {
        Self {
            scope,
            known: Vec::new(),
            moved: None,
        }
    }

    /// Whether the application is where input that needs `at` lands now.
    /// Closed where nothing can be said: no window in front, a pointer that
    /// cannot be read, a process that cannot be named.
    pub(crate) fn gate(&mut self, at: Gate, desktop: &mut Option<Desktop>) -> bool {
        let window = match at {
            Gate::Front => self.in_front(desktop),
            Gate::Pointer => self.under_pointer(desktop),
            Gate::At { x, y } => self.at_point(x, y, desktop),
        };
        window.is_some_and(|window| self.holds_window(window))
    }

    /// The application's window the picture shows, while one is in front:
    /// **the root owner of the window in front**, as the picture follows it --
    /// a dialog of the application's comes in its owner's picture -- or the
    /// window in front itself where only that is the application's.
    pub(crate) fn shown(&mut self) -> Option<HWND> {
        // SAFETY: no arguments; a window or null.
        let front = unsafe { GetForegroundWindow() };
        if front.is_null() {
            return None;
        }
        // SAFETY: a window handle; an ancestor of it, or null for one gone.
        let owner = unsafe { GetAncestor(front, GA_ROOTOWNER) };
        if !owner.is_null() && self.holds_window(owner) {
            return Some(owner);
        }
        // SAFETY: as above.
        let root = unsafe { GetAncestor(front, GA_ROOT) };
        (!root.is_null() && self.holds_window(root)).then_some(root)
    }

    /// The window at a point of the desktop, in its own pixels measured from
    /// its corner, refused off the input desktop as the pointer is.
    fn at_point(&mut self, x: i32, y: i32, desktop: &mut Option<Desktop>) -> Option<HWND> {
        let point = screen_point(x, y);
        // SAFETY: a point by value; a window or null.
        let mut window = unsafe { WindowFromPoint(point) };
        if window.is_null() && self.move_to_input(desktop) {
            // SAFETY: as above.
            window = unsafe { WindowFromPoint(point) };
        }
        (!window.is_null()).then_some(window)
    }

    /// The window in front. **None from a thread off the input desktop**,
    /// which a guest's thread is after the lock screen or an elevation prompt
    /// had it, so the thread is moved to the desktop as it is now and asked
    /// again.
    fn in_front(&mut self, desktop: &mut Option<Desktop>) -> Option<HWND> {
        // SAFETY: no arguments; a window or null.
        let mut window = unsafe { GetForegroundWindow() };
        if window.is_null() && self.move_to_input(desktop) {
            // SAFETY: as above.
            window = unsafe { GetForegroundWindow() };
        }
        (!window.is_null()).then_some(window)
    }

    /// The window under the pointer, the pointer refused off the input
    /// desktop as the window in front is.
    fn under_pointer(&mut self, desktop: &mut Option<Desktop>) -> Option<HWND> {
        let mut at = POINT { x: 0, y: 0 };
        // SAFETY: one point, written by the call.
        let mut read = unsafe { GetCursorPos(&mut at) } != 0;
        if !read && self.move_to_input(desktop) {
            // SAFETY: as above.
            read = unsafe { GetCursorPos(&mut at) } != 0;
        }
        if !read {
            return None;
        }
        // SAFETY: a point by value; a window or null.
        let window = unsafe { WindowFromPoint(at) };
        (!window.is_null()).then_some(window)
    }

    fn move_to_input(&mut self, desktop: &mut Option<Desktop>) -> bool {
        if self
            .moved
            .is_some_and(|moved| lowlat_common::clock::elapsed_ms(moved) < REATTACH_MS)
        {
            return false;
        }
        self.moved = Some(lowlat_common::clock::Time::now());
        match Desktop::attach() {
            Some(attached) => {
                // The desktop it was on is closed as this one replaces it.
                *desktop = Some(attached);
                true
            }
            None => false,
        }
    }

    /// Whether a window is the application's: **its root's**, so a child a
    /// helper process draws into belongs to the window it sits in, and a
    /// popup of the application's own -- a menu, a tooltip, a dialog -- is its
    /// own root, found by its process.
    #[allow(
        clippy::not_unsafe_ptr_arg_deref,
        reason = "a window handle is a value the system checks at each call, never memory read here"
    )]
    pub fn holds_window(&mut self, window: HWND) -> bool {
        // SAFETY: a window handle; the root, or null for a window gone.
        let root = unsafe { GetAncestor(window, GA_ROOT) };
        let mut pid = 0u32;
        // SAFETY: a window handle and one number written by the call.
        unsafe { GetWindowThreadProcessId(if root.is_null() { window } else { root }, &mut pid) };
        pid != 0 && self.holds_process(pid)
    }

    fn holds_process(&mut self, pid: u32) -> bool {
        if let Some(known) = self.known.iter().find(|known| known.pid == pid) {
            return known.holds;
        }
        self.known.retain(|known| !known.process.exited());
        // **A process that cannot be opened is not the application's, and is
        // asked about again next time**: its answer, held by no handle, could
        // outlive it.
        match Process::open(pid) {
            Some(process) => self.judge_opened(pid, process),
            None => false,
        }
    }

    /// The answer for a process just opened, kept under its handle. **One that
    /// opened but cannot be named is not the application's either, and is
    /// asked about again** rather than kept: a handle given fewer rights than
    /// were asked for -- by a process another program protects -- names
    /// nothing, and an answer kept for it would hold the guest away from the
    /// application for as long as the application runs.
    fn judge_opened(&mut self, pid: u32, process: Process) -> bool {
        let Some(image) = process.image() else {
            return false;
        };
        let holds = self.scope.holds(&image);
        if self.known.len() >= KNOWN_MAX {
            self.known.remove(0);
        }
        self.known.push(Known {
            pid,
            process,
            holds,
        });
        holds
    }
}

/// The desktop's corner on the screen, as a per-monitor aware thread reads
/// it: what a captured output's place is measured from.
fn desktop_corner() -> (i32, i32) {
    // SAFETY: reads of the system's metrics.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

/// A point of the desktop, measured from its corner, on the screen.
fn screen_point(x: i32, y: i32) -> POINT {
    screen_point_from(x, y, desktop_corner())
}

fn screen_point_from(x: i32, y: i32, (left, top): (i32, i32)) -> POINT {
    POINT {
        x: x.saturating_add(left),
        y: y.saturating_add(top),
    }
}

/// A rectangle of the screen as the desktop's own pixels, measured from its
/// corner as a captured output's place is.
pub(crate) fn on_desktop(r: RECT) -> Bounds {
    on_desktop_from(r, desktop_corner())
}

fn on_desktop_from(r: RECT, (left, top): (i32, i32)) -> Bounds {
    Bounds {
        left: r.left.saturating_sub(left),
        top: r.top.saturating_sub(top),
        right: r.right.saturating_sub(left),
        bottom: r.bottom.saturating_sub(top),
    }
}

/// Where a window's picture lies on its display, in the screen's pixels:
/// **its frame's bounds** -- the part a capture of it holds, which the picture
/// is placed by -- **cut to the monitor it is most on**, as that monitor cuts
/// it. Nothing for a window gone or off its monitor.
pub(crate) fn on_its_display(window: HWND) -> Option<RECT> {
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: a window handle and an output of the attribute's size.
    let read = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut frame).cast(),
            u32::try_from(size_of::<RECT>()).ok()?,
        )
    };
    if read < 0 {
        return None;
    }
    // SAFETY: a window handle; a monitor, the nearest whatever it is.
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    // SAFETY: plain data the call fills, its size set first.
    let mut info: MONITORINFO = unsafe { core::mem::zeroed() };
    info.cbSize = u32::try_from(size_of::<MONITORINFO>()).ok()?;
    // SAFETY: a monitor handle and an output of the size it says.
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return None;
    }
    overlap(frame, info.rcMonitor)
}

/// Where two rectangles meet: nothing where they do not.
fn overlap(a: RECT, b: RECT) -> Option<RECT> {
    let r = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (r.left < r.right && r.top < r.bottom).then_some(r)
}

fn corners(r: RECT) -> (i32, i32, i32, i32) {
    (r.left, r.top, r.right, r.bottom)
}

fn inside(inner: RECT, outer: RECT) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

/// Whether the pointer needs clipping to `window`, the clip in force `now`
/// and the one this set `ours`: **not where the clip already is that window,
/// nor where it is an application's own inside it** -- a game holding the
/// pointer to a part of its window. One of ours inside it is a window since
/// grown, and clipped again.
fn clips(now: RECT, window: RECT, ours: Option<(i32, i32, i32, i32)>) -> bool {
    if corners(now) == corners(window) {
        return false;
    }
    let theirs = ours != Some(corners(now));
    !(theirs && inside(now, window))
}

/// The system's clip of the pointer, as one guest's thread set it: the
/// pointer kept on the application's window while it moves by deltas
/// (docs/05-host.md section 7.4).
///
/// **Let go of by whoever set it**: the system keeps a clip as another window
/// takes the front by itself and as the window it was set for moves
/// (measured 2026-11-21), and the desk's own mouse is held by it too. **Never
/// one an application set**: let go of only while it is still this one.
#[derive(Debug, Default)]
pub(crate) struct Clip {
    set: Option<(i32, i32, i32, i32)>,
}

impl Clip {
    /// The pointer kept inside `window`, a rectangle of the screen.
    pub(crate) fn hold(&mut self, window: RECT) {
        if !clips(clip_now(), window, self.set) {
            return;
        }
        // SAFETY: a rectangle the call reads.
        if unsafe { ClipCursor(&raw const window) } != 0 {
            self.set = Some(corners(window));
        }
    }

    /// The clip let go of, where it is still the one this set.
    pub(crate) fn release(&mut self) {
        let Some(set) = self.set.take() else {
            return;
        };
        if corners(clip_now()) == set {
            // SAFETY: no rectangle: the whole screen again.
            unsafe { ClipCursor(core::ptr::null()) };
        }
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        self.release();
    }
}

fn clip_now() -> RECT {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: one rectangle written.
    unsafe { GetClipCursor(&raw mut r) };
    r
}

/// The most of an application's windows one run of Tabs steps through: a
/// launcher, its client and a game are three.
const ORDER_MAX: usize = 16;

/// The application's windows a held guest's Tabs under one Alt step through
/// (docs/05-host.md section 7.4): **those the system's switcher lists**, in
/// the order the system stacked them at the first Tab.
#[derive(Debug, Default)]
pub(crate) struct Switch {
    /// The windows, by their handles' values.
    order: [usize; ORDER_MAX],
    len: usize,
    /// The window stepped to last, or the one in front as the order was taken.
    at: usize,
    /// None of the order was in front as it was taken, and nothing of it has
    /// been brought forward since.
    behind: bool,
}

impl Switch {
    /// Bring the application's next window to the front: the order taken
    /// afresh unless `again`, stepped `backwards` with Shift. **From behind**,
    /// none of the application's windows in front, the first of the order --
    /// the application's topmost -- comes forward, the one window alone too;
    /// from one of them, the next, where there is another. A window gone or
    /// hidden since the order was taken is stepped past.
    pub(crate) fn step(&mut self, judge: &mut Judge, backwards: bool, again: bool) {
        if !again || self.len == 0 {
            self.take(judge);
        }
        let behind = self.behind;
        for place in tries(self.at, self.len, behind, backwards) {
            let Some(&window) = self.order.get(place) else {
                return;
            };
            let window = window as HWND;
            if !switchable(window) {
                continue;
            }
            self.at = place;
            self.behind = false;
            let taken = bring(window);
            lowlat_common::log_info!(
                "inject: alt-tab brought the application's next window forward, windows={} behind={} backwards={} taken={}",
                self.len,
                u8::from(behind),
                u8::from(backwards),
                u8::from(taken)
            );
            return;
        }
    }

    fn take(&mut self, judge: &mut Judge) {
        struct Walk<'a> {
            judge: &'a mut Judge,
            order: [usize; ORDER_MAX],
            len: usize,
        }
        unsafe extern "system" fn each(window: HWND, data: LPARAM) -> windows_sys::core::BOOL {
            // SAFETY: the walk below, alive for the enumeration that calls this.
            let walk = unsafe { &mut *(data as *mut Walk<'_>) };
            if switchable(window)
                && walk.judge.holds_window(window)
                && let Some(slot) = walk.order.get_mut(walk.len)
            {
                *slot = window as usize;
                walk.len += 1;
            }
            i32::from(walk.len < ORDER_MAX)
        }
        let mut walk = Walk {
            judge,
            order: [0; ORDER_MAX],
            len: 0,
        };
        // SAFETY: a callback that reads the walk only while the enumeration
        // that calls it runs.
        unsafe { EnumWindows(Some(each), (&raw mut walk) as LPARAM) };
        let front = walk.judge.shown().map(|window| window as usize);
        self.order = walk.order;
        self.len = walk.len;
        // **The window in front is where the stepping starts**; with none of
        // the order in front, it starts from behind.
        let at = front.and_then(|front| {
            self.order
                .iter()
                .take(self.len)
                .position(|&window| window == front)
        });
        self.at = at.unwrap_or(0);
        self.behind = at.is_none();
    }
}

/// The places of an order of `len` a Tab tries, the first that can come
/// forward taken: **from behind, every one from the first**; from `at`, every
/// other, the next first -- and none where it is alone.
fn tries(at: usize, len: usize, behind: bool, backwards: bool) -> impl Iterator<Item = usize> {
    let count = if behind { len } else { len.saturating_sub(1) };
    let first = if behind {
        0
    } else {
        stepped(at, len, backwards)
    };
    core::iter::successors(Some(first), move |&place| {
        Some(stepped(place, len, backwards))
    })
    .take(count)
}

/// The place after `at` in an order of `len`, or before it, round the end.
fn stepped(at: usize, len: usize, backwards: bool) -> usize {
    match len {
        0 => 0,
        _ if backwards => (at + len - 1) % len,
        _ => (at + 1) % len,
    }
}

/// Whether a window is one the system's switcher lists: shown or minimised,
/// not cloaked, and a window of its own -- no owner, or one that asks for a
/// place of its own -- that is not a tool window.
fn switchable(window: HWND) -> bool {
    // SAFETY: a window handle, which the calls check.
    let (shown, owned, style) = unsafe {
        (
            IsWindow(window) != 0 && IsWindowVisible(window) != 0,
            !GetWindow(window, GW_OWNER).is_null(),
            GetWindowLongPtrW(window, GWL_EXSTYLE),
        )
    };
    let tool = style & WS_EX_TOOLWINDOW as isize != 0;
    let own = style & WS_EX_APPWINDOW as isize != 0;
    shown && !tool && (!owned || own) && !cloaked(window)
}

/// Whether a window is kept off the screen by its program or on another
/// virtual desktop.
fn cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: a window handle and an output of the attribute's size.
    let read = unsafe {
        DwmGetWindowAttribute(window, DWMWA_CLOAKED as u32, (&raw mut cloaked).cast(), 4)
    };
    read >= 0 && cloaked != 0
}

/// A window brought to the front: restored first where it is minimised, and
/// **asked again with this thread's input joined to the window in front's**
/// where the system refused the front the first time, as it refuses a
/// process that may not take it. Whether it was taken.
fn bring(window: HWND) -> bool {
    // SAFETY: a window handle and thread numbers, which the calls check; the
    // threads' input is joined for the one call and parted after.
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        if SetForegroundWindow(window) != 0 {
            return true;
        }
        let theirs = GetWindowThreadProcessId(GetForegroundWindow(), core::ptr::null_mut());
        let mine = GetCurrentThreadId();
        let joined = theirs != 0 && theirs != mine && AttachThreadInput(mine, theirs, 1) != 0;
        let taken = SetForegroundWindow(window) != 0;
        BringWindowToTop(window);
        if joined {
            AttachThreadInput(mine, theirs, 0);
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// **An executable is its full path, compared as the file system compares
    /// names**: case aside and either separator, but never a name alone, a
    /// directory or a path that only begins the same.
    #[test]
    fn a_scope_holds_its_executables_by_their_full_paths() {
        // The second path's letters past ASCII, lower case here and upper
        // case below: u and i with diaereses.
        let scope = Scope::new([
            r"E:\Games\Example Client\client.exe",
            "C:/Games/\u{fc}n\u{ef}code/game.exe",
        ]);
        assert!(scope.holds(&wide(r"E:\Games\Example Client\client.exe")));
        assert!(scope.holds(&wide(r"e:\GAMES\example client\CLIENT.EXE")));
        assert!(scope.holds(&wide("C:\\Games\\\u{dc}N\u{cf}CODE\\GAME.exe")));
        assert!(!scope.holds(&wide("client.exe")), "a name alone passed");
        assert!(
            !scope.holds(&wide(r"D:\copy\client.exe")),
            "a copy elsewhere passed"
        );
        assert!(
            !scope.holds(&wide(r"E:\Games\Example Client")),
            "the folder passed"
        );
        assert!(
            !scope.holds(&wide(r"E:\Games\Example Client\client.exe.bak")),
            "a longer path passed"
        );
        assert!(!Scope::new::<[&str; 0], &str>([]).holds(&wide(r"C:\a.exe")));
        assert_eq!(
            scope.paths().collect::<Vec<_>>(),
            vec![
                r"E:\Games\Example Client\client.exe".to_owned(),
                "C:\\Games\\\u{fc}n\u{ef}code\\game.exe".to_owned()
            ]
        );
    }

    /// **This process is its own image, and only that**: named through its
    /// own handle, kept while it lives, and the window-less question of
    /// another process answered no.
    #[test]
    fn a_process_is_judged_by_the_image_it_runs() {
        let me = std::env::current_exe().expect("this test's own path");
        let mut held = Judge::new(Arc::new(Scope::new([me.as_os_str()])));
        let pid = std::process::id();
        assert!(held.holds_process(pid));
        assert_eq!(held.known.len(), 1);
        assert!(held.holds_process(pid), "the kept answer changed");
        assert_eq!(held.known.len(), 1, "the answer was asked for again");

        let mut other = Judge::new(Arc::new(Scope::new([r"C:\nowhere\none.exe"])));
        assert!(!other.holds_process(pid));
        assert!(!other.holds_process(0), "the idle process passed");
    }

    /// **A process opened with fewer rights than asked for names nothing, and
    /// its answer is not kept**: a handle that may only wait, as a process
    /// another program protects may hand out, is not the application's, and
    /// the process is asked about again.
    #[test]
    fn a_process_that_cannot_be_named_is_asked_about_again() {
        let me = std::env::current_exe().expect("this test's own path");
        let mut judge = Judge::new(Arc::new(Scope::new([me.as_os_str()])));
        let pid = std::process::id();
        // SAFETY: a right the system defines, not inherited; checked below.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(
            !handle.is_null(),
            "this process could not be opened to wait on"
        );
        assert!(!judge.judge_opened(pid, Process(handle)));
        assert!(judge.known.is_empty(), "an answer with no name was kept");
        assert!(
            judge.holds_process(pid),
            "the process was not asked about again"
        );
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// **A window's picture is cut to its display**: where two rectangles
    /// meet, and nothing where they only touch or miss.
    #[test]
    fn a_window_is_cut_to_its_display() {
        let display = rect(0, 0, 2560, 1440);
        let cut = overlap(rect(-100, 1000, 800, 1600), display).map(corners);
        assert_eq!(cut, Some((0, 1000, 800, 1440)));
        assert_eq!(
            overlap(rect(2560, 0, 3000, 500), display).map(corners),
            None
        );
        assert_eq!(
            overlap(rect(0, 1440, 500, 1600), display).map(corners),
            None
        );
    }

    /// **The pointer is clipped to the window unless the clip already keeps
    /// it there**: no clip in force or one wider than the window is clipped;
    /// the window's own rectangle and an application's clip inside it are
    /// left as they are; a clip of this guest's own inside it -- a window since
    /// grown -- is clipped again.
    #[test]
    fn the_pointer_is_clipped_to_the_window_unless_it_already_is() {
        let window = rect(200, 150, 1400, 850);
        let whole = rect(0, 0, 2560, 1440);
        let game = rect(500, 400, 900, 600);
        assert!(clips(whole, window, None), "no clip in force was left");
        assert!(clips(rect(100, 100, 1500, 900), window, None));
        assert!(!clips(window, window, Some(corners(window))));
        assert!(!clips(window, window, None));
        assert!(
            !clips(game, window, None),
            "the application's own clip was widened"
        );
        assert!(
            clips(game, window, Some(corners(game))),
            "an older clip of ours was kept"
        );
    }

    /// **Tabs under one Alt step round the order**, either way.
    #[test]
    fn tabs_step_round_the_order() {
        assert_eq!(stepped(0, 3, false), 1);
        assert_eq!(stepped(2, 3, false), 0);
        assert_eq!(stepped(0, 3, true), 2);
        assert_eq!(stepped(1, 3, true), 0);
        assert_eq!(stepped(0, 0, false), 0);
    }

    /// **A Tab tries every other window from the one in front, and from
    /// behind every window from the first** -- the one window alone too, where
    /// from the one in front it tries none.
    #[test]
    fn a_tab_from_behind_tries_the_applications_topmost_first() {
        let tried =
            |at, len, behind, backwards| tries(at, len, behind, backwards).collect::<Vec<_>>();
        assert_eq!(tried(0, 3, false, false), vec![1, 2]);
        assert_eq!(tried(0, 3, false, true), vec![2, 1]);
        assert_eq!(tried(1, 3, false, false), vec![2, 0]);
        assert_eq!(tried(0, 3, true, false), vec![0, 1, 2]);
        assert_eq!(tried(0, 3, true, true), vec![0, 2, 1]);
        assert_eq!(
            tried(0, 1, true, false),
            vec![0],
            "a lone window stayed behind"
        );
        assert!(tried(0, 1, false, false).is_empty());
        assert!(tried(0, 0, true, false).is_empty());
    }

    /// **The desktop's pixels and the screen's differ by the desktop's corner
    /// alone**, there and back: a display left of and below the primary, the
    /// desktop's corner at its left edge.
    #[test]
    fn a_point_of_the_desktop_is_found_on_the_screen() {
        let corner = (-352, 0);
        let bounds = on_desktop_from(rect(-352, 1440, 1568, 2640), corner);
        assert_eq!(
            (bounds.left, bounds.top, bounds.right, bounds.bottom),
            (0, 1440, 1920, 2640)
        );
        let back = screen_point_from(bounds.left, bounds.top, corner);
        assert_eq!((back.x, back.y), (-352, 1440));
    }
}
