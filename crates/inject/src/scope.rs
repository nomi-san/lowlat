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

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, WAIT_OBJECT_0};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId,
    WindowFromPoint,
};

use crate::event::Gate;
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

/// What one guest's thread asks of the application before its input goes in.
#[derive(Debug)]
pub(crate) struct Judge {
    scope: Arc<Scope>,
    known: Vec<Known>,
    /// When this thread was last moved to the input desktop for an answer.
    moved: Option<lowlat_common::clock::Time>,
}

impl Judge {
    pub(crate) fn new(scope: Arc<Scope>) -> Self {
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
        };
        window.is_some_and(|window| self.holds_window(window))
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
    fn holds_window(&mut self, window: HWND) -> bool {
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
        let Some(process) = Process::open(pid) else {
            return false;
        };
        let holds = process
            .image()
            .is_some_and(|image| self.scope.holds(&image));
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
}
