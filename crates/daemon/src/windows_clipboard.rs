//! The desktop's clipboard, on Windows: a guest's copied text put on it, and
//! its own copied text passed to the guests that may have it
//! (docs/07-platforms.md section 5.1, docs/impl-plan-windows.md W2.9).
//!
//! **Copied text on Windows is a value, not an ownership**: put with its
//! bytes, it stays on the clipboard after the host is gone, so nothing here
//! has to outlive a copy for a paste to work.
//!
//! **Read on a thread of its own.** Text another program announced and renders
//! only when asked is a wait on that program for as long as it takes --
//! fifteen seconds for one that hung fifteen, measured, with no limit of the
//! system's own -- so nothing else waits on this thread: the service's loop
//! takes what it has read and hands it what to put, and neither ever blocks.
//!
//! **Nothing is read with nobody to send it to.** The service's loop says
//! whether a connected guest may have the desktop's text, and until one may,
//! a change is not looked at.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, KillTimer, MSG,
    PostMessageW, RegisterClassW, SetTimer, TranslateMessage, WM_APP, WM_CLIPBOARDUPDATE, WM_TIMER,
    WNDCLASSW,
};

/// Unicode text, the one format copied text crosses in.
const CF_UNICODETEXT: u32 = 13;
/// The desktop's clipboard changed: the system's word, posted on to the loop.
const CHANGED: u32 = WM_APP + 1;
/// A guest's text waits to be put on the clipboard.
const WAKE: u32 = WM_APP + 2;
const FLOOR_TIMER: usize = 1;
/// The least time between two reads of the desktop's clipboard. A copy
/// settles in a burst of changes, and each read is a message to every guest
/// that may have it.
const FLOOR_MS: f64 = 50.0;
/// How often a clipboard another program holds open is asked again before a
/// change is let go, or a guest's text is.
const TRIES: u32 = 5;
/// The mark a program sets on a copy clipboard monitors are to skip.
const MARK: &str = "ExcludeClipboardContentFromMonitorProcessing";

/// The desktop's copy, as read.
#[derive(Debug)]
pub(crate) struct Copied {
    pub(crate) text: String,
    /// Its program asked clipboard monitors to skip it.
    pub(crate) marked: bool,
}

/// The clipboard's thread, as the service's loop holds it.
#[derive(Debug)]
pub(crate) struct Clipboard {
    window: usize,
    open: Arc<AtomicBool>,
    put: mpsc::Sender<Vec<u16>>,
    copied: Mutex<mpsc::Receiver<Copied>>,
}

impl Clipboard {
    /// Start the clipboard's thread and its window, or say why there is none.
    pub(crate) fn start() -> Option<Self> {
        let open = Arc::new(AtomicBool::new(false));
        let (put, to_put) = mpsc::channel();
        let (copy, copied) = mpsc::channel();
        let (made, window) = mpsc::channel();
        let theirs = Arc::clone(&open);
        let spawned = std::thread::Builder::new()
            .name("lowlatd-clipboard".to_string())
            .spawn(move || run(&made, &theirs, &to_put, &copy));
        if let Err(error) = spawned {
            lowlat_common::log_warn!("clipboard: no thread for it, error={error}");
            return None;
        }
        match window.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(window)) => Some(Self {
                window,
                open,
                put,
                copied: Mutex::new(copied),
            }),
            Ok(None) | Err(_) => {
                lowlat_common::log_warn!(
                    "clipboard: no window to hear it change, copied text crosses in neither direction"
                );
                None
            }
        }
    }

    /// Put a guest's text on the desktop's clipboard, as the wire carried it.
    pub(crate) fn put(&self, body: &[u8]) -> bool {
        // SAFETY: a window of this process's clipboard thread, or one already
        // gone, which the call refuses.
        self.put.send(to_wide(body)).is_ok()
            && unsafe { PostMessageW(self.window as HWND, WAKE, 0, 0) } != 0
    }

    /// Whether a connected guest may have the desktop's text, which is whether
    /// it is read at all.
    pub(crate) fn open(&self, open: bool) {
        self.open.store(open, Ordering::Relaxed);
    }

    /// What the desktop copied since the last pass.
    pub(crate) fn copied(&self) -> Vec<Copied> {
        self.copied
            .lock()
            .map(|copied| copied.try_iter().collect())
            .unwrap_or_default()
    }
}

