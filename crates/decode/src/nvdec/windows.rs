//! Pictures copied into textures of the graphics interface: the textures a
//! caller lends, one per plane, written on the backend's stream, and a fence
//! of the textures' own device signalled behind the copies. **Nothing here
//! waits for the device**: a picture is handed on with the fence's value,
//! and whoever reads it waits for that.
//!
//! Each texture is mapped to the runtime for its copy and unmapped after,
//! both queued on the stream. The unmap puts the copies before any graphics
//! work the textures' device issues after it, so the fence, signalled there
//! next, passes its value only once they are done -- and says so to any
//! device that reads the fence. Of a decoder decoding into the backend's own
//! surfaces, the copies read a surface behind its decode on the same stream;
//! of one mapping its own, the map waits for the decode, and the picture
//! stays mapped until its copies have passed, which is looked at the next
//! time anything maps.

use core::ffi::c_int;
use std::sync::Arc;

use lowlat_drivers::cuda::Registered;
use lowlat_drivers::d3d11::{Device, Fence, Span, Timer};
use lowlat_drivers::ffi::cuda::{CU_MEMORYTYPE_ARRAY, CU_MEMORYTYPE_DEVICE, CUDA_MEMCPY2D};
use lowlat_drivers::ffi::cuvid::CUVIDPROCPARAMS;

use super::{Backend, Error, Result, micros, zeroed};
use crate::d3d11::TIMED_EVERY;
use crate::{Fault, Picture};

/// The textures' device and its fence, and the value the fence was last
/// asked to reach.
pub(super) struct Textures {
    device: Arc<Device>,
    fence: Arc<Fence>,
    value: u64,
    /// Times a picture on the textures' device, from its take to the fence
    /// passing; whether a span is closed and not yet read; and pictures
    /// taken, of which one in [`TIMED_EVERY`] is timed.
    timer: Option<Timer>,
    timing: bool,
    taken: u32,
}

