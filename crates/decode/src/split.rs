//! The split, for every backend whose pictures are a device's textures: one
//! compute pass copies a decoded picture out of the decoder's surface into
//! one texture per plane, which another device on the adapter opens by its
//! handle, and the library's fence is signalled behind it. Nothing waits:
//! the picture is known finished when the fence passes the value its take
//! returns. One picture in [`TIMED_EVERY`] is timed on the device's own
//! clock, from its take to the fence passing.
//!
//! Beside it, the read-back's copy out: the rows of a picture mapped from a
//! staging texture, into the caller's planes.

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use lowlat_drivers::d3d11::{Com, Device, Event, Fence, SharedTexture, Span, Timer};
use lowlat_drivers::ffi::d3d11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC__bindgen_ty_1, D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
    D3D11_TEX2D_ARRAY_SRV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, DXGI_FORMAT,
    DXGI_FORMAT_R8_UINT, DXGI_FORMAT_R8G8_UINT, DXGI_FORMAT_R8G8B8A8_UINT,
    DXGI_FORMAT_R10G10B10A2_UINT, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16_UINT,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC, ID3D11ComputeShader, ID3D11Resource,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use lowlat_drivers::vcall;

use crate::d3d11::{Error, Result, TIMED_EVERY, check};
use crate::packed::{unpack_vuyx_row, unpack_y410_row};
use crate::{Format, Planes};

/// The split's compiled shaders, one per layout.
#[path = "d3d11_split.rs"]
mod code;

/// The views the split reads a surface through, per plane, as integers:
/// the luma and interleaved chroma of the two-plane layouts, or the one
/// packed plane at full chroma.
pub(crate) const fn views(format: Format) -> [DXGI_FORMAT; 2] {
    match format {
        Format::Nv12 => [DXGI_FORMAT_R8_UINT, DXGI_FORMAT_R8G8_UINT],
        Format::P010 => [DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16_UINT],
        Format::Yuv444 => [DXGI_FORMAT_R8G8B8A8_UINT, DXGI_FORMAT_UNKNOWN],
        Format::Yuv444_16 => [DXGI_FORMAT_R10G10B10A2_UINT, DXGI_FORMAT_UNKNOWN],
    }
}

/// The split's shaders, one per layout, made once per device.
struct Shaders {
    planar8: Com<ID3D11ComputeShader>,
    planar16: Com<ID3D11ComputeShader>,
    vuya: Com<ID3D11ComputeShader>,
    y410: Com<ID3D11ComputeShader>,
}

impl Shaders {
    fn new(device: &Device) -> Result<Self> {
        let make = |bytes: &[u8]| -> Result<Com<ID3D11ComputeShader>> {
            let mut shader: *mut ID3D11ComputeShader = core::ptr::null_mut();
            // SAFETY: a live device; the bytecode is the compiler's, live for
            // the call, and the output is a live local.
            let hr = unsafe {
                vcall!(
                    device.device(),
                    CreateComputeShader,
                    bytes.as_ptr().cast(),
                    u64::try_from(bytes.len()).unwrap_or(0),
                    core::ptr::null_mut(),
                    &raw mut shader
                )
            }
            .ok_or(Error::NoProfile)?;
            check(hr)?;
            // SAFETY: a shader whose reference the call handed over.
            unsafe { Com::from_raw(shader) }.ok_or(Error::NoProfile)
        };
        Ok(Self {
            planar8: make(code::PLANAR8)?,
            planar16: make(code::PLANAR16)?,
            vuya: make(code::VUYA)?,
            y410: make(code::Y410)?,
        })
    }

    fn for_format(&self, format: Format) -> *mut ID3D11ComputeShader {
        match format {
            Format::Nv12 => self.planar8.as_ptr(),
            Format::P010 => self.planar16.as_ptr(),
            Format::Yuv444 => self.vuya.as_ptr(),
            Format::Yuv444_16 => self.y410.as_ptr(),
        }
    }
}

