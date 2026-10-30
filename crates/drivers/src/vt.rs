//! The system's video decoder and the frameworks beneath it, reached at run
//! time on macOS alone.
//!
//! **Opened by path, never linked, and kept for the process.** The frameworks
//! live in the system's shared cache rather than in files of their own, and the
//! loader opens them by their framework paths all the same. Their entry points,
//! and the constant objects the calls take as keys and callbacks, are resolved
//! into one table the first time a decoder is asked for, so a program that
//! links the library names no framework and one that never decodes loads none.
//!
//! The rest is ownership: an object the frameworks hand over with a reference
//! is given it back when its holder drops, and a decoded picture's buffer is
//! held, locked for reading, for as long as anything reads its planes -- or
//! held alone while another device reads its surface.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::ptr::NonNull;
use std::sync::OnceLock;

use lowlat_common::dynlib::Library;

use crate::ffi::vt::{
    __CVBuffer, CFAllocatorRef, CFArrayAppendValue, CFArrayCallBacks, CFArrayCreateMutable,
    CFBooleanGetValue, CFBooleanRef, CFDictionaryCreateMutable, CFDictionaryKeyCallBacks,
    CFDictionarySetValue, CFDictionaryValueCallBacks, CFNumberCreate, CFRelease, CFRetain,
    CFStringRef, CMBlockBufferCreateWithMemoryBlock, CMSampleBufferCreateReady,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferRef, CVPixelBufferRelease,
    CVPixelBufferRetain, CVPixelBufferUnlockBaseAddress, IOSurfaceGetAllocSize, IOSurfaceGetID,
    OSType, VTDecompressionSessionCanAcceptFormatDescription, VTDecompressionSessionCreate,
    VTDecompressionSessionDecodeFrame, VTDecompressionSessionInvalidate,
    VTDecompressionSessionWaitForAsynchronousFrames, VTSessionCopyProperty,
    kCVPixelBufferLock_ReadOnly, kCVReturnSuccess,
};

const CORE_FOUNDATION: &CStr =
    c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
const CORE_MEDIA: &CStr = c"/System/Library/Frameworks/CoreMedia.framework/CoreMedia";
const CORE_VIDEO: &CStr = c"/System/Library/Frameworks/CoreVideo.framework/CoreVideo";
const VIDEO_TOOLBOX: &CStr = c"/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox";
const IO_SURFACE: &CStr = c"/System/Library/Frameworks/IOSurface.framework/IOSurface";

