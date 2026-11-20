//! The application's window in place of its output's picture, on Windows
//! (docs/05-host.md section 7.4): the system's capture of the application's
//! window, drawn at its place on a canvas the size of the output it is on,
//! black around it -- and black all over while the application shows no
//! window.
//!
//! **The picture follows the window in front while it is the application's**,
//! and stays on the last of the application's windows while another
//! program's is in front: the capture of a window goes on behind others. With
//! none of the application's windows followed yet, the topmost of them on the
//! screen is taken.
//!
//! **Never the desktop.** A capture the system refuses -- it refuses every
//! one to a host of the system account -- leaves the canvas black, and is
//! asked for again on a slow cadence.

use core::ffi::c_void;
use std::sync::Arc;

use lowlat_capture::d3d11::{Converter, Place};
use lowlat_capture::window::{self, Lacking, Window};
use lowlat_common::clock::{self, Time};
use lowlat_drivers::d3d11::{Com, Device, Output};
use lowlat_drivers::ffi::d3d11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC, ID3D11ShaderResourceView,
};
use lowlat_drivers::wgc::Wgc;
use lowlat_inject::{Judge, Scope};

use super::Error;

/// How long after the system refused the capture of a window it is asked
/// again: a refusal of the account's lasts, and a capture made for a window
/// costs milliseconds of the stream's thread.
const RETRY_MS: f64 = 2000.0;

/// The most of a capture's buffers whose views are kept: the pool's two, and
/// two more through a change of the window's size.
const VIEWS: usize = 4;

/// A picture to convert and where it goes.
pub(super) struct Taken {
    pub(super) view: Com<ID3D11ShaderResourceView>,
    pub(super) place: Place,
    /// When the system said the picture arrived, on the performance counter;
    /// nothing for the black canvas.
    pub(super) arrived: Option<i64>,
}

/// The window followed and its capture.
struct Followed {
    window: *mut c_void,
    capture: Window,
}

/// Whether this process can show an application's window as its picture.
/// **Not a host of the system account**: the system serves the capture of a
/// window per signed-in user and refuses every other account, and a system
/// older than the capture has none. **Asked of the system once**, since
/// neither answer moves while a process runs and the question costs 7 ms
/// (measured 2026-11-20).
#[must_use]
pub fn shows_windows() -> bool {
    static SHOWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHOWS.get_or_init(|| Wgc::load().is_ok_and(|wgc| matches!(window::supported(&wgc), Ok(true))))
}

/// The output the application shows a window on, by the output's name: its
/// window in front, else its topmost on the screen; nothing while it shows
/// none. What a stream holding its guests to the application is built on.
pub(super) fn output_of_application(scope: &Arc<Scope>) -> Option<String> {
    let mut judge = Judge::new(Arc::clone(scope));
    let shown = match window::foreground() {
        Some(front) if window::on_screen(front) && judge.holds_window(front) => Some(front),
        _ => window::topmost(&mut |each| judge.holds_window(each)),
    }?;
    window::output_of(shown)
}

/// The application's window as an output's picture.
pub(super) struct Windowed {
    wgc: Wgc,
    judge: Judge,
    followed: Option<Followed>,
    /// Views of the capture's buffers, by the texture's address: made once
    /// each, a pool cycling through two.
    views: Vec<(usize, Com<ID3D11ShaderResourceView>)>,
    /// What a black canvas is converted from: never read, since nothing of
    /// it is placed on the canvas.
    nothing: Com<ID3D11ShaderResourceView>,
    /// The canvas was converted black last, and nothing has been shown since.
    black: bool,
    /// A picture a wait saw arrive, not yet taken.
    pub(super) pending: bool,
    /// When the system last refused a capture, while it does.
    refused: Option<Time>,
    said_lacking: bool,
}

impl core::fmt::Debug for Windowed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Windowed")
            .field("following", &self.followed.is_some())
            .field("black", &self.black)
            .finish_non_exhaustive()
    }
}

impl Windowed {
    /// The pictures of the application `scope` names, on `device`.
    pub(super) fn new(device: &Device, scope: Arc<Scope>) -> Result<Self, Error> {
        let wgc = Wgc::load()?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 2,
            Height: 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let nothing = device.texture(&desc)?;
        Ok(Self {
            nothing: Converter::view_of(device, nothing.as_ptr())?,
            wgc,
            judge: Judge::new(scope),
            followed: None,
            views: Vec::with_capacity(VIEWS),
            black: false,
            pending: false,
            refused: None,
            said_lacking: false,
        })
    }