/// The views the split reads one slice of `texture` through, per plane, for
/// pictures in `format`.
pub(crate) fn source_views(
    device: &Device,
    format: Format,
    texture: *mut ID3D11Texture2D,
    slice: usize,
) -> Result<[Option<Com<ID3D11ShaderResourceView>>; 2]> {
    let mut out = [None, None];
    for (view, format) in out.iter_mut().zip(views(format)) {
        if format == DXGI_FORMAT_UNKNOWN {
            continue;
        }
        // The plane is the one the view's format names: a one-channel view
        // of a two-plane surface is its luma, a two-channel one its chroma.
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            __bindgen_anon_1: D3D11_SHADER_RESOURCE_VIEW_DESC__bindgen_ty_1 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: u32::try_from(slice).map_err(|_| Error::TooLarge)?,
                    ArraySize: 1,
                },
            },
        };
        let mut raw: *mut ID3D11ShaderResourceView = core::ptr::null_mut();
        // SAFETY: a live device and texture; the description and output are
        // live.
        let hr = unsafe {
            vcall!(
                device.device(),
                CreateShaderResourceView,
                texture.cast::<ID3D11Resource>(),
                &raw const desc,
                &raw mut raw
            )
        }
        .ok_or(Error::NoProfile)?;
        check(hr)?;
        // SAFETY: a view whose reference the call handed over.
        *view = Some(unsafe { Com::from_raw(raw) }.ok_or(Error::NoProfile)?);
    }
    Ok(out)
}

/// The split on one device and the fence it signals, with the device's
/// timing of one picture in [`TIMED_EVERY`].
pub(crate) struct Split {
    shaders: Shaders,
    fence: Arc<Fence>,
    /// The last value the fence was asked to reach.
    signalled: u64,
    /// Times a picture split on the device's own clock, from its take to
    /// the fence passing; whether a span is closed and not yet read; and
    /// pictures taken so, of which one in [`TIMED_EVERY`] is timed.
    timer: Option<Timer>,
    timing: bool,
    taken: u32,
}

impl Split {
    /// The split on `device`, where it has a fence to say when the split's
    /// work is done and the shaders build; none on a device without.
    pub(crate) fn new(device: &Device) -> Option<Self> {
        let fence = device
            .has_fences()
            .then(|| device.fence(0).ok())
            .flatten()?;
        let shaders = Shaders::new(device).ok()?;
        Some(Self {
            shaders,
            fence: Arc::new(fence),
            signalled: 0,
            timer: device.timer().ok(),
            timing: false,
            taken: 0,
        })
    }

    pub(crate) fn fence(&self) -> Arc<Fence> {
        Arc::clone(&self.fence)
    }

    /// Sleep on the fence until no more than `most` pictures split are
    /// unfinished on the device, or `timeout` passes: whether they are then.
    pub(crate) fn settle(&self, most: u64, event: &Event, timeout: Duration) -> Result<bool> {
        let Some(value) = self.signalled.checked_sub(most) else {
            return Ok(true);
        };
        // A wake says only that the fence may have moved: the event can be
        // left set by an earlier wait's notification arriving after its
        // timeout, so the fence itself is asked after every one.
        let started = Instant::now();
        loop {
            if self.fence.completed() >= value {
                return Ok(true);
            }
            let Some(left) = timeout.checked_sub(started.elapsed()) else {
                return Ok(false);
            };
            self.fence.notify_at(value, event)?;
            event.wait(left);
        }
    }

    /// Open a picture's take: the span last closed is read into `decode_us`
    /// once the device has passed it, and whether this picture is timed is
    /// the answer, its span opened if so -- never while the last is unread,
    /// since the timer holds one span.
    pub(crate) fn take(&mut self, device: &Device, decode_us: &mut u32) -> bool {
        let timed = self.read_span(device, decode_us) && self.taken % TIMED_EVERY == 0;
        self.taken = self.taken.wrapping_add(1);
        if timed && let Some(timer) = &self.timer {
            device.begin_span(timer);
        }
        timed
    }

    /// Read the span last closed, once the device has passed it, into
    /// `decode_us`: whether a new one may be opened.
    fn read_span(&mut self, device: &Device, decode_us: &mut u32) -> bool {
        let Some(timer) = &self.timer else {
            return false;
        };
        if !self.timing {
            return true;
        }
        match device.span(timer) {
            Ok(Span::Pending) => return false,
            Ok(Span::Micros(us)) => *decode_us = us,
            Ok(Span::Unsteady) | Err(_) => {}
        }
        self.timing = false;
        true
    }