/// Why the frameworks could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A framework did not open.
    Unavailable,
    /// Opened, but missing an entry point or a constant it must have.
    MissingSymbol,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("system video frameworks not available"),
            Self::MissingSymbol => f.write_str("system video frameworks missing an entry point"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

/// The frameworks' entry points and constants, resolved once.
pub struct Vt {
    pub retain: CFRetain,
    pub release: CFRelease,
    pub dictionary_create_mutable: CFDictionaryCreateMutable,
    pub dictionary_set_value: CFDictionarySetValue,
    pub array_create_mutable: CFArrayCreateMutable,
    pub array_append_value: CFArrayAppendValue,
    pub number_create: CFNumberCreate,
    pub boolean_get_value: CFBooleanGetValue,
    pub format_from_h264: CMVideoFormatDescriptionCreateFromH264ParameterSets,
    pub format_from_hevc: CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    pub block_buffer_with_memory: CMBlockBufferCreateWithMemoryBlock,
    pub sample_buffer_ready: CMSampleBufferCreateReady,
    pub buffer_format: CVPixelBufferGetPixelFormatType,
    pub buffer_width: CVPixelBufferGetWidth,
    pub buffer_height: CVPixelBufferGetHeight,
    pub buffer_planes: CVPixelBufferGetPlaneCount,
    pub plane_pitch: CVPixelBufferGetBytesPerRowOfPlane,
    pub plane_base: CVPixelBufferGetBaseAddressOfPlane,
    pub plane_rows: CVPixelBufferGetHeightOfPlane,
    pub buffer_lock: CVPixelBufferLockBaseAddress,
    pub buffer_unlock: CVPixelBufferUnlockBaseAddress,
    pub buffer_retain: CVPixelBufferRetain,
    pub buffer_release: CVPixelBufferRelease,
    /// A buffer's surface, and the surface's identity and size.
    pub buffer_surface: CVPixelBufferGetIOSurface,
    pub surface_id: IOSurfaceGetID,
    pub surface_size: IOSurfaceGetAllocSize,
    pub session_create: VTDecompressionSessionCreate,
    pub session_decode: VTDecompressionSessionDecodeFrame,
    pub session_wait: VTDecompressionSessionWaitForAsynchronousFrames,
    pub session_invalidate: VTDecompressionSessionInvalidate,
    pub session_accepts: VTDecompressionSessionCanAcceptFormatDescription,
    pub session_property: VTSessionCopyProperty,
    /// The callbacks a dictionary or an array of framework objects is made
    /// with, which retain what is put in and release it with the container.
    pub dictionary_keys: *const CFDictionaryKeyCallBacks,
    pub dictionary_values: *const CFDictionaryValueCallBacks,
    pub array_values: *const CFArrayCallBacks,
    pub boolean_true: CFBooleanRef,
    pub boolean_false: CFBooleanRef,
    /// An allocator that frees nothing: memory handed to a block buffer that
    /// stays this side's.
    pub allocator_null: CFAllocatorRef,
    /// The keys of a session's destination: the layouts it may hand out, and
    /// the surface every buffer is backed by.
    pub pixel_format_key: CFStringRef,
    pub surface_properties_key: CFStringRef,
    /// The keys of a decoder's choice: the hardware required, allowed or
    /// refused, and whether a session's decoder is the hardware's.
    pub require_hardware_key: CFStringRef,
    pub enable_hardware_key: CFStringRef,
    pub using_hardware_key: CFStringRef,
    /// Last, so the frameworks outlive every address taken from them.
    _libraries: [Library; 5],
}

// SAFETY: the entry points are code addresses, and the constants are objects
// the frameworks create once and never change, which their own calls take from
// any thread.
unsafe impl Send for Vt {}
// SAFETY: as above; nothing in the table is written after it is resolved.
unsafe impl Sync for Vt {}

impl fmt::Debug for Vt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vt")
    }
}

static VT: OnceLock<Result<Vt>> = OnceLock::new();

/// The frameworks, opened and resolved the first time they are asked for and
/// kept for the process; why not, where they cannot be.
pub fn load() -> Result<&'static Vt> {
    VT.get_or_init(Vt::open).as_ref().map_err(|e| *e)
}

/// A constant object of a framework's, read through the address of the
/// variable that holds it.
///
/// # Safety
///
/// `name` is a variable of type `T` the library exports.
unsafe fn constant<T: Copy>(library: &Library, name: &CStr) -> Option<T> {
    // SAFETY: the symbol is the variable's address; its type is the caller's
    // contract.
    let at = unsafe { library.symbol::<*const T>(name) }?;
    // SAFETY: a variable the framework initialised before it could be found.
    Some(unsafe { at.read() })
}

