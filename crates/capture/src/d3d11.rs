//! The conversion on D3D11: the duplicated desktop, read through a view of
//! its own, into the encoder's inputs, written through one view per plane --
//! by a compute shader, or on Intel's GPUs by a draw a plane, which costs them
//! a third to a sixth as much for the same samples (docs/05-host.md section
//! 3).
//!
//! **A target is the encoder's input itself**, bound for its writer alone and
//! registered with the encoder once, so nothing is copied between the
//! conversion and the encode; and the conversion is never waited on here,
//! since the encoder's work on the same device is ordered behind it.

use lowlat_drivers::d3d11::{Com, Device, Error};
use lowlat_drivers::ffi::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_UNORDERED_ACCESS, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_FORMAT_Y410, DXGI_SAMPLE_DESC, ID3D11ComputeShader,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11UnorderedAccessView, ID3D11VertexShader,
};
use lowlat_drivers::vcall;

/// The conversion's compiled shaders: per layout the compute pass and the
/// draw's pixel shaders, and the vertex shader every draw shares.
#[path = "d3d11_convert.rs"]
mod code;

/// The vendor number of Intel's GPUs.
const INTEL: u32 = 0x8086;

/// Whether the conversion draws its targets on `device` rather than
/// computing them: **on Intel's GPUs**, where the compute pass's stores into
/// the planes cost three to five times what a draw a plane does, for the same
/// samples (measured). Elsewhere the compute pass costs as little or less,
/// and some layouts cannot be drawn into at all.
fn draws(device: &Device) -> bool {
    device.adapter.vendor == INTEL
}

/// The layout the conversion writes, which is the one the encoder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Two planes at eight bits, the chroma interleaved at half resolution.
    Nv12,
    /// The same at ten bits, each sample in the high bits of sixteen.
    P010,
    /// One packed word a pixel at eight bits, chroma at full resolution.
    Ayuv,
    /// The same at ten bits.
    Y410,
}

impl Layout {
    /// The layout for a depth and a chroma.
    pub const fn of(ten_bit: bool, full_chroma: bool) -> Self {
        match (ten_bit, full_chroma) {
            (false, false) => Self::Nv12,
            (true, false) => Self::P010,
            (false, true) => Self::Ayuv,
            (true, true) => Self::Y410,
        }
    }

    /// The format of a target's texture.
    pub const fn format(self) -> DXGI_FORMAT {
        match self {
            Self::Nv12 => DXGI_FORMAT_NV12,
            Self::P010 => DXGI_FORMAT_P010,
            Self::Ayuv => DXGI_FORMAT_AYUV,
            Self::Y410 => DXGI_FORMAT_Y410,
        }
    }

    /// The views the shader writes a target through, the second absent for
    /// a packed layout. **On a two-plane texture the view's format names the
    /// plane.**
    const fn views(self) -> [DXGI_FORMAT; 2] {
        match self {
            Self::Nv12 => [DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM],
            Self::P010 => [DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM],
            Self::Ayuv => [DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN],
            Self::Y410 => [DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_UNKNOWN],
        }
    }

    pub const fn full_chroma(self) -> bool {
        matches!(self, Self::Ayuv | Self::Y410)
    }

    const fn code(self) -> &'static [u8] {
        match self {
            Self::Nv12 => code::NV12,
            Self::P010 => code::P010,
            Self::Ayuv => code::AYUV,
            Self::Y410 => code::Y410,
        }
    }

    /// The pixel shaders that draw a target plane by plane, the second absent
    /// for a packed layout.
    const fn drawn(self) -> [Option<&'static [u8]>; 2] {
        match self {
            Self::Nv12 => [Some(code::NV12_LUMA), Some(code::NV12_CHROMA)],
            Self::P010 => [Some(code::P010_LUMA), Some(code::P010_CHROMA)],
            Self::Ayuv => [Some(code::AYUV_PIXEL), None],
            Self::Y410 => [Some(code::Y410_PIXEL), None],
        }
    }
}

/// The views a target's planes are written through, the second absent for a
/// packed layout: by the compute pass, or by the draws.
enum Views {
    Unordered([Option<Com<ID3D11UnorderedAccessView>>; 2]),
    Rendered([Option<Com<ID3D11RenderTargetView>>; 2]),
}

/// One target: a texture the encoder reads, and the views the conversion
/// writes its planes through.
pub struct Target {
    texture: Com<ID3D11Texture2D>,
    views: Views,
    layout: Layout,
    /// The texture's size: the picture's, made whole 2x2 blocks for the
    /// two-plane layouts, which the system allocates no other way.
    width: u32,
    height: u32,
}