/// What the clipboard's thread keeps between messages.
struct State {
    mark: u32,
    /// The sequence number the host's own last write left.
    own: u32,
    /// When the desktop's clipboard was last read.
    read_at: Option<lowlat_common::clock::Time>,
    /// A read waits on the floor's timer.
    waiting: bool,
    tries: u32,
}

/// The window's procedure: the system's word that the clipboard changed,
/// posted on to the loop, which does the reading where nothing is held.
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        // SAFETY: this thread's own window.
        unsafe { PostMessageW(window, CHANGED, 0, 0) };
        return 0;
    }
    // SAFETY: the window's own message, handed on unchanged.
    unsafe { DefWindowProcW(window, message, w, l) }
}

/// The clipboard's thread: a message-only window the system tells of each
/// change, and the loop that reads and writes.
fn run(
    made: &mpsc::Sender<Option<usize>>,
    open: &AtomicBool,
    to_put: &mpsc::Receiver<Vec<u16>>,
    copy: &mpsc::Sender<Copied>,
) {
    let class: Vec<u16> = "lowlatd clipboard\0".encode_utf16().collect();
    let mark: Vec<u16> = MARK.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a class and a window of this thread's, with this module's
    // procedure; the messages are pumped on this thread until it ends.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let registered = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..core::mem::zeroed()
        };
        RegisterClassW(&raw const registered);
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() || AddClipboardFormatListener(window) == 0 {
            let _ = made.send(None);
            return;
        }
        let _ = made.send(Some(window as usize));
        let mut state = State {
            mark: RegisterClipboardFormatW(mark.as_ptr()),
            own: 0,
            read_at: None,
            waiting: false,
            tries: 0,
        };
        let mut message: MSG = core::mem::zeroed();
        while GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) > 0 {
            match message.message {
                CHANGED => changed(window, open, copy, &mut state),
                WM_TIMER if message.wParam == FLOOR_TIMER => {
                    KillTimer(window, FLOOR_TIMER);
                    state.waiting = false;
                    read(window, open, copy, &mut state);
                }
                WAKE => {
                    for text in to_put.try_iter() {
                        write(window, &text, &mut state);
                    }
                }
                _ => {
                    TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
        }
    }
}

/// How long a change waits for the floor: none past it, else what is left of
/// it, at least a millisecond.
fn floor_wait(since_ms: Option<f64>) -> Option<u32> {
    let since = since_ms?;
    if since >= FLOOR_MS {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a wait of at most fifty milliseconds"
    )]
    let left = (FLOOR_MS - since.max(0.0)).ceil() as u32;
    Some(left.max(1))
}

/// The desktop's clipboard changed: read it now, or at the floor's end.
fn changed(window: HWND, open: &AtomicBool, copy: &mpsc::Sender<Copied>, state: &mut State) {
    // **Nobody to send it to, so not read**: not the text, not its length.
    if !open.load(Ordering::Relaxed) || state.waiting {
        return;
    }
    match floor_wait(state.read_at.map(lowlat_common::clock::elapsed_ms)) {
        None => read(window, open, copy, state),
        Some(left) => {
            // SAFETY: this thread's own window.
            unsafe { SetTimer(window, FLOOR_TIMER, left, None) };
            state.waiting = true;
        }
    }
}

