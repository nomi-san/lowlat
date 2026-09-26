//! What a surface import descriptor needs beyond the display interface's
//! headers. Written by hand, so a regeneration of the bindings beside it leaves
//! this alone.

use super::va::{
    _VADRMPRIMESurfaceDescriptor__bindgen_ty_1, _VADRMPRIMESurfaceDescriptor__bindgen_ty_2,
};

/// One object a descriptor names.
///
/// **The fd is borrowed for the call and closed by the caller.** The runtime
/// duplicates what it needs.
pub type VADRMPRIMESurfaceDescriptorObject = _VADRMPRIMESurfaceDescriptor__bindgen_ty_1;

/// One layer a descriptor names.
pub type VADRMPRIMESurfaceDescriptorLayer = _VADRMPRIMESurfaceDescriptor__bindgen_ty_2;

/// Objects, layers and planes a descriptor can name.
///
/// The header fixes all three at four. A frame handed over here is one object
/// with one layer of two planes.
pub const VA_DRM_PRIME_OBJECTS: usize = 4;
pub const VA_DRM_PRIME_LAYERS: usize = 4;
pub const VA_DRM_PRIME_PLANES: usize = 4;

/// The four character code a two-plane eight-bit frame is named by, on the
/// display interface rather than the runtime's own.
pub const DRM_FORMAT_NV12: u32 = 0x3231_564E;
/// Two planes, ten bits a sample in the high bits of sixteen. The same
/// four-character code the colour interface uses for it.
pub const DRM_FORMAT_P010: u32 = 0x3031_3050;
/// One packed word per pixel, alpha in the high byte: the open stack's
/// eight-bit full-chroma surface.
pub const DRM_FORMAT_AYUV: u32 = 0x5655_5941;
/// The packed ten-bit full-chroma layout the driver reads Y410 as:
/// X, red difference, luma, blue difference at 2:10:10:10.
pub const DRM_FORMAT_XVYU2101010: u32 = 0x3033_5658;