    /// The window the picture shows now: the application's in front; else
    /// the one followed, while it has not closed -- covered, hidden or
    /// minimised, its last picture stays; else the application's topmost on
    /// the screen; else none.
    fn wanted(&mut self) -> Option<*mut c_void> {
        if let Some(front) = window::foreground()
            && window::on_screen(front)
            && self.judge.holds_window(front)
        {
            return Some(front);
        }
        if let Some(followed) = &self.followed
            && !followed.capture.closed()
        {
            return Some(followed.window);
        }
        let judge = &mut self.judge;
        window::topmost(&mut |each| judge.holds_window(each))
    }

    /// Move the capture onto the window the picture shows now, where that is
    /// another: the stream and its canvas stay as they are.
    pub(super) fn follow(&mut self, device: &Device, output: &str) {
        let wanted = self.wanted();
        if wanted == self.followed.as_ref().map(|followed| followed.window) {
            return;
        }
        if self.followed.take().is_some() {
            self.views.clear();
            if wanted.is_none() {
                lowlat_common::log_info!(
                    "display: {output} shows no window of the application, the canvas black"
                );
            }
        }
        let Some(window) = wanted else {
            return;
        };
        if self
            .refused
            .is_some_and(|at| clock::elapsed_ms(at) < RETRY_MS)
        {
            return;
        }
        match Window::new(&self.wgc, device, window) {
            Ok(capture) => {
                let lacking = capture.lacking();
                if lacking != Lacking::default()
                    && !core::mem::replace(&mut self.said_lacking, true)
                {
                    lowlat_common::log_warn!(
                        "display: this system's capture of a window cannot leave the pointer out={} \
                         hide its border={} take a picture a millisecond={} take its popups={}",
                        u8::from(lacking.cursor),
                        u8::from(lacking.border),
                        u8::from(lacking.interval),
                        u8::from(lacking.secondary)
                    );
                }
                lowlat_common::log_info!(
                    "display: {output} shows the application's window, at={:?}",
                    window::frame(window)
                );
                self.refused = None;
                self.followed = Some(Followed { window, capture });
            }
            Err(error) => {
                if self.refused.is_none() {
                    lowlat_common::log_warn!(
                        "display: the application's window could not be captured, the canvas \
                         stays black and the capture is asked for every {RETRY_MS:.0} ms (a host \
                         of the system account is refused it), {error}"
                    );
                }
                self.refused = Some(Time::now());
            }
        }
    }

    /// The window followed, for a test reading its pictures' stamps.
    #[cfg(test)]
    pub(super) fn followed(&self) -> Option<*mut c_void> {
        self.followed.as_ref().map(|followed| followed.window)
    }

    /// The output the followed window is on, where one is followed.
    pub(super) fn output(&self) -> Option<String> {
        self.followed
            .as_ref()
            .and_then(|followed| window::output_of(followed.window))
    }

    /// Wait up to `timeout_ms` for a picture of the followed window, or for
    /// its capture to close: true if either came. With none followed, the
    /// time is slept.
    pub(super) fn wait(&self, timeout_ms: u32) -> bool {
        match &self.followed {
            Some(followed) => followed.capture.wait(timeout_ms),
            None => {
                std::thread::sleep(core::time::Duration::from_millis(u64::from(timeout_ms)));
                false
            }
        }
    }

    /// The picture to convert now, at its place on `output`'s canvas: the
    /// followed window's newest, or the black canvas once as the application
    /// stops showing a window. Nothing when neither changed.
    pub(super) fn take(
        &mut self,
        device: &Device,
        output: &Output,
    ) -> Result<Option<Taken>, Error> {
        self.pending = false;
        let Some(followed) = self.followed.as_mut() else {
            if core::mem::replace(&mut self.black, true) {
                return Ok(None);
            }
            return Ok(Some(Taken {
                view: self.nothing.clone_ref(),
                place: Place::NOTHING,
                arrived: None,
            }));
        };
        let Some(picture) = followed.capture.take()? else {
            return Ok(None);
        };
        self.black = false;
        // **At the window's frame on the output**, which is where the
        // picture's top left lies on the screen; a window gone a moment ago
        // leaves the corner.
        let (x, y) = window::frame(followed.window)
            .map_or((0, 0), |(x, y, _, _)| (x - output.left, y - output.top));
        let place = Place {
            x,
            y,
            width: picture.width,
            height: picture.height,
        };
        let key = picture.texture.as_ptr() as usize;
        let view = match self.views.iter().find(|(seen, _)| *seen == key) {
            Some((_, view)) => view.clone_ref(),
            None => {
                let view = Converter::view_of(device, picture.texture.as_ptr())?;
                if self.views.len() >= VIEWS {
                    self.views.remove(0);
                }
                self.views.push((key, view.clone_ref()));
                view
            }
        };
        Ok(Some(Taken {
            view,
            place,
            arrived: Some(picture.arrived),
        }))
    }
}