    /// Queue the pass for a picture in `format`, `width` x `height` visible,
    /// read through `sources` into `planes`.
    pub(crate) fn dispatch(
        &self,
        device: &Device,
        format: Format,
        sources: [*mut ID3D11ShaderResourceView; 2],
        planes: [Option<&SharedTexture>; 3],
        (width, height): (u32, u32),
    ) -> Result<()> {
        let context = device.context();
        let uavs: [*mut ID3D11UnorderedAccessView; 3] =
            planes.map(|p| p.map_or(core::ptr::null_mut(), SharedTexture::view));
        let wanted = if format.full_chroma() { 3 } else { 2 };
        if uavs.iter().take(wanted).any(|u| u.is_null()) {
            return Err(Error::TooLarge);
        }
        // One thread a chroma sample of the two-plane layouts, which covers
        // four luma samples; one a sample at full chroma.
        let (cols, rows) = if format.full_chroma() {
            (width, height)
        } else {
            (width.div_ceil(2), height.div_ceil(2))
        };
        let none_srv = [core::ptr::null_mut::<ID3D11ShaderResourceView>(); 2];
        let none_uav = [core::ptr::null_mut::<ID3D11UnorderedAccessView>(); 3];
        // SAFETY: a live context on this thread; the shader, the views and the
        // targets are live, and every binding is undone before returning so
        // nothing of the split stays bound into the next decode.
        unsafe {
            vcall!(
                context,
                CSSetShader,
                self.shaders.for_format(format),
                core::ptr::null(),
                0
            );
            vcall!(context, CSSetShaderResources, 0, 2, sources.as_ptr());
            vcall!(
                context,
                CSSetUnorderedAccessViews,
                0,
                3,
                uavs.as_ptr(),
                core::ptr::null()
            );
            vcall!(context, Dispatch, cols.div_ceil(8), rows.div_ceil(8), 1);
            vcall!(context, CSSetShaderResources, 0, 2, none_srv.as_ptr());
            vcall!(
                context,
                CSSetUnorderedAccessViews,
                0,
                3,
                none_uav.as_ptr(),
                core::ptr::null()
            );
        }
        Ok(())
    }

    /// Signal the fence behind the work queued so far -- the span closed
    /// behind it with `timed` -- and hand the queue to the device: the value
    /// the picture is finished at.
    pub(crate) fn signal(&mut self, device: &Device, timed: bool) -> Result<u64> {
        let value = self.signalled + 1;
        match self.timer.as_ref().filter(|_| timed) {
            Some(timer) => {
                device.signal_timed(&self.fence, value, timer)?;
                self.timing = true;
            }
            None => device.signal(&self.fence, value)?,
        }
        self.signalled = value;
        Ok(value)
    }
}

/// A staging texture a picture or one plane is read back through: `format`
/// at `width` x `height`, one slice.
pub(crate) fn staging(
    device: &Device,
    format: DXGI_FORMAT,
    width: u32,
    height: u32,
) -> Result<Com<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ).unwrap_or(0),
        MiscFlags: 0,
    };
    let mut texture: *mut ID3D11Texture2D = core::ptr::null_mut();
    // SAFETY: a live device; the description and output are live.
    let hr = unsafe {
        vcall!(
            device.device(),
            CreateTexture2D,
            &raw const desc,
            core::ptr::null(),
            &raw mut texture
        )
    }
    .ok_or(Error::NoProfile)?;
    check(hr)?;
    // SAFETY: a texture whose reference the call handed over.
    unsafe { Com::from_raw(texture) }.ok_or(Error::NoProfile)
}