/// Read the desktop's text and hand it to the loop, unless it is the host's
/// own write coming back.
fn read(window: HWND, open: &AtomicBool, copy: &mpsc::Sender<Copied>, state: &mut State) {
    if !open.load(Ordering::Relaxed) {
        return;
    }
    state.read_at = Some(lowlat_common::clock::Time::now());
    // SAFETY: the system's clipboard calls on this thread's window; the data
    // handle is read under the open and let go before the close.
    unsafe {
        // **The host's own write is not sent back** to the guest that made it.
        if GetClipboardSequenceNumber() == state.own
            || IsClipboardFormatAvailable(CF_UNICODETEXT) == 0
        {
            return;
        }
        let marked = IsClipboardFormatAvailable(state.mark) != 0;
        if OpenClipboard(window) == 0 {
            // **Another program holds it open**: asked again at the floor's
            // pace, a few times, then this change is let go.
            if state.tries < TRIES {
                state.tries += 1;
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "fifty"
                )]
                SetTimer(window, FLOOR_TIMER, FLOOR_MS as u32, None);
                state.waiting = true;
            } else {
                state.tries = 0;
                lowlat_common::log_info!(
                    "clipboard: held open by another program, a change let go"
                );
            }
            return;
        }
        state.tries = 0;
        let data = GetClipboardData(CF_UNICODETEXT);
        let text = if data.is_null() {
            None
        } else {
            read_global(data)
        };
        CloseClipboard();
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let _ = copy.send(Copied { text, marked });
        }
    }
}

/// The text a clipboard handle holds, up to its terminator.
///
/// # Safety
///
/// `data` is a clipboard's handle to Unicode text, read under its open.
unsafe fn read_global(data: HANDLE) -> Option<String> {
    // SAFETY: the handle locked for the read and unlocked after; the read
    // stays within the block's own size.
    unsafe {
        let units = GlobalLock(data).cast::<u16>();
        if units.is_null() {
            return None;
        }
        let length = GlobalSize(data) / 2;
        let text = from_wide(std::slice::from_raw_parts(units, length));
        GlobalUnlock(data);
        Some(text)
    }
}

/// Put a guest's text on the desktop's clipboard, with its bytes.
fn write(window: HWND, wide: &[u16], state: &mut State) {
    // SAFETY: the system's clipboard calls on this thread's window; the block
    // is written whole before it is handed over, and freed only where the
    // clipboard did not take it.
    unsafe {
        let mut opened = OpenClipboard(window) != 0;
        let mut tries = 1;
        while !opened && tries < TRIES {
            std::thread::sleep(std::time::Duration::from_millis(10));
            opened = OpenClipboard(window) != 0;
            tries += 1;
        }
        if !opened {
            lowlat_common::log_warn!(
                "clipboard: held open by another program, a guest's text not put"
            );
            return;
        }
        EmptyClipboard();
        let block = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        let units = GlobalLock(block).cast::<u16>();
        let put = if units.is_null() {
            false
        } else {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), units, wide.len());
            GlobalUnlock(block);
            !SetClipboardData(CF_UNICODETEXT, block).is_null()
        };
        if !put {
            GlobalFree(block);
            lowlat_common::log_warn!("clipboard: a guest's text was refused by the clipboard");
        }
        CloseClipboard();
        state.own = GetClipboardSequenceNumber();
    }
}