impl Vt {
    fn open() -> Result<Self> {
        let open = |path| Library::open(path).ok_or(Error::Unavailable);
        let (cf, cm, cv, vt, io) = (
            open(CORE_FOUNDATION)?,
            open(CORE_MEDIA)?,
            open(CORE_VIDEO)?,
            open(VIDEO_TOOLBOX)?,
            open(IO_SURFACE)?,
        );
        let missing = Error::MissingSymbol;
        // SAFETY: each type is the SDK's declaration of the entry point of that
        // name, generated from its header; each constant's type is the
        // header's declaration of the variable.
        unsafe {
            Ok(Self {
                retain: cf.symbol(c"CFRetain").ok_or(missing)?,
                release: cf.symbol(c"CFRelease").ok_or(missing)?,
                dictionary_create_mutable: cf
                    .symbol(c"CFDictionaryCreateMutable")
                    .ok_or(missing)?,
                dictionary_set_value: cf.symbol(c"CFDictionarySetValue").ok_or(missing)?,
                array_create_mutable: cf.symbol(c"CFArrayCreateMutable").ok_or(missing)?,
                array_append_value: cf.symbol(c"CFArrayAppendValue").ok_or(missing)?,
                number_create: cf.symbol(c"CFNumberCreate").ok_or(missing)?,
                boolean_get_value: cf.symbol(c"CFBooleanGetValue").ok_or(missing)?,
                format_from_h264: cm
                    .symbol(c"CMVideoFormatDescriptionCreateFromH264ParameterSets")
                    .ok_or(missing)?,
                format_from_hevc: cm
                    .symbol(c"CMVideoFormatDescriptionCreateFromHEVCParameterSets")
                    .ok_or(missing)?,
                block_buffer_with_memory: cm
                    .symbol(c"CMBlockBufferCreateWithMemoryBlock")
                    .ok_or(missing)?,
                sample_buffer_ready: cm.symbol(c"CMSampleBufferCreateReady").ok_or(missing)?,
                buffer_format: cv
                    .symbol(c"CVPixelBufferGetPixelFormatType")
                    .ok_or(missing)?,
                buffer_width: cv.symbol(c"CVPixelBufferGetWidth").ok_or(missing)?,
                buffer_height: cv.symbol(c"CVPixelBufferGetHeight").ok_or(missing)?,
                buffer_planes: cv.symbol(c"CVPixelBufferGetPlaneCount").ok_or(missing)?,
                plane_pitch: cv
                    .symbol(c"CVPixelBufferGetBytesPerRowOfPlane")
                    .ok_or(missing)?,
                plane_base: cv
                    .symbol(c"CVPixelBufferGetBaseAddressOfPlane")
                    .ok_or(missing)?,
                plane_rows: cv.symbol(c"CVPixelBufferGetHeightOfPlane").ok_or(missing)?,
                buffer_lock: cv.symbol(c"CVPixelBufferLockBaseAddress").ok_or(missing)?,
                buffer_unlock: cv
                    .symbol(c"CVPixelBufferUnlockBaseAddress")
                    .ok_or(missing)?,
                buffer_retain: cv.symbol(c"CVPixelBufferRetain").ok_or(missing)?,
                buffer_release: cv.symbol(c"CVPixelBufferRelease").ok_or(missing)?,
                buffer_surface: cv.symbol(c"CVPixelBufferGetIOSurface").ok_or(missing)?,
                surface_id: io.symbol(c"IOSurfaceGetID").ok_or(missing)?,
                surface_size: io.symbol(c"IOSurfaceGetAllocSize").ok_or(missing)?,
                session_create: vt.symbol(c"VTDecompressionSessionCreate").ok_or(missing)?,
                session_decode: vt
                    .symbol(c"VTDecompressionSessionDecodeFrame")
                    .ok_or(missing)?,
                session_wait: vt
                    .symbol(c"VTDecompressionSessionWaitForAsynchronousFrames")
                    .ok_or(missing)?,
                session_invalidate: vt
                    .symbol(c"VTDecompressionSessionInvalidate")
                    .ok_or(missing)?,
                session_accepts: vt
                    .symbol(c"VTDecompressionSessionCanAcceptFormatDescription")
                    .ok_or(missing)?,
                session_property: vt.symbol(c"VTSessionCopyProperty").ok_or(missing)?,
                dictionary_keys: cf.symbol(c"kCFTypeDictionaryKeyCallBacks").ok_or(missing)?,
                dictionary_values: cf
                    .symbol(c"kCFTypeDictionaryValueCallBacks")
                    .ok_or(missing)?,
                array_values: cf.symbol(c"kCFTypeArrayCallBacks").ok_or(missing)?,
                boolean_true: constant(&cf, c"kCFBooleanTrue").ok_or(missing)?,
                boolean_false: constant(&cf, c"kCFBooleanFalse").ok_or(missing)?,
                allocator_null: constant(&cf, c"kCFAllocatorNull").ok_or(missing)?,
                pixel_format_key: constant(&cv, c"kCVPixelBufferPixelFormatTypeKey")
                    .ok_or(missing)?,
                surface_properties_key: constant(&cv, c"kCVPixelBufferIOSurfacePropertiesKey")
                    .ok_or(missing)?,
                require_hardware_key: constant(
                    &vt,
                    c"kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder",
                )
                .ok_or(missing)?,
                enable_hardware_key: constant(
                    &vt,
                    c"kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder",
                )
                .ok_or(missing)?,
                using_hardware_key: constant(
                    &vt,
                    c"kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder",
                )
                .ok_or(missing)?,
                _libraries: [cf, cm, cv, vt, io],
            })
        }
    }

