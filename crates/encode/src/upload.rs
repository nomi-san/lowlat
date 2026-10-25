//! A frame from ordinary memory written into a texture of an encoder's D3D11
//! device: how the vendor encoders on Windows take the generator's frames.

use lowlat_drivers::d3d11::{Com, Device, Error};
use lowlat_drivers::vcall;

use crate::ffi::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_STAGING, DXGI_FORMAT, DXGI_SAMPLE_DESC,
    ID3D11Resource, ID3D11Texture2D,
};

/// A texture `width` x `height` in `format`, as `usage` makes it: one the
/// encoder reads, or one the processor writes.
pub(crate) fn describe(
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
    usage: D3D11_USAGE,
) -> D3D11_TEXTURE2D_DESC {
    let (bind, cpu) = if usage == D3D11_USAGE_STAGING {
        (0, D3D11_CPU_ACCESS_WRITE)
    } else {
        (D3D11_BIND_RENDER_TARGET, 0)
    };
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: u32::try_from(bind).unwrap_or(0),
        CPUAccessFlags: u32::try_from(cpu).unwrap_or(0),
        MiscFlags: 0,
    }
}

/// Write a frame from ordinary memory into `texture`, two planes `width` x
/// `height`, through `staging`: the staging texture mapped and written row by
/// row -- the luma rows at the start, `RowPitch` apart, the chroma rows
/// directly after the last of them -- then copied whole into `texture`, never
/// into one plane of it, which drops the colour on some drivers. At ten bits
/// each sample is widened as it is written, high in its word.
pub(crate) fn upload(
    device: &Device,
    staging: &Com<ID3D11Texture2D>,
    texture: &Com<ID3D11Texture2D>,
    frame: &lowlat_capture::Frame<'_>,
    (width, height): (u32, u32),
    ten: bool,
) -> Result<(), Error> {
    let columns = usize::try_from(frame.width.min(width)).unwrap_or(0);
    let rows = usize::try_from(frame.height.min(height)).unwrap_or(0);
    let height = usize::try_from(height).unwrap_or(0);
    let context = device.context();
    let staging = staging.as_ptr().cast::<ID3D11Resource>();
    // SAFETY: plain data the call fills.
    let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
    // SAFETY: the device's context, on the thread that drives the encoder; a
    // live staging texture the processor may write; a live output.
    let hr = unsafe {
        vcall!(
            context,
            Map,
            staging,
            0,
            D3D11_MAP_WRITE,
            0,
            &raw mut mapped
        )
    }
    .unwrap_or(-1);
    if hr < 0 {
        return Err(Error::Status(hr));
    }
    let pitch = usize::try_from(mapped.RowPitch).unwrap_or(0);
    let base = mapped.pData.cast::<u8>();
    let sample = if ten { 2 } else { 1 };
    let chroma_width = columns.div_ceil(2) * 2;
    for (plane, first, count, samples) in [
        (&frame.luma, 0, rows, columns),
        (&frame.chroma, height, rows.div_ceil(2), chroma_width),
    ] {
        let samples = samples.min(pitch / sample);
        for row in 0..count {
            let Some(from) = plane.row(row).and_then(|full| full.get(..samples)) else {
                break;
            };
            // SAFETY: the mapping holds the texture's `height` luma rows and
            // half as many chroma rows after them, `pitch` bytes each; the row
            // written is one of those and the samples written fit its pitch.
            let to = unsafe { base.add((first + row) * pitch) };
            if ten {
                for (index, &value) in from.iter().enumerate() {
                    let value = u16::from(value);
                    let word = ((value << 2) | (value >> 6)) << 6;
                    // SAFETY: as above; two bytes a sample within the row.
                    unsafe {
                        to.add(index * 2)
                            .cast::<[u8; 2]>()
                            .write_unaligned(word.to_le_bytes());
                    }
                }
            } else {
                // SAFETY: as above; the source and the mapping do not overlap.
                unsafe { core::ptr::copy_nonoverlapping(from.as_ptr(), to, from.len()) };
            }
        }
    }
    // SAFETY: the mapping made above, given back; the copy reads it into the
    // caller's texture, which no picture in flight is reading.
    unsafe {
        vcall!(context, Unmap, staging, 0);
        vcall!(
            context,
            CopyResource,
            texture.as_ptr().cast::<ID3D11Resource>(),
            staging
        );
    }
    Ok(())
}
