//! The host's own icon in the notification area, on Windows: what the host is
//! doing, a kick for each guest and the rate's ceiling, and a notice as a
//! guest arrives and leaves (docs/impl-plan-windows.md W2.9).
//!
//! **The host runs in the session**, so this is a thread of the host with a
//! hidden window of its own on the session's default desktop, where Linux has
//! a program the desktop starts. It never moves to the input desktop, which is
//! the capture's and the input's business. The service's loop owns the
//! guests: a click is queued for it, and what to show is handed over when it
//! changes.
//!
//! **Who may act through it is the console's desktop** (docs/07-platforms.md
//! section 5.1): a choice in the menu takes input there, and a command or a key
//! from a program below the system account's integrity is refused. Such a
//! program can still open the menu and delete the icon, so the icon is added
//! again whenever an update of it fails.

use std::sync::{Arc, Mutex, mpsc};

use windows_sys::Win32::Foundation::{FreeLibrary, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
    LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CheckMenuRadioItem, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    HICON, HMENU, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW,
    MF_BYCOMMAND, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, PostMessageW,
    RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SMTO_ABORTIFHUNG,
    SendMessageTimeoutW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_NULL, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED,
};

use crate::tray::{self, Click, Entry, Shown};

/// The icon's own message: the shell saying it was clicked or its notice shown.
const CALLBACK: u32 = WM_APP + 1;
/// A new state waits in the mailbox.
const WAKE: u32 = WM_APP + 2;
/// The host is going: take the icon away.
const LEAVE: u32 = WM_APP + 3;
/// The shell's word for a click and for its keyboard equivalent (version 4).
const NIN_SELECT: u32 = 0x400;
const NIN_KEYSELECT: u32 = 0x401;
const ICON_ID: u32 = 1;
/// The system's own display icon, in the shell's own image library.
const DISPLAY_ICON: usize = 110;
/// How long the host waits for the icon to be taken away as it ends.
const LEAVE_MS: u32 = 500;

/// The tray, as the service's loop holds it.
#[derive(Debug)]
pub(crate) struct Tray {
    window: usize,
    mailbox: Arc<Mutex<Option<serde_json::Value>>>,
    clicks: Mutex<mpsc::Receiver<Click>>,
}

/// What the tray's thread keeps, read by its window's procedure.
struct Context {
    taskbar_created: u32,
    icon: HICON,
    shown: Shown,
    /// The guests last told about; none before the first state, which is a
    /// baseline rather than news.
    known: Option<Vec<tray::Guest>>,
    mailbox: Arc<Mutex<Option<serde_json::Value>>>,
    clicks: mpsc::Sender<Click>,
    /// Whether the last add failed, so its line is said once.
    missing: bool,
}

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<Context>> = const { std::cell::RefCell::new(None) };
}

impl Tray {
    /// Start the tray's thread and its window, or say why there is none.
    pub(crate) fn start() -> Option<Self> {
        let mailbox = Arc::new(Mutex::new(None));
        let (clicks, clicked) = mpsc::channel();
        let (made, window) = mpsc::channel();
        let theirs = Arc::clone(&mailbox);
        let spawned = std::thread::Builder::new()
            .name("lowlatd-tray".to_string())
            .spawn(move || run(&made, theirs, clicks));
        if let Err(error) = spawned {
            lowlat_common::log_warn!("tray: no thread for the icon, error={error}");
            return None;
        }
        match window.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(window)) => Some(Self {
                window,
                mailbox,
                clicks: Mutex::new(clicked),
            }),
            Ok(None) | Err(_) => {
                lowlat_common::log_warn!("tray: no window for the icon, the desk is shown nothing");
                None
            }
        }
    }

    /// Show this state: the tooltip, and a notice for whoever arrived or left.
    pub(crate) fn show(&self, state: serde_json::Value) {
        if let Ok(mut mailbox) = self.mailbox.lock() {
            *mailbox = Some(state);
        }
        // SAFETY: a window of this process's tray thread, or one already gone,
        // which the call refuses.
        unsafe { PostMessageW(self.window as HWND, WAKE, 0, 0) };
    }

    /// What the desk asked for since the last pass.
    pub(crate) fn clicks(&self) -> Vec<Click> {
        self.clicks
            .lock()
            .map(|clicks| clicks.try_iter().collect())
            .unwrap_or_default()
    }

    /// Take the icon away as the host ends, waiting a bounded time: a killed
    /// host's icon stays in the notification area until the pointer passes
    /// over it.
    pub(crate) fn leave(&self) {
        let mut answer = 0usize;
        // SAFETY: a window of this process's tray thread; the answer is written
        // by the call, which returns by its bound whatever the thread does.
        unsafe {
            SendMessageTimeoutW(
                self.window as HWND,
                LEAVE,
                0,
                0,
                SMTO_ABORTIFHUNG,
                LEAVE_MS,
                &raw mut answer,
            )
        };
    }
}