    /// Take ownership of an object a call handed over with a reference.
    ///
    /// # Safety
    ///
    /// `object` is null or a framework object whose reference the caller gives
    /// up to the holder.
    pub unsafe fn own(&'static self, object: *const c_void) -> Option<Object> {
        NonNull::new(object.cast_mut()).map(|object| Object { object, vt: self })
    }
}

/// A framework object held by a reference of its own, given back when the
/// holder drops.
pub struct Object {
    object: NonNull<c_void>,
    vt: &'static Vt,
}

impl Object {
    pub fn as_ptr(&self) -> *const c_void {
        self.object.as_ptr()
    }
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Object")
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: the holder's reference, given back once.
        unsafe { (self.vt.release)(self.object.as_ptr()) };
    }
}

/// A decoded picture's buffer, held by a reference of its own.
pub struct PixelBuffer {
    buffer: NonNull<__CVBuffer>,
    vt: &'static Vt,
}

// SAFETY: a buffer's reference count is the framework's own and is taken and
// given back from any thread; nothing here writes the buffer.
unsafe impl Send for PixelBuffer {}

impl fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PixelBuffer")
            .field("format", &self.format())
            .finish()
    }
}

impl PixelBuffer {
    /// A reference of this holder's own to `buffer`.
    ///
    /// # Safety
    ///
    /// `buffer` is null or a live buffer, kept alive by another reference for
    /// the length of the call.
    pub unsafe fn retain(vt: &'static Vt, buffer: CVPixelBufferRef) -> Option<Self> {
        NonNull::new(buffer)?;
        // SAFETY: a live buffer, the caller's contract; the count is ours now.
        let held = unsafe { (vt.buffer_retain)(buffer) };
        NonNull::new(held).map(|buffer| Self { buffer, vt })
    }

    /// Hold `buffer` by the reference its giver took for this holder.
    ///
    /// # Safety
    ///
    /// `buffer` is null or a live buffer whose reference the caller hands
    /// over.
    pub unsafe fn adopt(vt: &'static Vt, buffer: CVPixelBufferRef) -> Option<Self> {
        NonNull::new(buffer).map(|buffer| Self { buffer, vt })
    }

    /// The layout, as the framework's four characters name it.
    pub fn format(&self) -> OSType {
        // SAFETY: a live buffer this holder keeps alive.
        unsafe { (self.vt.buffer_format)(self.buffer.as_ptr()) }
    }

    /// The picture's width and height, which is the visible picture.
    pub fn size(&self) -> (usize, usize) {
        // SAFETY: as above.
        unsafe {
            (
                (self.vt.buffer_width)(self.buffer.as_ptr()),
                (self.vt.buffer_height)(self.buffer.as_ptr()),
            )
        }
    }

    /// Its planes, zero for a layout of one.
    pub fn planes(&self) -> usize {
        // SAFETY: as above.
        unsafe { (self.vt.buffer_planes)(self.buffer.as_ptr()) }
    }