impl core::fmt::Debug for Target {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Target")
            .field("layout", &self.layout)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl Target {
    /// A target in `layout` for a `width` x `height` picture, its contents
    /// undefined until converted into.
    pub fn new(device: &Device, layout: Layout, width: u32, height: u32) -> Result<Self, Error> {
        Self::made(device, layout, width, height, draws(device))
    }

    /// A target written by the draws when `drawn`, else by the compute pass.
    fn made(
        device: &Device,
        layout: Layout,
        width: u32,
        height: u32,
        drawn: bool,
    ) -> Result<Self, Error> {
        let (width, height) = if layout.full_chroma() {
            (width, height)
        } else {
            (width.next_multiple_of(2), height.next_multiple_of(2))
        };
        // **Bound for its writer alone**: unordered access for the compute
        // pass, a render target for the draws. The encoders take a texture
        // bound so, measured -- the draws' only Intel's -- and nothing else
        // writes it.
        let bind = if drawn {
            D3D11_BIND_RENDER_TARGET
        } else {
            D3D11_BIND_UNORDERED_ACCESS
        };
        let texture = device.texture(&D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: layout.format(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: u32::try_from(bind).unwrap_or(0),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })?;
        let raw = texture.as_ptr();
        let views = if drawn {
            let mut views = [None, None];
            for (view, format) in views.iter_mut().zip(layout.views()) {
                if format != DXGI_FORMAT_UNKNOWN {
                    *view = Some(device.render_view(raw, format)?);
                }
            }
            Views::Rendered(views)
        } else {
            let mut views = [None, None];
            for (view, format) in views.iter_mut().zip(layout.views()) {
                if format != DXGI_FORMAT_UNKNOWN {
                    *view = Some(device.unordered_view(raw, format)?);
                }
            }
            Views::Unordered(views)
        };
        Ok(Self {
            texture,
            views,
            layout,
            width,
            height,
        })
    }

    /// The texture, for the encoder to register.
    pub fn texture(&self) -> &Com<ID3D11Texture2D> {
        &self.texture
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// The texture's own size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// How the conversion writes its targets.
enum Pass {
    /// One compute run over the picture.
    Compute(Com<ID3D11ComputeShader>),
    /// One triangle a plane.
    Draw {
        vertex: Com<ID3D11VertexShader>,
        planes: [Option<Com<ID3D11PixelShader>>; 2],
    },
}

/// The conversion into one layout, on one device.
pub struct Converter {
    pass: Pass,
    layout: Layout,
    /// The view the last source was read through, and that source's address.
    ///
    /// **Kept while the source is the same texture**, which the duplication
    /// hands over again frame after frame. The address is safe to compare:
    /// the view holds its texture, so no other can be made there while it
    /// is kept.
    source: Option<(usize, Com<ID3D11ShaderResourceView>)>,
}

impl core::fmt::Debug for Converter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converter")
            .field("layout", &self.layout)
            .field("drawn", &matches!(self.pass, Pass::Draw { .. }))
            .field("viewed", &self.source.is_some())
            .finish()
    }
}

impl Converter {
    pub fn new(device: &Device, layout: Layout) -> Result<Self, Error> {
        Self::made(device, layout, draws(device))
    }

    /// The conversion by the draws when `drawn`, else by the compute pass.
    fn made(device: &Device, layout: Layout, drawn: bool) -> Result<Self, Error> {
        let pass = if drawn {
            let mut planes = [None, None];
            for (plane, code) in planes.iter_mut().zip(layout.drawn()) {
                if let Some(code) = code {
                    *plane = Some(device.pixel_shader(code)?);
                }
            }
            Pass::Draw {
                vertex: device.vertex_shader(code::VERTEX)?,
                planes,
            }
        } else {
            Pass::Compute(device.compute_shader(layout.code())?)
        };
        Ok(Self {
            pass,
            layout,
            source: None,
        })
    }

    /// Queue the conversion of `source` into `target`, and hand the queue to
    /// the device so the work starts at once. Returns as soon as it is
    /// queued: the encoder's work on this device is ordered behind it, and so
    /// is the duplication's release of the source.
    pub fn convert(
        &mut self,
        device: &Device,
        source: *mut ID3D11Texture2D,
        target: &Target,
    ) -> Result<(), Error> {
        let key = source as usize;
        let view = match &self.source {
            Some((held, view)) if *held == key => view.as_ptr(),
            _ => {
                self.source = None;
                let view = Self::view_of(device, source)?;
                let raw = view.as_ptr();
                self.source = Some((key, view));
                raw
            }
        };
        self.run(device, view, target)
    }