/// The tray's thread: the window, the icon, and the window's messages.
fn run(
    made: &mpsc::Sender<Option<usize>>,
    mailbox: Arc<Mutex<Option<serde_json::Value>>>,
    clicks: mpsc::Sender<Click>,
) {
    let class: Vec<u16> = "lowlatd tray\0".encode_utf16().collect();
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
        // **A top-level window, hidden, never a message-only one**: the
        // taskbar's word that it was made again and the session's end are
        // broadcasts, which a message-only window never gets.
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            let _ = made.send(None);
            return;
        }
        let taskbar_created = RegisterWindowMessageW(
            "TaskbarCreated\0"
                .encode_utf16()
                .collect::<Vec<_>>()
                .as_ptr(),
        );
        // **The taskbar's broadcast alone is let through**: from below the
        // system account's integrity a broadcast never arrives without it,
        // where the shell's own messages to the icon arrive as they are.
        if ChangeWindowMessageFilterEx(window, taskbar_created, MSGFLT_ALLOW, std::ptr::null_mut())
            == 0
        {
            lowlat_common::log_warn!(
                "tray: the taskbar's restart will not be heard, the icon is not added again after it"
            );
        }
        CONTEXT.with(|context| {
            *context.borrow_mut() = Some(Context {
                taskbar_created,
                icon: display_icon(),
                shown: Shown {
                    connected: true,
                    ..Shown::default()
                },
                known: None,
                mailbox,
                clicks,
                missing: false,
            });
        });
        with_context(|context| add(window, context));
        let _ = made.send(Some(window as usize));
        let mut message: MSG = core::mem::zeroed();
        while GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// Run `act` on the thread's context, borrowed for no longer than the call:
/// the menu's own loop dispatches messages, so nothing may hold it across one.
fn with_context<T>(act: impl FnOnce(&mut Context) -> T) -> Option<T> {
    CONTEXT.with(|context| context.try_borrow_mut().ok()?.as_mut().map(act))
}

unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let taskbar_created = with_context(|context| context.taskbar_created).unwrap_or(0);
    match message {
        CALLBACK => {
            // Version 4: the event in the low word.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the low word"
            )]
            let event = (l as u32) & 0xFFFF;
            // **The menu on the shell's own words for asking**: a right click
            // comes as its raw button messages and as the context menu, and
            // opening on both would open the menu again as it closes.
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                show_menu(window, w);
            }
            0
        }
        WAKE => {
            let state = with_context(|context| {
                context
                    .mailbox
                    .lock()
                    .ok()
                    .and_then(|mut mailbox| mailbox.take())
            })
            .flatten();
            if let Some(state) = state {
                with_context(|context| changed(window, context, &state));
            }
            0
        }
        LEAVE => {
            let data = icon_data(window);
            // SAFETY: the icon's identity, in a structure that lives for the call.
            unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) };
            0
        }
        _ if taskbar_created != 0 && message == taskbar_created => {
            lowlat_common::log_info!("tray: the taskbar was made again, the icon added to it");
            let data = icon_data(window);
            // SAFETY: as above.
            unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) };
            with_context(|context| {
                context.missing = false;
                add(window, context);
            });
            0
        }
        // SAFETY: the window's own message, handed on unchanged.
        _ => unsafe { DefWindowProcW(window, message, w, l) },
    }
}

/// The icon's identity: this window's, first and only icon.
fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    // SAFETY: a plain structure, every field of which zero is a valid value.
    let mut data: NOTIFYICONDATAW = unsafe { core::mem::zeroed() };
    data.cbSize = u32::try_from(core::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0);
    data.hWnd = window;
    data.uID = ICON_ID;
    data
}

