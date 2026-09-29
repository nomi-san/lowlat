//! The application's side of a picture handed out as textures, for the
//! tests that read one back.

use lowlat_drivers::d3d11::{Com, Device};
use lowlat_drivers::ffi::d3d11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Resource, ID3D11Texture2D,
};
use lowlat_drivers::vcall;

/// The application's side of a picture handed out as textures: a device of
/// its own on the same adapter, opening each plane by its handle and reading
/// it back through a staging texture.
pub struct Reader {
    pub device: Device,
    /// Each plane opened, by handle, with its staging texture.
    pub opened: Vec<(u64, Com<ID3D11Texture2D>, Com<ID3D11Texture2D>)>,
}

impl Reader {
    /// `rows` rows of `row_bytes` of the plane behind `handle` into `out`
    /// at `pitch`.
    pub fn read(
        &mut self,
        handle: u64,
        rows: usize,
        row_bytes: usize,
        out: &mut [u8],
        pitch: usize,
    ) {
        if !self.opened.iter().any(|(h, _, _)| *h == handle) {
            let texture = self.device.open_shared(handle).expect("the plane opened");
            // SAFETY: plain data the call fills whole.
            let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
            // SAFETY: a live texture; the output is a live local.
            unsafe { vcall!(texture.as_ptr(), GetDesc, &raw mut desc) };
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ as u32;
            desc.MiscFlags = 0;
            let mut raw: *mut ID3D11Texture2D = core::ptr::null_mut();
            // SAFETY: a live device; the description and output are live.
            let hr = unsafe {
                vcall!(
                    self.device.device(),
                    CreateTexture2D,
                    &raw const desc,
                    core::ptr::null(),
                    &raw mut raw
                )
            }
            .expect("the entry");
            assert!(hr >= 0, "staging: 0x{hr:08x}");
            // SAFETY: a texture whose reference the call handed over.
            let staging = unsafe { Com::from_raw(raw) }.expect("staging");
            self.opened.push((handle, texture, staging));
        }
        let (_, texture, staging) = self
            .opened
            .iter()
            .find(|(h, _, _)| *h == handle)
            .expect("opened");
        let context = self.device.context();
        let staging = staging.as_ptr().cast::<ID3D11Resource>();
        // SAFETY: a live context on this thread; both are this device's, of
        // one format and size.
        unsafe {
            vcall!(
                context,
                CopyResource,
                staging,
                texture.as_ptr().cast::<ID3D11Resource>()
            )
        };
        // SAFETY: plain data the call fills.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
        // SAFETY: as above; the output is live.
        let hr = unsafe { vcall!(context, Map, staging, 0, D3D11_MAP_READ, 0, &raw mut mapped) }
            .expect("the entry");
        assert!(hr >= 0, "map: 0x{hr:08x}");
        for row in 0..rows {
            // SAFETY: the mapping holds `RowPitch` bytes a row for every row
            // of the plane, and `row_bytes` is inside one.
            let from = unsafe {
                core::slice::from_raw_parts(
                    mapped
                        .pData
                        .cast::<u8>()
                        .add(row * mapped.RowPitch as usize),
                    row_bytes,
                )
            };
            out[row * pitch..row * pitch + row_bytes].copy_from_slice(from);
        }
        // SAFETY: mapped above, unmapped once.
        unsafe { vcall!(context, Unmap, staging, 0) };
    }
}