impl Backend<'_> {
    /// Copy pictures into textures made on `device`, whose `fence` is
    /// signalled behind each picture's copies. The device's immediate
    /// context is used on the thread that drives this, as the runtime's
    /// context is.
    pub fn attach_textures(&mut self, device: Arc<Device>, fence: Arc<Fence>) {
        let timer = device.timer().ok();
        self.textures = Some(Textures {
            device,
            fence,
            value: 0,
            timer,
            timing: false,
            taken: 0,
        });
    }

    /// Read the last timed picture's span, once the device has passed it,
    /// into the decode's figure, and open the next at once if this picture
    /// is one timed and it may be: not while the last is still unread, since
    /// the timer holds one span. Whether it was opened.
    fn open_span(&mut self) -> bool {
        let Some(textures) = self.textures.as_mut() else {
            return false;
        };
        let Some(timer) = textures.timer.as_ref() else {
            return false;
        };
        if textures.timing {
            match textures.device.span(timer) {
                Ok(Span::Pending) => return false,
                Ok(Span::Micros(us)) => self.decode_us = us,
                Ok(Span::Unsteady) | Err(_) => {}
            }
            textures.timing = false;
        }
        let due = textures.taken % TIMED_EVERY == 0;
        textures.taken = textures.taken.wrapping_add(1);
        if due {
            textures.device.begin_span(timer);
        }
        due
    }

    /// Whether pictures can be copied into textures: a device attached, and
    /// a runtime that writes them.
    pub fn exports_textures(&self) -> bool {
        self.textures.is_some() && self.cuda.has_graphics()
    }

    /// Whether the textures' device is gone, which no decode reports.
    pub fn textures_lost(&self) -> bool {
        self.textures.as_ref().is_some_and(|t| t.device.lost())
    }

    /// As [`crate::Decoder::take`], into `planes`: the textures lent for the
    /// picture, one per plane its layout has, each made at the picture's
    /// size on the attached device and registered with this runtime. With
    /// the picture, the value the fence reaches once it is there.
    pub fn take_to_textures(
        &mut self,
        planes: [Option<&Registered>; 3],
    ) -> core::result::Result<Option<(Picture, u64)>, Fault> {
        let taken = self.take_with(|this, slot| this.copy_to_textures(slot, planes))?;
        let value = self.textures.as_ref().map_or(0, |t| t.value);
        Ok(taken.map(|picture| (picture, value)))
    }

    /// Copy `slot`'s picture into `planes` on the stream, then signal the
    /// fence's next value behind the copies. Nothing is waited for: what
    /// this thread spends is the submit, and the decode's figure is the
    /// textures' device's own time for the last picture, from its take to
    /// the fence passing.
    fn copy_to_textures(&mut self, slot: usize, planes: [Option<&Registered>; 3]) -> Result<()> {
        let started = lowlat_common::clock::Time::now();
        let timed = self.open_span();
        self.settle()?;
        self.ensure_stream()?;
        let shape = self.shape.ok_or(Error::NoProfile)?;
        let format = shape.format();
        let (width, height, _) = self.output().ok_or(Error::NoProfile)?;
        let width = usize::try_from(width).map_err(|_| Error::TooLarge)?;
        let height = usize::try_from(height).map_err(|_| Error::TooLarge)?;
        // Bytes a row and rows per plane, as the textures are made: the
        // luma, then the chroma interleaved at half size or two planes of it
        // at full size.
        let sample = format.sample();
        let (count, extents) = if format.full_chroma() {
            (3, [(width * sample, height); 3])
        } else {
            let chroma = (width.div_ceil(2) * 2 * sample, format.chroma_rows(height));
            (2, [(width * sample, height), chroma, (0, 0)])
        };
        let [y, uv, v] = planes;
        let (Some(y), Some(uv)) = (y, uv) else {
            return Err(Error::TooLarge);
        };
        if v.is_some() != (count == 3) {
            return Err(Error::TooLarge);
        }
        let targets = [y, uv, v.unwrap_or(y)];
        let targets = targets.get(..count).ok_or(Error::TooLarge)?;
        let cuda = self.cuda;
        let (Some((stream, copied)), Some(decoder)) = (self.stream.as_ref(), self.decoder.as_ref())
        else {
            return Err(Error::NoProfile);
        };

        let mut copies: [CUDA_MEMCPY2D; 3] = [zeroed(); 3];
        for (copy, (bytes, rows)) in copies.iter_mut().zip(extents) {
            copy.WidthInBytes = bytes;
            copy.Height = rows;
        }
        // Where the planes are read: the backend's own surface, behind its
        // decode on the stream; or the decoder's picture, mapped on the
        // stream -- which waits for the decode -- and left mapped behind the
        // copies.
        let mut mapped_picture = None;
        let mut synced = started;
        match &self.registered {
            Some(registered) => {
                let surface = registered.planes.get(slot).ok_or(Error::TooLarge)?;
                for (copy, plane) in copies.iter_mut().zip(surface) {
                    if copy.WidthInBytes > plane.width * plane.element || copy.Height > plane.height
                    {
                        return Err(Error::TooLarge);
                    }
                    copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
                    copy.srcArray = plane.raw;
                }
            }
            None => {
                let mut proc_params: CUVIDPROCPARAMS = zeroed();
                proc_params.progressive_frame = 1;
                proc_params.output_stream = stream.raw();
                let picture = c_int::try_from(slot).map_err(|_| Error::TooLarge)?;
                let (ptr, pitch) = decoder.map(picture, &mut proc_params)?;
                synced = lowlat_common::clock::Time::now();
                mapped_picture = Some(ptr);
                // The planes lie a coded height apart; only the visible rows
                // are read.
                let coded_height = usize::try_from(shape.height).unwrap_or(0);
                let plane = u64::try_from(pitch * coded_height).unwrap_or(0);
                let fits = height <= coded_height && extents.iter().all(|(b, _)| *b <= pitch);
                for (copy, at) in copies.iter_mut().zip([ptr, ptr + plane, ptr + 2 * plane]) {
                    copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
                    copy.srcDevice = at;
                    copy.srcPitch = pitch;
                }
                if !fits {
                    let _ = decoder.unmap(ptr);
                    return Err(Error::TooLarge);
                }
            }
        }

        let written = (|| -> Result<()> {
            let mapped = cuda.map(targets, stream)?;
            let mut queued = Ok(());
            for (copy, array) in copies.iter_mut().zip(mapped.arrays).take(count) {
                copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
                copy.dstArray = array;
                // SAFETY: the source is the backend's surface or the mapped
                // picture, each covering the rows and bytes checked above and
                // kept until the stream has passed the copy; the destination
                // is a texture made for this picture's plane, mapped until
                // the unmap below, queued behind it on the same stream.
                queued = unsafe { cuda.copy_2d_async(copy, stream) }.map_err(Error::from);
                if queued.is_err() {
                    break;
                }
            }
            let unmapped = cuda.unmap(mapped, stream).map_err(Error::from);
            queued?;
            unmapped
        })();
        // A mapped picture is unmapped once its copies have passed: the
        // next time anything maps, when they long have.
        let left = mapped_picture.filter(|ptr| {
            let recorded = copied.record(stream).is_ok();
            if !recorded {
                let _ = decoder.unmap(*ptr);
            }
            recorded
        });
        self.left_mapped = left;
        written?;

        let textures = self.textures.as_mut().ok_or(Error::NoProfile)?;
        let value = textures.value + 1;
        let timer = textures.timer.as_ref().filter(|_| timed);
        match timer {
            Some(timer) => textures.device.signal_timed(&textures.fence, value, timer),
            None => textures.device.signal(&textures.fence, value),
        }
        .map_err(|e| match e {
            lowlat_drivers::d3d11::Error::Status(hr) => {
                Error::Status(u32::from_ne_bytes(hr.to_ne_bytes()))
            }
            _ => Error::NoProfile,
        })?;
        textures.value = value;
        textures.timing |= timer.is_some();
        let done = lowlat_common::clock::Time::now();
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        Ok(())
    }
}