/// Copy `text` into a fixed field, cut to fit and terminated.
fn put(field: &mut [u16], text: &str) {
    let room = field.len().saturating_sub(1);
    let mut length = 0;
    for (slot, unit) in field.iter_mut().zip(text.encode_utf16().take(room)) {
        *slot = unit;
        length += 1;
    }
    if let Some(end) = field.get_mut(length) {
        *end = 0;
    }
}

/// The tooltip: what the host is doing, named.
fn tip(shown: &Shown) -> String {
    format!("lowlat: {}", shown.line())
}

/// Add the icon, and say so once when there is no taskbar to add it to.
fn add(window: HWND, context: &mut Context) -> bool {
    let mut data = icon_data(window);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = CALLBACK;
    data.hIcon = context.icon;
    put(&mut data.szTip, &tip(&context.shown));
    // SAFETY: a structure that lives for both calls.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) } != 0;
    if added {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_SETVERSION, &raw const data) };
        if context.missing {
            lowlat_common::log_info!("tray: the icon added");
        }
        context.missing = false;
    } else if !context.missing {
        lowlat_common::log_info!(
            "tray: no taskbar to add the icon to yet, it is added when one is made"
        );
        context.missing = true;
    }
    added
}

/// Ask the shell for `data`'s change, adding the icon again first where it
/// is gone: any program in the session can delete it.
fn modify(window: HWND, context: &mut Context, data: &NOTIFYICONDATAW) -> bool {
    // SAFETY: a structure that lives for the call.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, data) } != 0 {
        return true;
    }
    if !context.missing {
        lowlat_common::log_info!("tray: the icon was gone, adding it again");
    }
    // SAFETY: as above.
    add(window, context) && unsafe { Shell_NotifyIconW(NIM_MODIFY, data) } != 0
}

/// Take a new state: the tooltip, and a notice for who arrived and who left.
fn changed(window: HWND, context: &mut Context, state: &serde_json::Value) {
    context.shown.read(state);
    let mut data = icon_data(window);
    data.uFlags = NIF_TIP | NIF_SHOWTIP;
    put(&mut data.szTip, &tip(&context.shown));
    modify(window, context, &data);
    let told = context
        .known
        .as_deref()
        .map(|before| tray::arrivals(before, &context.shown.guests))
        .unwrap_or_default();
    context.known = Some(context.shown.guests.clone());
    if told.is_empty() {
        return;
    }
    let said: Vec<String> = told
        .iter()
        .map(|(who, arrived)| {
            format!(
                "{who} {}",
                if *arrived {
                    "connected"
                } else {
                    "disconnected"
                }
            )
        })
        .collect();
    let mut notice = icon_data(window);
    notice.uFlags = NIF_INFO;
    notice.dwInfoFlags = NIIF_INFO;
    put(&mut notice.szInfoTitle, &said.join(", "));
    // **Never empty**: a notice with no text is the shell's word for taking
    // one away.
    put(&mut notice.szInfo, &context.shown.line());
    if modify(window, context, &notice) {
        lowlat_common::log_info!(
            "tray: the desk told of {} arrival(s) and departure(s)",
            said.len()
        );
    } else {
        lowlat_common::log_info!(
            "tray: no taskbar to tell the desk of {} arrival(s) and departure(s)",
            said.len()
        );
    }
}

/// The menu's leaf entries in the order they are drawn, the system's command
/// for each being its place in this list plus one. **Numbered here, never by
/// the entry's own number**: a menu command carries sixteen bits, and a kick's
/// number is past them.
fn commands(entries: &[Entry]) -> Vec<i32> {
    let mut numbered = Vec::new();
    for entry in entries {
        if entry.separator {
            continue;
        }
        if entry.children.is_empty() {
            numbered.push(entry.id);
        } else {
            numbered.extend(commands(&entry.children));
        }
    }
    numbered
}

/// What the system's command `chosen` asks for.
fn click_of(numbered: &[i32], chosen: i32) -> Option<Click> {
    let at = usize::try_from(chosen.checked_sub(1)?).ok()?;
    tray::clicked(*numbered.get(at)?)
}