/// Copy the visible planes out of a mapped staging texture `allocated_rows`
/// tall: the two-plane layouts plane by plane, the chroma plane after the
/// luma plane's allocated rows; the packed full-chroma layouts unpacked, the
/// ten-bit samples moved to the high bits as every other backend hands them
/// out.
pub(crate) fn copy_planes(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    allocated_rows: u32,
    format: Format,
    (width, height): (u32, u32),
    out: &mut Planes<'_>,
) -> Result<()> {
    if mapped.pData.is_null() {
        return Err(Error::Status(-1));
    }
    let pitch = usize::try_from(mapped.RowPitch).map_err(|_| Error::TooLarge)?;
    let rows = usize::try_from(allocated_rows).map_err(|_| Error::TooLarge)?;
    let width = usize::try_from(width).map_err(|_| Error::TooLarge)?;
    let height = usize::try_from(height).map_err(|_| Error::TooLarge)?;
    let total = if format.full_chroma() {
        pitch * rows
    } else {
        pitch * (rows + format.chroma_rows(rows))
    };
    // SAFETY: the device mapped the whole staging texture at `pData`, which
    // is `RowPitch` bytes a row for each of its rows, and its second plane's
    // rows after the first's.
    let source = unsafe { core::slice::from_raw_parts(mapped.pData.cast::<u8>(), total) };
    match format {
        Format::Nv12 | Format::P010 => {
            let row_bytes = width * format.sample();
            copy_rows(source, 0, pitch, out.y, out.y_pitch, row_bytes, height)?;
            copy_rows(
                source,
                pitch * rows,
                pitch,
                out.uv,
                out.uv_pitch,
                row_bytes,
                format.chroma_rows(height),
            )
        }
        Format::Yuv444 | Format::Yuv444_16 => {
            let sample = format.sample();
            let rows = height.min(out.y.len() / out.y_pitch.max(1));
            for row in 0..rows {
                let from = source
                    .get(row * pitch..row * pitch + 4 * width)
                    .ok_or(Error::Status(-1))?;
                let span = |p: usize| row * p..row * p + sample * width;
                let y = out.y.get_mut(span(out.y_pitch)).ok_or(Error::TooLarge)?;
                let u = out.uv.get_mut(span(out.uv_pitch)).ok_or(Error::TooLarge)?;
                let v = out.v.get_mut(span(out.v_pitch)).ok_or(Error::TooLarge)?;
                if format == Format::Yuv444 {
                    unpack_vuyx_row(from, y, u, v);
                } else {
                    unpack_y410_row(from, y, u, v);
                }
            }
            Ok(())
        }
    }
}

/// `rows` rows of `row_bytes` from a plane of the mapping into a plane of
/// the caller's, each at its own pitch; short output takes what fits.
pub(crate) fn copy_rows(
    source: &[u8],
    offset: usize,
    pitch: usize,
    to: &mut [u8],
    to_pitch: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<()> {
    let rows = rows.min(to.len() / to_pitch.max(1));
    for row in 0..rows {
        let from = source
            .get(offset + row * pitch..offset + row * pitch + row_bytes)
            .ok_or(Error::Status(-1))?;
        let dst = to
            .get_mut(row * to_pitch..row * to_pitch + row_bytes)
            .ok_or(Error::TooLarge)?;
        dst.copy_from_slice(from);
    }
    Ok(())
}

/// Whole microseconds from a millisecond figure, saturated.
pub(crate) fn micros(ms: f64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative duration in whole microseconds, saturated"
    )]
    let us = (ms * 1000.0).max(0.0).min(f64::from(u32::MAX)) as u32;
    us
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_drivers::d3d11::D3d11;

    /// **A wake left over from an earlier wait does not end a settle**: with
    /// the event already set and the fence short of the value, the settle
    /// sleeps on to its deadline and answers that the pictures are still
    /// unfinished, rather than letting the next unit past the bound.
    #[test]
    #[ignore = "requires a GPU"]
    fn a_stale_wake_does_not_end_a_settle() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|a| a.decodes_here())
            .expect("a GPU");
        let device = d3d11.open(adapter.luid).expect("a device");
        let mut split = Split::new(&device).expect("a device with fences");
        // Pictures signalled that the device never reaches.
        split.signalled = split.fence.completed().saturating_add(8);
        let event = Event::new().expect("an event");
        event.set();
        let timeout = Duration::from_millis(50);
        let started = Instant::now();
        assert_eq!(split.settle(2, &event, timeout), Ok(false));
        assert!(started.elapsed() >= timeout, "{:?}", started.elapsed());
    }
}