    /// Plane `index`'s bytes from one row to the next, which reading needs no
    /// lock for; none for a plane the layout lacks.
    pub fn pitch(&self, index: usize) -> Option<usize> {
        // SAFETY: as above, and a plane the layout has.
        (index < self.planes())
            .then(|| unsafe { (self.vt.plane_pitch)(self.buffer.as_ptr(), index) })
    }

    /// The surface behind the buffer, valid for as long as the buffer is held;
    /// none for a buffer that has none.
    pub fn surface(&self) -> Option<Surface> {
        // SAFETY: a live buffer; the surface is the buffer's own, not given.
        let surface = unsafe { (self.vt.buffer_surface)(self.buffer.as_ptr()) };
        if surface.is_null() {
            return None;
        }
        // SAFETY: a live surface, which the buffer keeps.
        let (id, size) = unsafe {
            (
                (self.vt.surface_id)(surface),
                (self.vt.surface_size)(surface),
            )
        };
        Some(Surface {
            surface: surface.cast_const().cast(),
            id,
            size,
        })
    }

    /// Locked for reading: the planes' addresses are valid from here until the
    /// lock drops. The buffer back where the framework refused.
    pub fn lock(self) -> core::result::Result<Locked, Self> {
        // SAFETY: a live buffer; the flag asks for reading alone.
        let locked = unsafe {
            (self.vt.buffer_lock)(self.buffer.as_ptr(), u64::from(kCVPixelBufferLock_ReadOnly))
        };
        if locked == kCVReturnSuccess {
            Ok(Locked { buffer: self })
        } else {
            Err(self)
        }
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: the holder's reference, given back once.
        unsafe { (self.vt.buffer_release)(self.buffer.as_ptr()) };
    }
}

/// The surface behind a buffer: its address, its identifier, unique among the
/// surfaces alive, and its whole allocation in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub surface: *const c_void,
    pub id: u32,
    pub size: usize,
}

/// A buffer locked for reading, its planes where they lie.
#[derive(Debug)]
pub struct Locked {
    buffer: PixelBuffer,
}

impl Locked {
    pub fn buffer(&self) -> &PixelBuffer {
        &self.buffer
    }

    /// Plane `index`: the first sample of its first row, the bytes from one
    /// row to the next and its rows; none for a plane the layout lacks.
    pub fn plane(&self, index: usize) -> Option<(*const u8, usize, usize)> {
        if index >= self.buffer.planes() {
            return None;
        }
        let at = self.buffer.buffer.as_ptr();
        let vt = self.buffer.vt;
        // SAFETY: a live buffer, locked, and a plane it has.
        let (base, pitch, rows) = unsafe {
            (
                (vt.plane_base)(at, index),
                (vt.plane_pitch)(at, index),
                (vt.plane_rows)(at, index),
            )
        };
        (!base.is_null()).then_some((base.cast_const().cast::<u8>(), pitch, rows))
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: locked for reading when this was made, unlocked once; the
        // buffer's reference goes after it.
        unsafe {
            (self.buffer.vt.buffer_unlock)(
                self.buffer.buffer.as_ptr(),
                u64::from(kCVPixelBufferLock_ReadOnly),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every name the table needs resolves**, entry points and constants
    /// alike, from the five frameworks opened by path: a name the system does
    /// not export fails the load whole rather than leaving something to call
    /// that is not there. A constant read through the table is the
    /// framework's own object.
    #[test]
    fn every_name_resolves() {
        let vt = load().expect("the system's video frameworks");
        // SAFETY: the framework's own two values.
        unsafe {
            assert_eq!((vt.boolean_get_value)(vt.boolean_true), 1);
            assert_eq!((vt.boolean_get_value)(vt.boolean_false), 0);
        }
        for key in [
            vt.pixel_format_key,
            vt.surface_properties_key,
            vt.require_hardware_key,
            vt.enable_hardware_key,
            vt.using_hardware_key,
        ] {
            assert!(!key.is_null());
        }
        assert!(!vt.dictionary_keys.is_null() && !vt.array_values.is_null());
        assert!(!vt.allocator_null.is_null());
    }
}