/// A label as the system's menu reads it: an ampersand is otherwise a
/// mnemonic's mark, and a guest's name may carry one.
fn label(text: &str) -> Vec<u16> {
    text.replace('&', "&&")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

/// Make the system's menu for `entries`, numbered by `numbered`.
fn build(entries: &[Entry], numbered: &[i32]) -> HMENU {
    let command = |id: i32| {
        numbered
            .iter()
            .position(|listed| *listed == id)
            .map_or(0, |at| at + 1)
    };
    // SAFETY: a menu this function makes and hands to its caller, every
    // label living for the call that copies it.
    unsafe {
        let menu = CreatePopupMenu();
        let mut radios: Vec<(usize, bool)> = Vec::new();
        for entry in entries {
            let grayed = if entry.enabled { 0 } else { MF_GRAYED };
            if entry.separator {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else if entry.children.is_empty() {
                let at = command(entry.id);
                AppendMenuW(menu, MF_STRING | grayed, at, label(&entry.label).as_ptr());
                if let Some(selected) = entry.radio {
                    radios.push((at, selected));
                }
            } else {
                let sub = build(&entry.children, numbered);
                AppendMenuW(
                    menu,
                    MF_POPUP | grayed,
                    sub as usize,
                    label(&entry.label).as_ptr(),
                );
            }
        }
        // **One of a set marked as the set's**, as the panel shows it on Linux.
        if let (Some(first), Some(last), Some((selected, _))) = (
            radios.first(),
            radios.last(),
            radios.iter().find(|(_, selected)| *selected),
        ) {
            let id = |at: usize| u32::try_from(at).unwrap_or(0);
            CheckMenuRadioItem(menu, id(first.0), id(last.0), id(*selected), MF_BYCOMMAND);
        }
        menu
    }
}

/// Open the menu where the shell said, and queue what is chosen for the loop.
fn show_menu(window: HWND, anchor: WPARAM) {
    let Some(shown) = with_context(|context| context.shown.clone()) else {
        return;
    };
    let tree = tray::menu(&shown, false);
    let numbered = commands(&tree.children);
    let menu = build(&tree.children, &numbered);
    // The anchor's signed coordinates, the shell's own point.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "two signed words"
    )]
    let (mut x, mut y) = (
        i32::from(anchor as u16 as i16),
        i32::from((anchor >> 16) as u16 as i16),
    );
    if anchor == 0 {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: a point the call writes.
        unsafe { GetCursorPos(&raw mut point) };
        (x, y) = (point.x, point.y);
    }
    // SAFETY: this thread's window and menu. **The foreground first and a null
    // message after**, or the menu neither closes on a click elsewhere nor
    // opens a second time.
    let chosen = unsafe {
        SetForegroundWindow(window);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            x,
            y,
            0,
            window,
            std::ptr::null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        chosen
    };
    if let Some(click) = click_of(&numbered, chosen) {
        with_context(|context| {
            let _ = context.clicks.send(click);
        });
    }
}