    /// The view a source texture is read through, at its own format: for a
    /// caller that keeps one per texture, a source that cycles through
    /// several of its own.
    pub fn view_of(
        device: &Device,
        source: *mut ID3D11Texture2D,
    ) -> Result<Com<ID3D11ShaderResourceView>, Error> {
        // SAFETY: plain data the call fills whole.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: the caller's live texture; the output is a live local.
        unsafe { vcall!(source, GetDesc, &raw mut desc) };
        device.shader_view(source, desc.Format)
    }

    /// Queue the conversion of what `view` reads into `target`, as
    /// [`Converter::convert`] does with a view of its own.
    pub fn convert_view(
        &self,
        device: &Device,
        view: &Com<ID3D11ShaderResourceView>,
        target: &Target,
    ) -> Result<(), Error> {
        self.run(device, view.as_ptr(), target)
    }

    fn run(
        &self,
        device: &Device,
        view: *mut ID3D11ShaderResourceView,
        target: &Target,
    ) -> Result<(), Error> {
        if target.layout != self.layout {
            return Err(Error::Unavailable);
        }
        match (&self.pass, &target.views) {
            (Pass::Compute(shader), Views::Unordered(views)) => {
                let targets: [*mut ID3D11UnorderedAccessView; 2] = views
                    .each_ref()
                    .map(|view| view.as_ref().map_or(core::ptr::null_mut(), Com::as_ptr));
                let bound = if self.layout.full_chroma() { 1 } else { 2 };
                // One thread a 2x2 block of the two-plane layouts, one a pixel
                // of the packed ones.
                let (columns, rows) = if self.layout.full_chroma() {
                    (target.width, target.height)
                } else {
                    (target.width / 2, target.height / 2)
                };
                device.dispatch(
                    shader,
                    &[view],
                    targets.get(..bound).unwrap_or_default(),
                    (columns.div_ceil(8), rows.div_ceil(8)),
                )?;
            }
            (Pass::Draw { vertex, planes }, Views::Rendered(views)) => {
                // The luma plane or the packed word at the target's size, the
                // chroma plane at half of it.
                let sizes = [
                    (target.width, target.height),
                    (target.width / 2, target.height / 2),
                ];
                for ((plane, rendered), size) in planes.iter().zip(views).zip(sizes) {
                    if let (Some(plane), Some(rendered)) = (plane, rendered) {
                        device.draw(vertex, plane, &[view], rendered, size)?;
                    }
                }
            }
            // A target made for the other pass, which only another device's
            // conversion would be.
            _ => return Err(Error::Unavailable),
        }
        // SAFETY: the device's context, on the thread that drives it.
        unsafe { vcall!(device.context(), Flush) };
        Ok(())
    }

    /// Let go of the last source's view: what a duplication made again hands
    /// over is another texture, and this one is the old duplication's.
    pub fn forget(&mut self) {
        self.source = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::{PATTERN, reference, reference_ten};
    use lowlat_drivers::d3d11::D3d11;
    use lowlat_drivers::ffi::d3d11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
        DXGI_FORMAT_B8G8R8A8_UNORM, ID3D11Resource,
    };