/// A guest's text as the clipboard takes it: one terminator the wire counted
/// taken off, the rest as sent -- line ends included -- and one put back.
fn to_wide(body: &[u8]) -> Vec<u16> {
    let text = body.strip_suffix(&[0]).unwrap_or(body);
    String::from_utf8_lossy(text)
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

/// The clipboard's text up to its terminator.
fn from_wide(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(units.get(..end).unwrap_or(units))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A guest's text reaches the clipboard as it was sent**: the wire's one
    /// terminator off and the clipboard's on, line ends untouched, and text
    /// beyond the basic plane in its pairs.
    #[test]
    fn a_guests_text_is_put_as_it_was_sent() {
        let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        assert_eq!(to_wide(b"one\r\ntwo\nthree\0"), wide("one\r\ntwo\nthree"));
        assert_eq!(
            to_wide("caf\u{e9} \u{1f600}\0".as_bytes()),
            wide("caf\u{e9} \u{1f600}")
        );
        assert_eq!(to_wide(b"no terminator"), wide("no terminator"));
        assert_eq!(to_wide(b""), vec![0]);
    }

    /// The desktop's text is read up to its terminator, pairs joined and a
    /// lone half replaced rather than refused.
    #[test]
    fn the_desktops_text_is_read_up_to_its_terminator() {
        let mut units: Vec<u16> = "hi \u{1f600}".encode_utf16().collect();
        units.extend([0, u16::from(b'z')]);
        assert_eq!(from_wide(&units), "hi \u{1f600}");
        assert_eq!(from_wide(&[0xD800, u16::from(b'a')]), "\u{fffd}a");
        assert_eq!(from_wide(&[u16::from(b'x')]), "x");
    }

    /// Copy `text` as another program would: its own window, its bytes, and
    /// the monitors' mark where `marked`.
    fn copy_elsewhere(text: &str, marked: bool) {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: a message-only window of this thread's, and the clipboard's
        // calls on it; each block is written whole before it is handed over.
        unsafe {
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            assert!(OpenClipboard(window) != 0, "the clipboard did not open");
            EmptyClipboard();
            let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            let block = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                GlobalLock(block).cast::<u16>(),
                wide.len(),
            );
            GlobalUnlock(block);
            SetClipboardData(CF_UNICODETEXT, block);
            if marked {
                let mark: Vec<u16> = MARK.encode_utf16().chain(Some(0)).collect();
                let block = GlobalAlloc(GMEM_MOVEABLE, 4);
                GlobalLock(block).cast::<u32>().write(0);
                GlobalUnlock(block);
                SetClipboardData(RegisterClipboardFormatW(mark.as_ptr()), block);
            }
            CloseClipboard();
        }
    }

    /// The desktop's text as another program reads it.
    fn paste_elsewhere() -> Option<String> {
        // SAFETY: the clipboard's calls with no window, reading alone.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let data = GetClipboardData(CF_UNICODETEXT);
            let text = if data.is_null() {
                None
            } else {
                read_global(data)
            };
            CloseClipboard();
            text
        }
    }

    /// **Off by default: it needs the desktop's clipboard, and overwrites it.**
    /// Run as the person at the desk with `--ignored`. A guest's text is pasted
    /// elsewhere and not read back; another program's copy is read, its mark
    /// with it; and with nobody to send it to, nothing is read.
    #[test]
    #[ignore = "needs the desktop's clipboard, which it overwrites"]
    fn text_crosses_both_ways_and_nothing_is_read_while_closed() {
        let settle = || std::thread::sleep(std::time::Duration::from_millis(300));
        let clipboard = Clipboard::start().expect("a clipboard");
        clipboard.open(true);
        assert!(clipboard.put(b"w29 from a guest\r\nline two\0"));
        settle();
        assert_eq!(
            paste_elsewhere().as_deref(),
            Some("w29 from a guest\r\nline two")
        );
        assert!(
            clipboard.copied().is_empty(),
            "the host's own write came back"
        );
        copy_elsewhere("w29 from the desk", false);
        settle();
        let copied = clipboard.copied();
        assert_eq!(copied.len(), 1, "{copied:?}");
        assert_eq!(copied[0].text, "w29 from the desk");
        assert!(!copied[0].marked);
        copy_elsewhere("w29 a password", true);
        settle();
        let copied = clipboard.copied();
        assert_eq!(copied.len(), 1, "{copied:?}");
        assert!(copied[0].marked, "the monitors' mark was not seen");
        clipboard.open(false);
        copy_elsewhere("w29 for nobody", false);
        settle();
        assert!(
            clipboard.copied().is_empty(),
            "read with nobody to send it to"
        );
    }

    /// **A change inside the floor is read when it ends**, a first change and
    /// one past it at once.
    #[test]
    fn a_change_inside_the_floor_waits_for_its_end() {
        assert_eq!(floor_wait(None), None);
        assert_eq!(floor_wait(Some(0.0)), Some(50));
        assert_eq!(floor_wait(Some(49.5)), Some(1));
        assert_eq!(floor_wait(Some(12.3)), Some(38));
        assert_eq!(floor_wait(Some(50.0)), None);
        assert_eq!(floor_wait(Some(400.0)), None);
    }
}