/// The system's own display icon at the notification area's size, from the
/// shell's image library read as data from the system's directory alone; the
/// generic application icon where it has none.
fn display_icon() -> HICON {
    let library: Vec<u16> = "imageres.dll\0".encode_utf16().collect();
    // SAFETY: a library mapped as data, from the system's directory, never
    // run; the icon made from it is a copy, so it is let go after.
    unsafe {
        let module = LoadLibraryExW(
            library.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE
                | LOAD_LIBRARY_AS_IMAGE_RESOURCE
                | LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        if !module.is_null() {
            let icon = LoadImageW(
                module,
                DISPLAY_ICON as *const u16,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            );
            FreeLibrary(module);
            if !icon.is_null() {
                return icon;
            }
        }
        LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(guests: &[u32]) -> Shown {
        Shown {
            connected: true,
            bitrate_mbps: 10,
            guests: guests
                .iter()
                .map(|number| tray::Guest {
                    number: *number,
                    connected: true,
                    ..tray::Guest::default()
                })
                .collect(),
            ..Shown::default()
        }
    }

    /// **A command is the entry's place, and maps back to that entry
    /// exactly**, a kick of a guest whose number puts the entry's own number
    /// far past sixteen bits included; separators and a submenu's own entry
    /// are not commands.
    #[test]
    fn a_command_maps_back_to_the_entry_it_stands_for() {
        let tree = tray::menu(&shown(&[1, 70_000]), false);
        let numbered = commands(&tree.children);
        assert_eq!(
            numbered,
            vec![
                tray::STATUS,
                tray::KICK_AT + 1,
                tray::KICK_AT + 70_000,
                tray::RATE_AT + 5,
                tray::RATE_AT + 10,
                tray::RATE_AT + 20,
                tray::RATE_AT + 30,
                tray::RATE_AT + 50,
                tray::FPS_AT,
                tray::FPS_AT + 30,
                tray::FPS_AT + 60,
                tray::FPS_AT + 120,
                tray::FPS_AT + 144,
                tray::QUALITY_AT,
                tray::QUALITY_AT + 1,
                tray::QUALITY_AT + 2,
                tray::LEVEL_AT,
                tray::LEVEL_AT + 1,
                tray::LEVEL_AT + 2,
            ]
        );
        assert_eq!(click_of(&numbered, 3), Some(Click::Kick(70_000)));
        assert_eq!(click_of(&numbered, 2), Some(Click::Kick(1)));
        assert_eq!(click_of(&numbered, 7), Some(Click::Bitrate(30)));
        assert_eq!(click_of(&numbered, 9), Some(Click::Fps(0)));
        assert_eq!(click_of(&numbered, 13), Some(Click::Fps(144)));
        assert_eq!(click_of(&numbered, 15), Some(Click::Quality(1)));
        assert_eq!(click_of(&numbered, 19), Some(Click::CgLevel(2)));
        // Nothing chosen, the status line, and a number past the menu.
        assert_eq!(click_of(&numbered, 0), None);
        assert_eq!(click_of(&numbered, 1), None);
        assert_eq!(click_of(&numbered, 20), None);
        assert!(
            u16::try_from(numbered.len()).is_ok(),
            "every command fits the sixteen bits a menu command carries"
        );
    }

    /// A field takes as much as fits and is always terminated.
    #[test]
    fn a_field_is_cut_to_fit_and_terminated() {
        let mut field = [0xFFFFu16; 4];
        put(&mut field, "abcdef");
        assert_eq!(
            field,
            [u16::from(b'a'), u16::from(b'b'), u16::from(b'c'), 0]
        );
        put(&mut field, "x");
        assert_eq!(field[..2], [u16::from(b'x'), 0]);
    }

    /// An ampersand in a guest's name is shown, not taken as a mnemonic.
    #[test]
    fn an_ampersand_in_a_label_is_shown() {
        assert_eq!(String::from_utf16_lossy(&label("a&b")), "a&&b\0");
    }

    /// **Off by default: it needs the desktop's taskbar**, and puts an icon and
    /// two notices on it. Run as the person at the desk with `--ignored`. The
    /// icon is added; a guest's arrival is told; the icon, deleted as any
    /// program in the session can delete it, is added again by the next
    /// change; and it is taken away as the host ends.
    #[test]
    #[ignore = "needs the desktop's taskbar"]
    fn the_icon_is_added_again_after_a_delete_and_taken_away_at_the_end() {
        use windows_sys::Win32::UI::Shell::{NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect};
        let listed = |window: usize| {
            let identity = NOTIFYICONIDENTIFIER {
                cbSize: u32::try_from(core::mem::size_of::<NOTIFYICONIDENTIFIER>()).unwrap_or(0),
                hWnd: window as HWND,
                uID: ICON_ID,
                guidItem: windows_sys::core::GUID::from_u128(0),
            };
            // SAFETY: a structure and a rectangle that live for the call.
            let mut rect = unsafe { core::mem::zeroed() };
            unsafe { Shell_NotifyIconGetRect(&raw const identity, &raw mut rect) == 0 }
        };
        let settle = || std::thread::sleep(std::time::Duration::from_millis(400));
        let tray = Tray::start().expect("a tray");
        assert!(listed(tray.window), "the icon was not added");
        let guest = |id: u32| serde_json::json!({ "id": id, "connected": true });
        tray.show(serde_json::json!({ "guests": [guest(1)] }));
        tray.show(serde_json::json!({ "guests": [guest(1), guest(2)] }));
        settle();
        let data = icon_data(tray.window as HWND);
        // SAFETY: the icon's identity, as any program can name it.
        assert_ne!(unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) }, 0);
        assert!(
            !listed(tray.window),
            "the icon was not deleted from outside"
        );
        tray.show(serde_json::json!({ "guests": [] }));
        settle();
        assert!(
            listed(tray.window),
            "the icon was not added again after a delete"
        );
        tray.leave();
        assert!(
            !listed(tray.window),
            "the icon was not taken away at the end"
        );
    }
}