    /// The pattern as the duplication hands a desktop over: eight-bit BGRA,
    /// each colour a 2x2 block, so subsampling has nothing to average.
    fn source(device: &Device) -> (Com<ID3D11Texture2D>, u32, u32) {
        let width = u32::try_from(PATTERN.len()).expect("a short pattern") * 2;
        let height = 2;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for (block, [r, g, b]) in PATTERN.iter().enumerate() {
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let at = (dy * width as usize + block * 2 + dx) * 4;
                    pixels[at..at + 4].copy_from_slice(&[*b, *g, *r, 0xFF]);
                }
            }
        }
        (bgra(device, width, height, &pixels), width, height)
    }

    /// Noise as the duplication would hand it over, `width` x `height`: every
    /// sample one a form rounding differently would land elsewhere on.
    fn noise(device: &Device, width: u32, height: u32) -> Com<ID3D11Texture2D> {
        let mut state = 0x9e37_79b9_u32;
        let pixels: Vec<u8> = (0..width * height * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        bgra(device, width, height, &pixels)
    }

    /// An eight-bit BGRA texture of `device` holding `pixels`, rows `width *
    /// 4` bytes apart.
    fn bgra(device: &Device, width: u32, height: u32, pixels: &[u8]) -> Com<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
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
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let mut raw: *mut ID3D11Texture2D = core::ptr::null_mut();
        // SAFETY: a live device; the description, the pixels and the output
        // are live for the call.
        let hr = unsafe {
            vcall!(
                device.device(),
                CreateTexture2D,
                &raw const desc,
                &raw const init,
                &raw mut raw
            )
        }
        .expect("the entry");
        assert!(hr >= 0, "the source texture: 0x{hr:08x}");
        // SAFETY: a texture whose reference the call handed over.
        unsafe { Com::from_raw(raw) }.expect("a texture")
    }

    /// A target's texture read back whole: its rows, `RowPitch` apart, and a
    /// two-plane layout's chroma rows after its luma rows.
    fn read_back(device: &Device, target: &Target) -> (Vec<u8>, usize) {
        let (width, height) = target.size();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: target.layout().format(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ as u32,
            MiscFlags: 0,
        };
        let staging = device.texture(&desc).expect("a staging texture");
        let context = device.context();
        // SAFETY: plain data the call fills.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
        // SAFETY: the device's context on this thread; both textures are live
        // and of one format and size; the mapping is read and unmapped before
        // the staging texture goes.
        unsafe {
            vcall!(
                context,
                CopyResource,
                staging.as_ptr().cast::<ID3D11Resource>(),
                target.texture().as_ptr().cast::<ID3D11Resource>()
            );
            let hr = vcall!(
                context,
                Map,
                staging.as_ptr().cast::<ID3D11Resource>(),
                0,
                D3D11_MAP_READ,
                0,
                &raw mut mapped
            )
            .expect("the entry");
            assert!(hr >= 0, "map: 0x{hr:08x}");
        }
        let pitch = mapped.RowPitch as usize;
        let rows = if target.layout().full_chroma() {
            height as usize
        } else {
            height as usize * 3 / 2
        };
        // SAFETY: the mapping holds `rows` rows of `pitch` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * rows) }
            .to_vec();
        // SAFETY: mapped above.
        unsafe { vcall!(context, Unmap, staging.as_ptr().cast::<ID3D11Resource>(), 0) };
        (bytes, pitch)
    }

    /// **The conversion lands on the reference, in every layout, sample for
    /// sample, read where an encoder reads it**: the same functions the Linux
    /// conversion is checked against, within one step. A ten-bit sample of
    /// the two-plane layout is read from the high ten bits of its word and
    /// the low six must be clear; the packed words are taken apart by their
    /// documented fields. On every adapter that renders; one that has no
    /// target in a layout says so and the others decide. Needs a GPU, so off
    /// by default: `cargo test -p lowlat-capture d3d11 -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "requires a GPU"]
    fn the_conversion_matches_the_reference_in_every_layout() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let mut checked = 0;
        for adapter in d3d11.adapters().expect("the walk") {
            if !adapter.decodes_here() {
                continue;
            }
            let device = d3d11.open(adapter.luid).expect("a device");
            let (texture, width, height) = source(&device);
            for layout in [Layout::Nv12, Layout::P010, Layout::Ayuv, Layout::Y410] {
                let target = match Target::new(&device, layout, width, height) {
                    Ok(target) => target,
                    Err(error) => {
                        println!("{}: no {layout:?} target, {error}", adapter.description);
                        continue;
                    }
                };
                let mut converter = Converter::new(&device, layout).expect("the shader");
                converter
                    .convert(&device, texture.as_ptr(), &target)
                    .expect("the conversion");
                let (bytes, pitch) = read_back(&device, &target);
                let mut worst = 0u16;
                for (block, [r, g, b]) in PATTERN.iter().enumerate() {
                    let (want_y, want_u, want_v) = match layout {
                        Layout::Nv12 | Layout::Ayuv => {
                            let (y, u, v) = reference(*r, *g, *b);
                            (u16::from(y), u16::from(u), u16::from(v))
                        }
                        Layout::P010 | Layout::Y410 => reference_ten(*r, *g, *b),
                    };
                    let mut check = |name: &str, got: u16, want: u16| {
                        worst = worst.max(got.abs_diff(want));
                        assert!(
                            got.abs_diff(want) <= 1,
                            "{} {layout:?} block {block} {name} {got} wanted {want}",
                            adapter.description
                        );
                    };
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let x = block * 2 + dx;
                            let at = dy * pitch;
                            match layout {
                                Layout::Nv12 => check("luma", u16::from(bytes[at + x]), want_y),
                                Layout::P010 => {
                                    let word = u16::from_le_bytes([
                                        bytes[at + x * 2],
                                        bytes[at + x * 2 + 1],
                                    ]);
                                    assert_eq!(word & 0x3F, 0, "{layout:?} low bits set");
                                    check("luma", word >> 6, want_y);
                                }
                                Layout::Ayuv => {
                                    let p = &bytes[at + x * 4..at + x * 4 + 4];
                                    check("v", u16::from(p[0]), want_v);
                                    check("u", u16::from(p[1]), want_u);
                                    check("y", u16::from(p[2]), want_y);
                                    check("alpha", u16::from(p[3]), 255);
                                }
                                Layout::Y410 => {
                                    let p = &bytes[at + x * 4..at + x * 4 + 4];
                                    let word = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                                    check("u", (word & 0x3FF) as u16, want_u);
                                    check("y", ((word >> 10) & 0x3FF) as u16, want_y);
                                    check("v", ((word >> 20) & 0x3FF) as u16, want_v);
                                    check("alpha", (word >> 30) as u16, 3);
                                }
                            }
                        }
                    }
                    let chroma = height as usize * pitch;
                    match layout {
                        Layout::Nv12 => {
                            check("u", u16::from(bytes[chroma + block * 2]), want_u);
                            check("v", u16::from(bytes[chroma + block * 2 + 1]), want_v);
                        }
                        Layout::P010 => {
                            let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                            let (u, v) = (word(chroma + block * 4), word(chroma + block * 4 + 2));
                            assert_eq!((u | v) & 0x3F, 0, "{layout:?} chroma low bits set");
                            check("u", u >> 6, want_u);
                            check("v", v >> 6, want_v);
                        }
                        Layout::Ayuv | Layout::Y410 => {}
                    }
                }
                println!(
                    "{}: {layout:?} {} within {worst} of the reference over {} colours",
                    adapter.description,
                    if draws(&device) { "drawn" } else { "computed" },
                    PATTERN.len()
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no adapter converted anything");
    }

    /// **The draws land on the compute pass's samples, byte for byte**, on a
    /// picture of noise, in every layout an adapter can write both ways: on
    /// every adapter that renders, whichever pass it converts by. A layout
    /// one way cannot write says so and the others decide. Needs a GPU, so
    /// off by default.
    #[test]
    #[ignore = "requires a GPU"]
    fn the_draws_land_on_the_compute_pass_samples() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let (width, height) = (256, 64);
        let mut compared = 0;
        for adapter in d3d11.adapters().expect("the walk") {
            if !adapter.decodes_here() {
                continue;
            }
            let device = d3d11.open(adapter.luid).expect("a device");
            let source = noise(&device, width, height);
            for layout in [Layout::Nv12, Layout::P010, Layout::Ayuv, Layout::Y410] {
                let both = (
                    Target::made(&device, layout, width, height, false),
                    Target::made(&device, layout, width, height, true),
                );
                let (Ok(computed), Ok(drawn)) = both else {
                    println!("{}: {layout:?} not writable both ways", adapter.description);
                    continue;
                };
                for (target, by_draws) in [(&computed, false), (&drawn, true)] {
                    Converter::made(&device, layout, by_draws)
                        .expect("the shaders")
                        .convert(&device, source.as_ptr(), target)
                        .expect("the conversion");
                }
                let ((a, a_pitch), (b, b_pitch)) =
                    (read_back(&device, &computed), read_back(&device, &drawn));
                let (rows, row) = match layout {
                    Layout::Nv12 => (height * 3 / 2, width),
                    Layout::P010 => (height * 3 / 2, width * 2),
                    Layout::Ayuv | Layout::Y410 => (height, width * 4),
                };
                for at in 0..rows as usize {
                    let (a, b) = (
                        &a[at * a_pitch..][..row as usize],
                        &b[at * b_pitch..][..row as usize],
                    );
                    let first = a.iter().zip(b).position(|(x, y)| x != y);
                    assert_eq!(
                        first, None,
                        "{} {layout:?} row {at}: drawn and computed differ",
                        adapter.description
                    );
                }
                println!("{}: {layout:?} drawn as computed", adapter.description);
                compared += 1;
            }
        }
        assert!(compared > 0, "no adapter wrote any layout both ways");
    }
}
