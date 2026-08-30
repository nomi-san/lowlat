//! Colour conversion, from what the display scans out to what an encoder takes.
//!
//! The shader is in `shaders/convert.comp` and its compiled form is committed
//! beside it, so building this crate needs no shader compiler. See
//! docs/05-host.md section 3 for the colour rules it implements; they are not
//! restated here.
//!
//! **The result is one image in a two-plane layout, written through a view per
//! plane.** That is not a style choice: the two-plane format cannot be written
//! to directly on any device seen here, while each of its planes can, and
//! moving plane data by copying between subresources instead drops chroma on
//! more than one driver and looks like an encoder fault for as long as it takes
//! to find.

use ash::vk;

use std::os::fd::OwnedFd;

use crate::vulkan::{Device, Error, Imported, PlaneLayout, driver};

/// The untiled arrangement, as the display interface numbers it.
const LINEAR: u64 = 0;

/// Both ways a frame can be handed on, in the order they are asked about.
///
/// **Wanted together so the choice of encoder is not made here**: one takes a
/// display-interface descriptor and another this interface's own, and the
/// picture is allocated before either has been picked. **But wanting both is
/// not the same as getting both.** Which of them a device will export, and
/// whether it will export them from one allocation, is a property of the exact
/// image being made -- its format, tiling, usage and flags -- and has to be
/// asked. Asking for a pair a device does not offer is not a refusal at
/// allocation; it is accepted and the export fails later, or does not, and the
/// only thing that says so is the validation layer.
const WANTED: [vk::ExternalMemoryHandleTypeFlags; 2] = [
    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
    vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD,
];

/// The compiled shader, committed rather than built.
///
/// A build-time shader compiler would be a dependency for something that
/// changes a handful of times in a project's life, and it would have to be
/// present on every machine that builds this. The source sits beside it and
/// `scripts/build-shaders.sh` regenerates it.
const CONVERT: &[u8] = include_bytes!("../shaders/convert.spv");

/// The full-chroma entry point, from the same source file as [`CONVERT`] and
/// committed beside it. The two cannot drift: they are one body compiled
/// twice, because a compiler strips an uncalled entry point as dead.
const CONVERT_444: &[u8] = include_bytes!("../shaders/convert-444.spv");

/// The packed full-chroma entry point, the third body from the same file.
const CONVERT_PACKED: &[u8] = include_bytes!("../shaders/convert-packed.spv");

/// Invocations per workgroup, per axis. Each one owns a 2x2 block, so a group
/// covers 16 by 16 pixels.
const GROUP: u32 = 8;

/// How much of the picture a poke covers, per axis.
///
/// **Measured, and the answer is all of it.** The wake scales with the work
/// the poke submits and does not saturate until the dispatch is the size of a
/// conversion. Swept on the integrated open-stack device at 1080p, against a
/// warm conversion of 0.43 ms and 1.95 ms with no poke at all: a sixteenth of
/// the picture in each axis leaves the conversion at 0.93 ms, a quarter of
/// each axis reaches 0.44 but keeps a 1.09 ms tail, and only the whole
/// picture holds both the median and the tail, at 0.43 and 0.55. One
/// workgroup wakes least of all, and a buffer fill wakes less than that. The
/// same sweep at 1440p reads 1.18, 0.70 and 0.70 against a warm 0.64.
///
/// **A sixteenth was the earlier figure and it was wrong on this hardware**,
/// or stopped being right: it recovers about half the wakeup and reads as a
/// working poke, because something is plainly better than nothing.
///
/// **This is half of a pair.** The poke buys nothing unless the conversion
/// follows within about a millisecond of it, so this and the stream's poke
/// lead have to move together -- changing either alone measures as no change
/// at all, live and on the bench.
const POKE_DIVISOR: u32 = 1;

/// The workgroups a poke covers, from a picture's size.
///
/// **Written once so the caller cannot drift from the measured figure.** A
/// caller picking its own fraction would be re-deriving what the constant
/// above already stands for.
pub fn poke_groups(width: u32, height: u32) -> (u32, u32) {
    (
        width
            .div_ceil(2)
            .div_ceil(GROUP)
            .div_ceil(POKE_DIVISOR)
            .max(1),
        height
            .div_ceil(2)
            .div_ceil(GROUP)
            .div_ceil(POKE_DIVISOR)
            .max(1),
    )
}

/// How long a collect waits before declaring the conversion stuck, in
/// nanoseconds.
///
/// **A bound, not a pace.** The conversion's fence orders behind the
/// compositor's own in-flight render into the captured buffer, so it is not
/// only this process's work: a display stack that stops signalling would turn
/// an unbounded wait into a caller that can never observe its own stop flag,
/// and a teardown that never returns. The longest legitimate wait measured is
/// under three milliseconds; a hundred is far past pathological and still
/// bounded.
const COLLECT_BUDGET_NS: u64 = 100_000_000;

/// What the shader is told, per dispatch.
///
/// Laid out to match the shader's own block exactly. Two signed extents then
/// two flags, which is sixteen bytes and needs no padding on either side.
///
/// **A field added here without one added there reads whatever follows the
/// range**, which is undefined rather than zero, so the two move together.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Params {
    width: i32,
    height: i32,
    dither: u32,
    ten_bit: u32,
}

/// How many bits a converted sample carries.
///
/// **The target's property, not the source's.** What the display hands us
/// arrives through a sampler and is normalised whatever its depth
/// ([07 §3.3](../../../docs/07-platforms.md)); this is what the encoder reads
/// afterwards, and it is the only place the two can differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    #[default]
    Eight,
    Ten,
}

impl Depth {
    /// The layout one luma sample is stored in.
    ///
    /// **Sixteen bits a sample, not the packed ten-bit formats, and the
    /// devices decided that.** `R10X6_UNORM_PACK16` would have let the driver
    /// place the samples in the high ten bits for us; one of the three parts
    /// here does not offer it as a storage image at all, at either tiling,
    /// with or without a transfer usage -- while offering `R16_UNORM`
    /// everywhere. So the alignment is ours to do, in one multiply in the
    /// shader, rather than a second allocation path for one vendor.
    ///
    /// **What leaves is the same bytes either way.** A descriptor names a
    /// four-character code and a pitch; the layout this image was made with
    /// never travels, so a sixteen-bit plane holding samples in its high ten
    /// bits *is* the ten-bit two-plane format an encoder imports.
    const fn luma(self) -> vk::Format {
        match self {
            Self::Eight => vk::Format::R8_UNORM,
            Self::Ten => vk::Format::R16_UNORM,
        }
    }

    /// The layout one pair of colour samples is stored in.
    const fn chroma(self) -> vk::Format {
        match self {
            Self::Eight => vk::Format::R8G8_UNORM,
            Self::Ten => vk::Format::R16G16_UNORM,
        }
    }

    pub const fn ten_bit(self) -> bool {
        matches!(self, Self::Ten)
    }

    /// Bytes one luma sample occupies.
    ///
    /// **Ten bits cost two bytes, not one and a quarter.** They are stored one
    /// to a sixteen-bit word with six bits unused, so anything sizing a plane
    /// from width and height has to multiply by this or read back half a
    /// picture and call it a whole one.
    pub const fn bytes_per_sample(self) -> u32 {
        match self {
            Self::Eight => 1,
            Self::Ten => 2,
        }
    }
}

/// How a converted frame is laid out, for whatever imports it next.
/// How a converted frame is laid out, for whatever imports it next.
///
/// **Two layouts, one entry point each in the shader, and a third to come
/// with the packed path.** An encoder that reads full-resolution chroma as
/// three planes is not the same encoder that reads it packed into one, so the
/// kind travels with every handover rather than being inferable from the
/// offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Luma, then interleaved colour at half resolution in both directions.
    #[default]
    SemiPlanar420,
    /// Luma, blue difference, red difference, each at full resolution.
    Planar444,
    /// One word per pixel carrying all three, which is the open stack's
    /// full-chroma surface: AYUV at eight bits, Y410 at ten.
    Packed444,
}

#[derive(Debug, Clone, Copy)]
pub struct Exported {
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    /// Bytes per row, the same for every plane, which is the single figure an
    /// encoder registering by pointer is given.
    pub pitch: u32,
    /// Luma first, then the colour. For the subsampled layout the interleaved
    /// colour plane begins exactly one luma plane in, which is what an encoder
    /// assumes and what this allocation is built to guarantee; for the
    /// full-chroma layout the two colour planes follow in order.
    pub planes: [PlaneLayout; 3],
    /// How the planes above are arranged.
    pub kind: Layout,
    /// How many bits each sample carries. **An importer cannot work this out
    /// from the offsets**: a ten-bit frame and an eight-bit one twice as wide
    /// have the same pitch and the same plane split, and describing one as the
    /// other is accepted by the driver and decodes to noise.
    pub depth: Depth,
}

impl Exported {
    /// The layout one untiled region has when it carries both planes and the
    /// colour plane begins exactly one luma plane in.
    ///
    /// **Written once because an encoder is told it once.** It is given one
    /// address and one row length and derives the rest, so any interface that
    /// hands a frame on has to produce this same arrangement; two of them
    /// computing it separately is how they come to disagree.
    pub fn packed(width: u32, height: u32, pitch: u32) -> Self {
        Self::packed_at(width, height, pitch, Depth::Eight)
    }

    /// The same, for a frame whose samples are not eight bits.
    pub fn packed_at(width: u32, height: u32, pitch: u32, depth: Depth) -> Self {
        Self {
            width,
            height,
            depth,
            modifier: LINEAR,
            pitch,
            kind: Layout::SemiPlanar420,
            planes: [
                PlaneLayout { offset: 0, pitch },
                PlaneLayout {
                    offset: pitch * height,
                    pitch,
                },
                PlaneLayout {
                    offset: 0,
                    pitch: 0,
                },
            ],
        }
    }
}

/// A converted frame: two planes in one allocation, laid out the way an
/// encoder expects to find them.
///
/// **Two images rather than one in a two-plane format, and the encoder is the
/// reason.** An encoder registering a frame by pointer is given one address and
/// one row length, and takes the colour plane to begin exactly one luma plane
/// later. A driver asked for a two-plane image does not oblige: measured here,
/// it put the colour plane 49152 bytes further on than that, which an encoder
/// reading the computed position cannot see and cannot survive. Two images
/// bound into one allocation at offsets of our choosing put it exactly where it
/// is expected, and cost nothing: both are untiled, their row lengths come out
/// equal, and the whole thing still leaves as one descriptor.
pub struct Nv12 {
    luma_image: vk::Image,
    chroma_image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    /// One view per plane, which is what the conversion writes through.
    planes: [vk::ImageView; 2],
    pub width: u32,
    pub height: u32,
    /// Bytes per row, the same for both planes. An encoder is told this once.
    pub pitch: u32,
    /// How many bits each sample carries.
    pub depth: Depth,
}

/// What a conversion writes into, named by handles rather than ownership.
///
/// **The seam that lets a conversion fill somebody else's picture.** An owned
/// [`Nv12`] borrows itself here; an encoder that lends its own picture's
/// planes builds one from those handles. The final layout is the reader's:
/// an owned target stays writable, a lent picture is handed over in the
/// layout its encoder reads, so the writer's recording is what pays the
/// transition.
#[derive(Debug, Clone, Copy)]
pub struct TargetRef {
    /// The image behind each plane; one two-plane image repeats itself, and a
    /// subsampled target carries its third slot as the same image so the
    /// barrier pass can deduplicate it.
    pub luma_image: vk::Image,
    pub chroma_image: vk::Image,
    pub cr_image: vk::Image,
    /// One view per plane, what the shader writes through. The third slot is
    /// unset for the subsampled layout, whose entry point never reads it.
    pub planes: [vk::ImageView; 3],
    /// The layout the pictures are left in. [`vk::ImageLayout::GENERAL`] for
    /// an owned target; a lent picture names what its reader expects.
    pub final_layout: vk::ImageLayout,
    /// How the planes above are arranged, which is what picks the shader's
    /// entry point.
    pub kind: Layout,
    /// How many bits a sample in these planes carries. **Travels with the
    /// handles rather than being configured**, because a lent picture's depth
    /// belongs to the encoder that lent it and the conversion has to be told,
    /// not to decide.
    pub depth: Depth,
}

impl TargetRef {
    /// A target lent by an encoder on the same device.
    ///
    /// One two-plane picture, written through its plane views and handed
    /// over in the layout the encoder reads.
    pub fn lent_to_encoder(image: vk::Image, planes: [vk::ImageView; 2]) -> Self {
        Self::lent_to_encoder_at(image, planes, Depth::Eight)
    }

    /// The same, for an encoder whose picture is not eight-bit.
    pub fn lent_to_encoder_at(image: vk::Image, planes: [vk::ImageView; 2], depth: Depth) -> Self {
        Self {
            luma_image: image,
            chroma_image: image,
            cr_image: image,
            planes: [planes[0], planes[1], vk::ImageView::null()],
            final_layout: vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
            kind: Layout::SemiPlanar420,
            depth,
        }
    }
}

impl Nv12 {
    /// This target, by its handles.
    pub fn target(&self) -> TargetRef {
        TargetRef {
            luma_image: self.luma_image,
            chroma_image: self.chroma_image,
            cr_image: self.chroma_image,
            planes: [self.planes[0], self.planes[1], vk::ImageView::null()],
            final_layout: vk::ImageLayout::GENERAL,
            kind: Layout::SemiPlanar420,
            depth: self.depth,
        }
    }
}

/// A converted frame in the three-plane full-chroma layout: luma, blue
/// difference and red difference, each at full resolution, in one allocation.
///
/// **Three images rather than one, for the same reason the subsampled target
/// is two.** An encoder registering a frame by pointer is given one address
/// and one row length and takes each plane to follow the one before it; three
/// images bound into one allocation at offsets of our choosing put them
/// exactly where the encoder expects, and cost nothing more than the two-plane
/// arrangement does.
pub struct Planar444 {
    luma_image: vk::Image,
    cb_image: vk::Image,
    cr_image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    /// One view per plane, which is what the conversion writes through.
    planes: [vk::ImageView; 3],
    pub width: u32,
    pub height: u32,
    /// Bytes per row, the same for all three planes.
    pub pitch: u32,
    /// How many bits each sample carries.
    pub depth: Depth,
}

impl Planar444 {
    /// This target, by its handles.
    pub fn target(&self) -> TargetRef {
        TargetRef {
            luma_image: self.luma_image,
            chroma_image: self.cb_image,
            cr_image: self.cr_image,
            planes: self.planes,
            final_layout: vk::ImageLayout::GENERAL,
            kind: Layout::Planar444,
            depth: self.depth,
        }
    }
}

impl core::fmt::Debug for Planar444 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Planar444")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pitch", &self.pitch)
            .finish_non_exhaustive()
    }
}

/// A converted frame in the packed full-chroma layout: one word per pixel
/// carrying luma and both colour differences.
///
/// **One image of plain thirty-two-bit words, not a colour format.** The
/// packed ten-bit layouts cannot be written as storage images on the parts
/// seen here, so the shader composes each word itself and the image only
/// carries it; an importer names the layout from the descriptor, and the
/// bytes are the same either way.
pub struct Packed444 {
    image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    /// The view the conversion writes through.
    view: vk::ImageView,
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub pitch: u32,
    /// How many bits each sample carries, which is what picks AYUV over Y410.
    pub depth: Depth,
}

impl Packed444 {
    /// This target, by its handles.
    pub fn target(&self) -> TargetRef {
        TargetRef {
            luma_image: self.image,
            chroma_image: self.image,
            cr_image: self.image,
            planes: [self.view, vk::ImageView::null(), vk::ImageView::null()],
            final_layout: vk::ImageLayout::GENERAL,
            kind: Layout::Packed444,
            depth: self.depth,
        }
    }
}

impl core::fmt::Debug for Packed444 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Packed444")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pitch", &self.pitch)
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for Nv12 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nv12")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pitch", &self.pitch)
            .finish_non_exhaustive()
    }
}

/// What one submission is waiting to be collected.
///
/// **The flight flag.** Present means something has been submitted and not
/// collected, which is what lets `poll` answer without asking the fence
/// and keeps a second submit from racing the first. The view names the
/// captured buffer a conversion read, which the display swaps, so it is
/// per-submission.
enum Flight {
    Convert(vk::ImageView),
}

/// What a converted picture came to, as one value.
///
/// **Compared against the previous frame's and nothing else.** It says whether
/// the picture changed, never what it contains, and two pictures that agree
/// here are treated as the same picture -- so the loop that trusts it needs a
/// periodic forced pass for the case where they agree and should not have.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Digest(pub u64);

pub struct Converter {
    sampler: vk::Sampler,
    layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// The full-chroma entry point, beside the subsampled one above. The two
    /// share a layout and a descriptor set; only the entry differs.
    pipeline_444: vk::Pipeline,
    /// The packed full-chroma entry point, third from the same source file.
    pipeline_packed: vk::Pipeline,
    pool: vk::DescriptorPool,
    commands: vk::CommandPool,
    /// Reset and reused, never recreated.
    ///
    /// **A fence is a driver object and creating one is not free.** On one
    /// driver each create and each destroy is a synchronous round trip to the
    /// card's own processor, which busy-polls the clock while it waits, so a
    /// fence per conversion is two of those per frame and shows up as burned
    /// processor time rather than as waiting. One fence, reset before each
    /// submit, is what a fence is for.
    ///
    /// **One conversion at a time.** A second conversion in flight would need
    /// a second fence, and the flight below refuses it.
    fence: vk::Fence,
    /// The single command buffer, reset and re-recorded per submission.
    ///
    /// Reused for the same reason the fence is: creating either is a driver
    /// round trip, and per frame it shows up as burned processor time rather
    /// than as waiting. The one-in-flight rule is what makes a single buffer
    /// safe: it is only reset after the previous submission's fence has fired.
    command: vk::CommandBuffer,
    /// One descriptor set, rewritten per submission rather than reallocated.
    ///
    /// Bound by `command`, so the one-in-flight rule protects it exactly as it
    /// protects the command buffer: it is only touched again after the fence
    /// has fired.
    set: vk::DescriptorSet,
    /// The submission in flight, if there is one.
    flight: Option<Flight>,
    /// Whether the flight has already overrun the collect budget once.
    ///
    /// A stuck conversion is polled rather than waited for from then on:
    /// waiting the budget again on every ask would slow the caller's loop to
    /// the budget's own cadence for as long as the wedge lasts.
    stuck: bool,
    /// Where the shader accumulates its summary, on the device.
    summary: vk::Buffer,
    summary_memory: vk::DeviceMemory,
    /// The same eight bytes somewhere the processor can read them.
    ///
    /// **A copy rather than a shared allocation.** Eight thousand atomics a
    /// frame into memory the host can see would cross the bus for each one on
    /// a card that has its own; they land in device memory and come back once.
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    /// Mapped once and left mapped. Coherent, so nothing has to invalidate it.
    mapped: *mut u8,
}

// SAFETY: the mapped pointer is owned by this converter and is only read
// inside `poll`, after the fence has fired, which the one-conversion rule
// above already serialises. Nothing else may reach it.
unsafe impl Send for Converter {}

impl core::fmt::Debug for Converter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converter").finish_non_exhaustive()
    }
}

/// Two thirty-two bit accumulators, which is what the shader writes.
///
/// **Sixty-four bits, and the question is collisions between one frame and the
/// next rather than across a session.** Two consecutive pictures that differ
/// have to differ here; a picture that recurs later is not a hazard, because
/// nothing is being looked up. At sixty comparisons a second that is one
/// chance in about ten to the fourteenth over an hour, and the forced pass
/// covers the rest.
const DIGEST_BYTES: u64 = 8;

/// The same figure where a length is wanted rather than a device size.
const DIGEST: usize = 8;

impl Converter {
    /// One small buffer and the memory under it.
    fn buffer(
        device: &Device,
        usage: vk::BufferUsageFlags,
        host: bool,
    ) -> Result<(vk::Buffer, vk::DeviceMemory), Error> {
        let vk_device = &device.device;
        let info = vk::BufferCreateInfo::default()
            .size(DIGEST_BYTES)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { vk_device.create_buffer(&info, None) }.map_err(driver)?;

        // SAFETY: created on this device just above.
        let requirements = unsafe { vk_device.get_buffer_memory_requirements(buffer) };
        let index = if host {
            device.host_visible_memory(requirements.memory_type_bits)
        } else {
            device.device_local_memory(requirements.memory_type_bits)
        };
        let index = match index {
            Ok(index) => index,
            Err(error) => {
                // SAFETY: nothing is bound to it.
                unsafe { vk_device.destroy_buffer(buffer, None) };
                return Err(error);
            }
        };
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = match unsafe { vk_device.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(error) => {
                // SAFETY: nothing is bound to it.
                unsafe { vk_device.destroy_buffer(buffer, None) };
                return Err(driver(error));
            }
        };
        // SAFETY: neither handle is bound yet.
        if let Err(error) = unsafe { vk_device.bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: allocated above and nothing refers to either.
            unsafe {
                vk_device.free_memory(memory, None);
                vk_device.destroy_buffer(buffer, None);
            }
            return Err(driver(error));
        }
        Ok((buffer, memory))
    }

    /// Build the pipeline. Once per session, never per frame.
    pub fn new(device: &Device) -> Result<Self, Error> {
        let vk_device = &device.device;

        // Nothing is filtered: every read is an exact texel and a sampler that
        // interpolated would average pixels that are not ours to average.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        // SAFETY: the create info outlives the call.
        let sampler = unsafe { vk_device.create_sampler(&sampler_info, None) }.map_err(driver)?;

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            // The packed body's own binding: the same picture, integer-typed.
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings outlive the call.
        let layout = unsafe { vk_device.create_descriptor_set_layout(&layout_info, None) }
            .map_err(driver)?;

        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(u32::try_from(size_of::<Params>()).unwrap_or(16))];
        let layouts = [layout];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&ranges);
        // SAFETY: both slices outlive the call.
        let pipeline_layout =
            unsafe { vk_device.create_pipeline_layout(&pipeline_layout_info, None) }
                .map_err(driver)?;

        let pipeline = Self::build_pipeline(vk_device, pipeline_layout, CONVERT, c"main")?;
        // **The second entry point, same module and same layout.** The two
        // pipelines differ only in which function they run, which is what
        // keeps the colour rules and the summary in one file. The interface
        // names every stage's entry point main, so the full-chroma function
        // was renamed at compile time and the name here is the same.
        let pipeline_444 = Self::build_pipeline(vk_device, pipeline_layout, CONVERT_444, c"main")?;
        let pipeline_packed =
            Self::build_pipeline(vk_device, pipeline_layout, CONVERT_PACKED, c"main")?;

        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(4),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes)
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET);
        // SAFETY: the sizes outlive the call.
        let pool = unsafe { vk_device.create_descriptor_pool(&pool_info, None) }.map_err(driver)?;

        // **The reset flag, because the one command buffer is reset per
        // submission.** Resetting a buffer from a pool without it is a
        // violation both drivers here happen to tolerate; the validation
        // layer is what names it.
        let commands_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(device.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the create info outlives the call.
        let commands =
            unsafe { vk_device.create_command_pool(&commands_info, None) }.map_err(driver)?;

        // One command buffer for the life of the converter, reset per
        // submission. See the field's note.
        let command_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(commands)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the allocate info outlives the call and the pool is this
        // device's.
        let buffers =
            unsafe { vk_device.allocate_command_buffers(&command_info) }.map_err(driver)?;
        let command = *buffers.first().ok_or(Error::NoQueue)?;

        // One descriptor set, rewritten per submission rather than
        // reallocated. See the field's note.
        let layouts = [layout];
        let set_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        // SAFETY: the allocate info outlives the call and the pool is this
        // device's.
        let sets = unsafe { vk_device.allocate_descriptor_sets(&set_info) }.map_err(driver)?;
        let set = *sets.first().ok_or(Error::NoMemoryType)?;

        let (summary, summary_memory) = Self::buffer(
            device,
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST,
            false,
        )?;
        let (readback, readback_memory) =
            Self::buffer(device, vk::BufferUsageFlags::TRANSFER_DST, true)?;
        // SAFETY: host visible and coherent, and nothing else maps it. It stays
        // mapped for the life of the converter, which is what makes reading it
        // per frame free.
        let mapped = unsafe {
            vk_device.map_memory(
                readback_memory,
                0,
                DIGEST_BYTES,
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(driver)?
        .cast::<u8>();

        // Last, so nothing after it can fail and leak it.
        // SAFETY: the create info outlives the call.
        let fence = unsafe { vk_device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(driver)?;

        Ok(Self {
            sampler,
            layout,
            pipeline_layout,
            pipeline,
            pipeline_444,
            pipeline_packed,
            pool,
            commands,
            fence,
            command,
            set,
            flight: None,
            stuck: false,
            summary,
            summary_memory,
            readback,
            readback_memory,
            mapped,
        })
    }

    fn build_pipeline(
        device: &ash::Device,
        layout: vk::PipelineLayout,
        code: &[u8],
        entry: &core::ffi::CStr,
    ) -> Result<vk::Pipeline, Error> {
        // The committed shader is a byte array; the interface wants words. A
        // misaligned or odd-length blob is a corrupt file rather than a
        // runtime condition, so it is refused rather than patched around.
        if code.len() % 4 != 0 {
            return Err(Error::BadShader);
        }
        let mut words = Vec::with_capacity(code.len() / 4);
        for chunk in code.chunks_exact(4) {
            let quad: [u8; 4] = chunk.try_into().map_err(|_| Error::BadShader)?;
            words.push(u32::from_le_bytes(quad));
        }

        let module_info = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: the words outlive the call.
        let module = unsafe { device.create_shader_module(&module_info, None) }.map_err(driver)?;

        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(entry);
        let create = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(layout)];
        // SAFETY: everything borrowed outlives the call.
        let built =
            unsafe { device.create_compute_pipelines(vk::PipelineCache::null(), &create, None) };
        // The module is only needed while the pipeline is being built.
        // SAFETY: created above; the pipeline holds no reference to it.
        unsafe { device.destroy_shader_module(module, None) };

        match built {
            Ok(pipelines) => pipelines.first().copied().ok_or(Error::BadShader),
            Err((_, result)) => Err(driver(result)),
        }
    }

    /// Release everything. Explicit for the same reason the import's is.
    ///
    /// **A conversion that never finished leaks the converter instead of
    /// hanging the teardown.** Destroying what a queue may still read trades
    /// a bounded leak for a fault, and the idle wait below has no timeout of
    /// its own, so the stuck case has to be caught before it.
    pub fn destroy(mut self, device: &Device) {
        let vk_device = &device.device;
        if self.flight.is_some() {
            // SAFETY: the fence is this device's and was submitted when the
            // flight was created.
            let waited =
                unsafe { vk_device.wait_for_fences(&[self.fence], true, COLLECT_BUDGET_NS) };
            if waited.is_err() {
                lowlat_common::log_warn!(
                    "convert: a conversion never finished, leaking its pipeline"
                );
                return;
            }
        }
        // SAFETY: every handle came from this device and the wait means no
        // submitted work still refers to any of them.
        unsafe {
            let _ = vk_device.device_wait_idle();
            if let Some(Flight::Convert(view)) = self.flight.take() {
                // SAFETY: the wait above means nothing submitted refers to it.
                vk_device.destroy_image_view(view, None);
            }
            vk_device.unmap_memory(self.readback_memory);
            vk_device.destroy_buffer(self.readback, None);
            vk_device.free_memory(self.readback_memory, None);
            vk_device.destroy_buffer(self.summary, None);
            vk_device.free_memory(self.summary_memory, None);
            vk_device.destroy_fence(self.fence, None);
            // The command buffer goes with its pool, and the set with its own.
            vk_device.destroy_command_pool(self.commands, None);
            vk_device.destroy_descriptor_pool(self.pool, None);
            vk_device.destroy_pipeline(self.pipeline, None);
            vk_device.destroy_pipeline(self.pipeline_444, None);
            vk_device.destroy_pipeline(self.pipeline_packed, None);
            vk_device.destroy_pipeline_layout(self.pipeline_layout, None);
            vk_device.destroy_descriptor_set_layout(self.layout, None);
            vk_device.destroy_sampler(self.sampler, None);
        }
    }
}

impl Device {
    /// Allocate a conversion target.
    ///
    /// Two planes in one image, with a view per plane. The image is asked for
    /// with a mutable format and extended usage, which is what lets it be
    /// written through those views: the two-plane format itself reports no
    /// write support anywhere, while the single-component formats its planes
    /// are addressed by report it everywhere.
    pub fn allocate_nv12(&self, width: u32, height: u32) -> Result<Nv12, Error> {
        self.allocate_planar(width, height, Depth::Eight)
    }

    /// The same, at a chosen depth.
    pub fn allocate_planar(&self, width: u32, height: u32, depth: Depth) -> Result<Nv12, Error> {
        // Both dimensions round up to even. A plane at half resolution has no
        // meaning for an odd one, and the shader's last block would write
        // outside the colour plane.
        let width = width.next_multiple_of(2);
        let height = height.next_multiple_of(2);

        // **Asked once, of the image that is actually made.** Both planes use
        // the same tiling and usage and differ only in format and extent, and
        // a device that answered differently for the two would leave one plane
        // of a picture unexportable, so the narrower answer is what both are
        // built with.
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC;
        let handle_types = self.exportable(depth.luma(), usage, vk::ImageTiling::LINEAR)
            & self.exportable(depth.chroma(), usage, vk::ImageTiling::LINEAR);
        let luma_image = self.plane_image(width, height, depth.luma(), handle_types)?;
        let chroma_image =
            match self.plane_image(width / 2, height / 2, depth.chroma(), handle_types) {
                Ok(image) => image,
                Err(error) => {
                    // SAFETY: created just above and nothing refers to it.
                    unsafe { self.device.destroy_image(luma_image, None) };
                    return Err(error);
                }
            };

        match self.bind_planes(luma_image, chroma_image, width, height, depth) {
            Ok(nv12) => Ok(nv12),
            Err(error) => {
                // SAFETY: both created above; binding is what failed.
                unsafe {
                    self.device.destroy_image(luma_image, None);
                    self.device.destroy_image(chroma_image, None);
                }
                Err(error)
            }
        }
    }

    /// One plane, untiled and shareable.
    ///
    /// **Untiled because every encoder here reads untiled** without being
    /// taught anything, and writing into what the device prefers and copying
    /// into what the encoder accepts would move a whole frame per frame. Both
    /// handle kinds are asked for at once so the same frame can go to an
    /// encoder that wants a display-interface descriptor or one that wants this
    /// interface's own; a device that refused the pair would fail here rather
    /// than at the handover.
    /// Which of the two handle kinds this device will really export a plane
    /// as, for the exact image the conversion allocates.
    ///
    /// **Asked per handle type, then intersected.** The interface answers one
    /// type at a time and reports, for that type, whether it can be exported
    /// at all and which other types it may be combined with in one allocation.
    /// A pair that both sides do not name is a pair no allocation may carry,
    /// and asking for it anyway is accepted by the drivers here while the
    /// validation layer calls it out twice -- once for the image and once for
    /// the memory.
    ///
    /// **One type is a normal answer, and none is too.** A device may export
    /// pictures one way, or not at all -- a software implementation converts
    /// perfectly well and hands nothing to anybody. Exporting is not a
    /// precondition for converting, so an empty answer declares no external
    /// kinds rather than refusing the allocation, and the refusal comes at the
    /// export, where somebody actually wanted one.
    fn exportable(
        &self,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        tiling: vk::ImageTiling,
    ) -> vk::ExternalMemoryHandleTypeFlags {
        let mut offered = vk::ExternalMemoryHandleTypeFlags::empty();
        let mut compatible = vk::ExternalMemoryHandleTypeFlags::from_raw(u32::MAX);
        for wanted in WANTED {
            let mut external =
                vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(wanted);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(tiling)
                .usage(usage)
                .flags(vk::ImageCreateFlags::empty())
                .push_next(&mut external);
            let mut properties = vk::ExternalImageFormatProperties::default();
            let mut out = vk::ImageFormatProperties2::default().push_next(&mut properties);
            // SAFETY: both chains outlive the call.
            let asked = unsafe {
                self.instance.get_physical_device_image_format_properties2(
                    self.physical,
                    &info,
                    &mut out,
                )
            };
            // **A refusal here is an answer, not a fault.** A device that will
            // not make this image for this handle type says so by refusing the
            // query, and the other type may still work.
            if asked.is_err() {
                continue;
            }
            let features = properties
                .external_memory_properties
                .external_memory_features;
            if !features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE) {
                continue;
            }
            offered |= wanted;
            compatible &= properties
                .external_memory_properties
                .compatible_handle_types;
        }
        offered & compatible
    }

    /// Which handle kinds this device will export an *allocation* as.
    ///
    /// **A different question from the one above, with a different answer.**
    /// The image query says how an image may be backed by foreign memory; this
    /// says how memory may leave. One driver here refuses the image query for
    /// a display-interface descriptor on an untiled image -- that kind wants a
    /// modifier-tiled image, which this pipeline deliberately does not make --
    /// while exporting the allocation as one perfectly well. Asking the image
    /// question about the memory is how a working export gets refused, and
    /// asking neither is how both end up declared and neither checked.
    ///
    /// **Every kind the device will export, and not the kinds it will export
    /// together.** `compatibleHandleTypes` says which kinds may share one
    /// allocation, and intersecting it across the kinds wanted answers a
    /// question nobody asked: a picture is exported as one kind at a time, and
    /// which one depends on the encoder that has not been chosen yet. One
    /// vendor here reports each kind compatible with itself alone, so that
    /// intersection is empty and the allocation ends up declaring nothing
    /// exportable -- which took capture on that card out entirely, and only a
    /// live output switch found it.
    fn exportable_memory(&self) -> vk::ExternalMemoryHandleTypeFlags {
        let mut offered = vk::ExternalMemoryHandleTypeFlags::empty();
        for wanted in WANTED {
            let info = vk::PhysicalDeviceExternalBufferInfo::default()
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .handle_type(wanted);
            let mut out = vk::ExternalBufferProperties::default();
            // SAFETY: both structures outlive the call.
            unsafe {
                self.instance
                    .get_physical_device_external_buffer_properties(self.physical, &info, &mut out);
            }
            if !out
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
            {
                continue;
            }
            offered |= wanted;
        }
        offered
    }

    fn plane_image(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
        handle_types: vk::ExternalMemoryHandleTypeFlags,
    ) -> Result<vk::Image, Error> {
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_types);
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // **Declared only where there is something to declare.** An empty
        // chain here is not the same as an absent one: it names a set of
        // foreign kinds this image may be backed by, and naming none of them
        // is a different image from one that was never asked.
        let create = if handle_types.is_empty() {
            create
        } else {
            create.push_next(&mut external)
        };
        // SAFETY: every borrowed structure outlives the call.
        unsafe { self.device.create_image(&create, None) }.map_err(driver)
    }

    /// Put both planes in one allocation, colour exactly one luma plane in.
    fn bind_planes(
        &self,
        luma_image: vk::Image,
        chroma_image: vk::Image,
        width: u32,
        height: u32,
        depth: Depth,
    ) -> Result<Nv12, Error> {
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: both images are this device's and untiled, which is what
        // makes this query answerable.
        let (luma_layout, chroma_layout) = unsafe {
            (
                self.device
                    .get_image_subresource_layout(luma_image, subresource),
                self.device
                    .get_image_subresource_layout(chroma_image, subresource),
            )
        };
        // An encoder is told one row length for both planes. Half the width at
        // twice the samples comes to the same figure as the full width, at
        // either depth, so they agree unless a driver pads them differently --
        // and if one ever does, that is a refusal rather than something to
        // paper over.
        if luma_layout.row_pitch != chroma_layout.row_pitch {
            return Err(Error::PlanesDisagree);
        }
        let pitch = u32::try_from(luma_layout.row_pitch).map_err(|_| Error::PlanesDisagree)?;
        let colour_at = luma_layout.row_pitch * u64::from(height);

        // SAFETY: both images are this device's.
        let (luma_needs, chroma_needs) = unsafe {
            (
                self.device.get_image_memory_requirements(luma_image),
                self.device.get_image_memory_requirements(chroma_image),
            )
        };
        // **A driver's own number, checked before it is divided by.** Zero
        // here is not a layout this code can place a plane in, and reaching
        // the modulo with it kills the process rather than refusing the
        // allocation.
        if chroma_needs.alignment == 0 || colour_at % chroma_needs.alignment != 0 {
            return Err(Error::PlanesDisagree);
        }
        let index =
            self.device_local_memory(luma_needs.memory_type_bits & chroma_needs.memory_type_bits)?;

        // **What the allocation may become, not what its images may be backed
        // by.** The two are asked separately because the drivers here answer
        // them differently, and using one answer for both either declares a
        // kind the image cannot carry or refuses an export that works.
        let exports = self.exportable_memory();
        let mut exportable = vk::ExportMemoryAllocateInfo::default().handle_types(exports);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(colour_at + chroma_needs.size)
            .memory_type_index(index);
        let allocate = if exports.is_empty() {
            allocate
        } else {
            allocate.push_next(&mut exportable)
        };
        // SAFETY: the chain outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let bound = (|| -> Result<[vk::ImageView; 2], Error> {
            // SAFETY: neither image is bound yet and both are this device's.
            unsafe {
                self.device
                    .bind_image_memory(luma_image, memory, 0)
                    .map_err(driver)?;
                self.device
                    .bind_image_memory(chroma_image, memory, colour_at)
                    .map_err(driver)?;
            }
            let mut planes = [vk::ImageView::null(); 2];
            // **The same formats the images were made with.** A view is what
            // the shader stores through, so one narrower than its image writes
            // a byte into every two-byte sample and leaves the other untouched
            // -- which is not a refusal anywhere, just half a picture.
            for (at, (image, format)) in
                [(luma_image, depth.luma()), (chroma_image, depth.chroma())]
                    .into_iter()
                    .enumerate()
            {
                let info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    });
                // SAFETY: the create info outlives the call.
                match unsafe { self.device.create_image_view(&info, None) } {
                    Ok(view) => {
                        if let Some(slot) = planes.get_mut(at) {
                            *slot = view;
                        }
                    }
                    Err(result) => {
                        for view in planes.into_iter().filter(|v| *v != vk::ImageView::null()) {
                            // SAFETY: created just above on this device.
                            unsafe { self.device.destroy_image_view(view, None) };
                        }
                        return Err(driver(result));
                    }
                }
            }
            Ok(planes)
        })();

        match bound {
            Ok(planes) => Ok(Nv12 {
                luma_image,
                chroma_image,
                memory,
                depth,
                planes,
                width,
                height,
                pitch,
            }),
            Err(error) => {
                // SAFETY: nothing is bound to it any more.
                unsafe { self.device.free_memory(memory, None) };
                Err(error)
            }
        }
    }

    /// Allocate a conversion target in the three-plane full-chroma layout.
    ///
    /// All three planes are the picture's own size, which is the whole
    /// difference from [`Device::allocate_planar`]: the colour is not
    /// subsampled, so nothing about the allocation may halve it.
    pub fn allocate_planar_444(
        &self,
        width: u32,
        height: u32,
        depth: Depth,
    ) -> Result<Planar444, Error> {
        // Every plane is full resolution, so the only rounding left is the
        // dispatch grid's own.
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC;
        let handle_types = self.exportable(depth.luma(), usage, vk::ImageTiling::LINEAR);
        let luma_image = self.plane_image(width, height, depth.luma(), handle_types)?;
        let cb_image = match self.plane_image(width, height, depth.luma(), handle_types) {
            Ok(image) => image,
            Err(error) => {
                // SAFETY: created just above and nothing refers to it.
                unsafe { self.device.destroy_image(luma_image, None) };
                return Err(error);
            }
        };
        let cr_image = match self.plane_image(width, height, depth.luma(), handle_types) {
            Ok(image) => image,
            Err(error) => {
                // SAFETY: both created above and nothing refers to either.
                unsafe {
                    self.device.destroy_image(luma_image, None);
                    self.device.destroy_image(cb_image, None);
                }
                return Err(error);
            }
        };

        match self.bind_planes_444(luma_image, cb_image, cr_image, width, height, depth) {
            Ok(target) => Ok(target),
            Err(error) => {
                // SAFETY: all three created above; binding is what failed.
                unsafe {
                    self.device.destroy_image(luma_image, None);
                    self.device.destroy_image(cb_image, None);
                    self.device.destroy_image(cr_image, None);
                }
                Err(error)
            }
        }
    }

    /// Put three full-resolution planes in one allocation, each exactly one
    /// plane after the one before it.
    fn bind_planes_444(
        &self,
        luma_image: vk::Image,
        cb_image: vk::Image,
        cr_image: vk::Image,
        width: u32,
        height: u32,
        depth: Depth,
    ) -> Result<Planar444, Error> {
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: all three images are this device's and untiled, which is
        // what makes this query answerable.
        let (luma_layout, cb_layout) = unsafe {
            (
                self.device
                    .get_image_subresource_layout(luma_image, subresource),
                self.device
                    .get_image_subresource_layout(cb_image, subresource),
            )
        };
        // All three planes are one sample wide, so their row lengths must
        // agree or the single pitch the encoder is given cannot describe them.
        if luma_layout.row_pitch != cb_layout.row_pitch {
            return Err(Error::PlanesDisagree);
        }
        let pitch = u32::try_from(luma_layout.row_pitch).map_err(|_| Error::PlanesDisagree)?;
        let plane_bytes = luma_layout.row_pitch * u64::from(height);

        // SAFETY: the images are this device's.
        let (luma_needs, cb_needs) = unsafe {
            (
                self.device.get_image_memory_requirements(luma_image),
                self.device.get_image_memory_requirements(cb_image),
            )
        };
        // **A driver's own number, checked before it is divided by.** Zero
        // here is not a layout this code can place a plane in.
        if cb_needs.alignment == 0
            || plane_bytes % cb_needs.alignment != 0
            || (plane_bytes * 2) % cb_needs.alignment != 0
        {
            return Err(Error::PlanesDisagree);
        }
        let index =
            self.device_local_memory(luma_needs.memory_type_bits & cb_needs.memory_type_bits)?;

        // **What the allocation may become, not what its images may be backed
        // by**, exactly as on the subsampled target.
        let exports = self.exportable_memory();
        let mut exportable = vk::ExportMemoryAllocateInfo::default().handle_types(exports);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(plane_bytes * 2 + cb_needs.size)
            .memory_type_index(index);
        let allocate = if exports.is_empty() {
            allocate
        } else {
            allocate.push_next(&mut exportable)
        };
        // SAFETY: the chain outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let bound = (|| -> Result<[vk::ImageView; 3], Error> {
            // SAFETY: no image is bound yet and all are this device's.
            unsafe {
                self.device
                    .bind_image_memory(luma_image, memory, 0)
                    .map_err(driver)?;
                self.device
                    .bind_image_memory(cb_image, memory, plane_bytes)
                    .map_err(driver)?;
                self.device
                    .bind_image_memory(cr_image, memory, plane_bytes * 2)
                    .map_err(driver)?;
            }
            let mut planes = [vk::ImageView::null(); 3];
            for (at, image) in [luma_image, cb_image, cr_image].into_iter().enumerate() {
                let info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(depth.luma())
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    });
                // SAFETY: the create info outlives the call.
                match unsafe { self.device.create_image_view(&info, None) } {
                    Ok(view) => {
                        if let Some(slot) = planes.get_mut(at) {
                            *slot = view;
                        }
                    }
                    Err(result) => {
                        for view in planes.into_iter().filter(|v| *v != vk::ImageView::null()) {
                            // SAFETY: created just above on this device.
                            unsafe { self.device.destroy_image_view(view, None) };
                        }
                        return Err(driver(result));
                    }
                }
            }
            Ok(planes)
        })();

        match bound {
            Ok(planes) => Ok(Planar444 {
                luma_image,
                cb_image,
                cr_image,
                memory,
                depth,
                planes,
                width,
                height,
                pitch,
            }),
            Err(error) => {
                // SAFETY: nothing is bound to it any more.
                unsafe { self.device.free_memory(memory, None) };
                Err(error)
            }
        }
    }

    /// Allocate a conversion target in the packed full-chroma layout.
    ///
    /// One word per pixel for the whole picture, which is the shape the open
    /// stack's full-chroma surfaces take.
    pub fn allocate_packed_444(
        &self,
        width: u32,
        height: u32,
        depth: Depth,
    ) -> Result<Packed444, Error> {
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC;
        // **A plain word format, which is a storage format everywhere.** The
        // packed colour layouts this stands in for cannot be written directly
        // on the parts seen here; the shader composes the word and nothing
        // reads it as colour on this side.
        let handle_types = self.exportable(vk::Format::R32_UINT, usage, vk::ImageTiling::LINEAR);
        let image = self.plane_image(width, height, vk::Format::R32_UINT, handle_types)?;

        match self.bind_packed(image, width, height, depth) {
            Ok(target) => Ok(target),
            Err(error) => {
                // SAFETY: created above; binding is what failed.
                unsafe { self.device.destroy_image(image, None) };
                Err(error)
            }
        }
    }

    /// Put the one packed plane in its own allocation.
    fn bind_packed(
        &self,
        image: vk::Image,
        width: u32,
        height: u32,
        depth: Depth,
    ) -> Result<Packed444, Error> {
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: the image is this device's and untiled.
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };
        let pitch = u32::try_from(layout.row_pitch).map_err(|_| Error::PlanesDisagree)?;

        // SAFETY: the image is this device's.
        let needs = unsafe { self.device.get_image_memory_requirements(image) };
        let index = self.device_local_memory(needs.memory_type_bits)?;

        let exports = self.exportable_memory();
        let mut exportable = vk::ExportMemoryAllocateInfo::default().handle_types(exports);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(needs.size)
            .memory_type_index(index);
        let allocate = if exports.is_empty() {
            allocate
        } else {
            allocate.push_next(&mut exportable)
        };
        // SAFETY: the chain outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let bound = (|| -> Result<vk::ImageView, Error> {
            // SAFETY: the image is not bound yet and is this device's.
            unsafe { self.device.bind_image_memory(image, memory, 0) }.map_err(driver)?;
            let info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R32_UINT)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            // SAFETY: the create info outlives the call.
            unsafe { self.device.create_image_view(&info, None) }.map_err(driver)
        })();

        match bound {
            Ok(view) => Ok(Packed444 {
                image,
                memory,
                view,
                width,
                height,
                pitch,
                depth,
            }),
            Err(error) => {
                // SAFETY: nothing is bound to it any more.
                unsafe { self.device.free_memory(memory, None) };
                Err(error)
            }
        }
    }

    /// Memory the device reads fastest, which is where a conversion target
    /// belongs: nothing on the processor reads it.
    pub(crate) fn device_local_memory(&self, allowed: u32) -> Result<u32, Error> {
        // SAFETY: the device came from this instance.
        let memory = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical)
        };
        memory
            .memory_types
            .iter()
            .take(usize::try_from(memory.memory_type_count).unwrap_or(0))
            .enumerate()
            .find(|(at, kind)| {
                allowed & (1 << at) != 0
                    && kind
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .and_then(|(at, _)| u32::try_from(at).ok())
            .ok_or(Error::NoMemoryType)
    }

    /// Hand the converted frame out as a descriptor an encoder can take.
    ///
    /// **The descriptor is the caller's to close.** Exporting duplicates it
    /// rather than moving it, so the frame stays usable and both ends have to
    /// be released.
    ///
    /// `display_interface` picks which kind of handle comes back. An encoder
    /// reached through the display stack takes one kind; one reached through
    /// this vendor's own compute interface takes the other, and it has no name
    /// for the first. The frame is allocated able to produce either.
    pub fn export_nv12(
        &self,
        nv12: &Nv12,
        display_interface: bool,
    ) -> Result<(OwnedFd, Exported), Error> {
        let wanted = if display_interface {
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT
        } else {
            vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD
        };
        // **Refused here rather than at the far end.** The allocation declared
        // what it may become; asking for anything else is asking for a
        // descriptor that was never made, and what comes back from a driver
        // that does not refuse it is one an encoder cannot import for a reason
        // naming neither side.
        if !self.exportable_memory().contains(wanted) {
            return Err(Error::NoExport);
        }
        let external = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(nv12.memory)
            .handle_type(wanted);
        // SAFETY: the info outlives the call and the memory is this device's.
        let fd = unsafe { external.get_memory_fd(&info) }.map_err(driver)?;
        // SAFETY: the driver returned a fresh owned descriptor.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        // **Not queried back.** The layout is the one this allocation was built
        // to, so reporting anything else would mean the two had drifted.
        Ok((
            fd,
            Exported::packed_at(nv12.width, nv12.height, nv12.pitch, nv12.depth),
        ))
    }

    /// Hand a full-chroma frame out as a descriptor an encoder can take.
    ///
    /// Same contract as [`Device::export_nv12`]; the descriptor carries three
    /// plane offsets instead of two, and an importer that reads only the first
    /// two is an importer of the wrong layout.
    pub fn export_planar_444(
        &self,
        frame: &Planar444,
        display_interface: bool,
    ) -> Result<(OwnedFd, Exported), Error> {
        let wanted = if display_interface {
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT
        } else {
            vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD
        };
        if !self.exportable_memory().contains(wanted) {
            return Err(Error::NoExport);
        }
        let external = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(frame.memory)
            .handle_type(wanted);
        // SAFETY: the info outlives the call and the memory is this device's.
        let fd = unsafe { external.get_memory_fd(&info) }.map_err(driver)?;
        // SAFETY: the driver returned a fresh owned descriptor.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        let plane_bytes = u64::from(frame.pitch) * u64::from(frame.height);
        Ok((
            fd,
            Exported {
                width: frame.width,
                height: frame.height,
                modifier: LINEAR,
                pitch: frame.pitch,
                kind: Layout::Planar444,
                depth: frame.depth,
                planes: [
                    PlaneLayout {
                        offset: 0,
                        pitch: frame.pitch,
                    },
                    PlaneLayout {
                        offset: u32::try_from(plane_bytes).unwrap_or(0),
                        pitch: frame.pitch,
                    },
                    PlaneLayout {
                        offset: u32::try_from(plane_bytes * 2).unwrap_or(0),
                        pitch: frame.pitch,
                    },
                ],
            },
        ))
    }

    /// Release a conversion target.
    pub fn release_nv12(&self, nv12: Nv12) {
        // SAFETY: every handle came from this device, and the wait means no
        // submitted work still refers to them.
        unsafe {
            let _ = self.device.device_wait_idle();
            for view in nv12.planes {
                self.device.destroy_image_view(view, None);
            }
            self.device.destroy_image(nv12.luma_image, None);
            self.device.destroy_image(nv12.chroma_image, None);
            self.device.free_memory(nv12.memory, None);
        }
    }

    /// Release a full-chroma conversion target.
    pub fn release_planar_444(&self, frame: Planar444) {
        // SAFETY: every handle came from this device, and the wait means no
        // submitted work still refers to them.
        unsafe {
            let _ = self.device.device_wait_idle();
            for view in frame.planes {
                self.device.destroy_image_view(view, None);
            }
            self.device.destroy_image(frame.luma_image, None);
            self.device.destroy_image(frame.cb_image, None);
            self.device.destroy_image(frame.cr_image, None);
            self.device.free_memory(frame.memory, None);
        }
    }

    /// Hand a packed full-chroma frame out as a descriptor.
    ///
    /// One plane, one offset, one row length; the importer names the layout
    /// from the four-character code the descriptor's kind implies, which is
    /// why the kind travels with it.
    pub fn export_packed_444(
        &self,
        frame: &Packed444,
        display_interface: bool,
    ) -> Result<(OwnedFd, Exported), Error> {
        let wanted = if display_interface {
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT
        } else {
            vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD
        };
        if !self.exportable_memory().contains(wanted) {
            return Err(Error::NoExport);
        }
        let external = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(frame.memory)
            .handle_type(wanted);
        // SAFETY: the info outlives the call and the memory is this device's.
        let fd = unsafe { external.get_memory_fd(&info) }.map_err(driver)?;
        // SAFETY: the driver returned a fresh owned descriptor.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        Ok((
            fd,
            Exported {
                width: frame.width,
                height: frame.height,
                modifier: LINEAR,
                pitch: frame.pitch,
                kind: Layout::Packed444,
                depth: frame.depth,
                planes: [
                    PlaneLayout {
                        offset: 0,
                        pitch: frame.pitch,
                    },
                    PlaneLayout {
                        offset: 0,
                        pitch: 0,
                    },
                    PlaneLayout {
                        offset: 0,
                        pitch: 0,
                    },
                ],
            },
        ))
    }

    /// Release a packed full-chroma conversion target.
    pub fn release_packed_444(&self, frame: Packed444) {
        // SAFETY: every handle came from this device, and the wait means no
        // submitted work still refers to them.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_image_view(frame.view, None);
            self.device.destroy_image(frame.image, None);
            self.device.free_memory(frame.memory, None);
        }
    }
}

impl Converter {
    /// Convert one captured frame into the target, blocking until the device
    /// has finished.
    ///
    /// **For a diagnostic, not for the loop.** The loop submits and collects
    /// around its own collect of the previous picture, so the conversion
    /// overlaps the encode; this is the same pair with the wait put back,
    /// which is the only difference.
    pub fn run(
        &mut self,
        device: &Device,
        source: &Imported,
        target: &TargetRef,
        dither: bool,
    ) -> Result<Digest, Error> {
        self.submit(device, source, target, dither)?;
        self.collect(device)?.ok_or(Error::Busy)
    }

    /// Submit one conversion. Returns as soon as it is queued, not when it is
    /// done.
    ///
    /// **One at a time.** A conversion already in flight is refused: the
    /// converter holds one fence and one command buffer, so two in flight is
    /// not a state it can serve, and one in flight means the caller has not
    /// collected the previous picture yet.
    ///
    /// The digest the conversion produces is handed out by [`Self::poll`],
    /// never here: it only exists once the device is done.
    pub fn submit(
        &mut self,
        device: &Device,
        source: &Imported,
        target: &TargetRef,
        dither: bool,
    ) -> Result<(), Error> {
        if self.flight.is_some() {
            return Err(Error::Busy);
        }
        let vk_device = &device.device;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(source.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(source.format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // SAFETY: the create info outlives the call.
        let view = unsafe { vk_device.create_image_view(&view_info, None) }.map_err(driver)?;

        match self.dispatch(device, source, target, view, dither, None) {
            Ok(()) => {
                self.flight = Some(Flight::Convert(view));
                Ok(())
            }
            Err(error) => {
                // SAFETY: created above; nothing submitted refers to it
                // because submitting is what failed, or nothing was submitted.
                unsafe { vk_device.destroy_image_view(view, None) };
                Err(error)
            }
        }
    }

    /// Poke the device with a trivial conversion dispatch.
    ///
    /// **For the wakeup cost an integrated device pays.** Measured there, a
    /// conversion submitted after a few milliseconds of idle costs 1.3 ms
    /// against 0.4 ms warm, because the compute block powers down between
    /// frames. A one-workgroup dispatch two milliseconds before the real one
    /// pays the wakeup while the loop would otherwise be waiting for the
    /// display, so the conversion itself runs warm. It is the real pipeline
    /// rather than a buffer fill, because measured again, a fill wakes only
    /// half of what the block needs.
    ///
    /// The target is whichever slot the next real conversion overwrites
    /// whole, so the one block this writes is of no consequence; the digest
    /// it produces is the caller's to discard.
    pub fn poke(
        &mut self,
        device: &Device,
        source: &Imported,
        target: &TargetRef,
        groups: (u32, u32),
    ) -> Result<(), Error> {
        if self.flight.is_some() {
            return Err(Error::Busy);
        }
        let vk_device = &device.device;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(source.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(source.format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // SAFETY: the create info outlives the call.
        let view = unsafe { vk_device.create_image_view(&view_info, None) }.map_err(driver)?;

        match self.dispatch(device, source, target, view, false, Some(groups)) {
            Ok(()) => {
                self.flight = Some(Flight::Convert(view));
                Ok(())
            }
            Err(error) => {
                // SAFETY: created above; nothing submitted refers to it.
                unsafe { vk_device.destroy_image_view(view, None) };
                Err(error)
            }
        }
    }

    /// Collect the submitted conversion, waiting for the device to finish.
    ///
    /// **The blocking half of the pair, and the wait is bounded.** The loop
    /// calls this on the far side of its own collect of the previous picture,
    /// so the wait is normally for work that is already done; that is the
    /// whole reason the submit happened where it did. Past
    /// [`COLLECT_BUDGET_NS`] the flight is left standing and the answer is
    /// nothing-yet: the next submit is refused as busy, the caller holds its
    /// last picture, and its loop stays responsive instead of hanging on a
    /// display stack that stopped signalling. Nothing when no conversion was
    /// submitted, which is the ordinary state of a display that could not be
    /// read.
    pub fn collect(&mut self, device: &Device) -> Result<Option<Digest>, Error> {
        if self.flight.is_none() {
            return Ok(None);
        }
        // Already overran the budget once: only poll from here on. See the
        // field's note.
        if self.stuck {
            let collected = self.poll(device)?;
            if collected.is_some() {
                self.stuck = false;
            }
            return Ok(collected);
        }
        // SAFETY: the fence is this device's and was submitted when the
        // flight was created.
        match unsafe {
            device
                .device
                .wait_for_fences(&[self.fence], true, COLLECT_BUDGET_NS)
        } {
            // The fence fired, so this cannot be None.
            Ok(()) => self.poll(device),
            Err(vk::Result::TIMEOUT) => {
                self.stuck = true;
                Ok(None)
            }
            Err(error) => Err(driver(error)),
        }
    }

    /// Whether the submitted conversion has finished, and what it found.
    ///
    /// **Never waits.** A not-ready answer costs one fence status ask and
    /// nothing else; the caller goes round its loop and asks again. When the
    /// fence has fired, the digest is read and the flight is cleared, which is
    /// what lets the next submit proceed.
    ///
    /// Nothing when no conversion was submitted, which is the state after a
    /// collect and the ordinary state of a display that could not be read.
    pub fn poll(&mut self, device: &Device) -> Result<Option<Digest>, Error> {
        let Some(flight) = self.flight.take() else {
            return Ok(None);
        };
        // SAFETY: the fence is this device's and was submitted when the flight
        // was created.
        let ready = unsafe { device.device.get_fence_status(self.fence) }.map_err(driver)?;
        if !ready {
            // Not collected yet; the flight goes back.
            self.flight = Some(flight);
            return Ok(None);
        }

        // **Read after the fence and not before.** The copy is the last thing
        // in a conversion's recording, so the fence being signalled is exactly
        // what says these eight bytes are that submission's rather than the
        // previous one's. A poke leaves them partial, which is fine: its
        // caller discards the digest.
        // SAFETY: mapped for the life of the converter, coherent, and eight
        // bytes long; the fence firing ordered the write that filled it.
        let bytes = unsafe { core::slice::from_raw_parts(self.mapped, DIGEST) };
        let mut value = [0u8; DIGEST];
        value.copy_from_slice(bytes);

        // SAFETY: the fence fired, so nothing submitted refers to the view.
        let Flight::Convert(view) = flight;
        unsafe { device.device.destroy_image_view(view, None) };
        Ok(Some(Digest(u64::from_le_bytes(value))))
    }

    /// Point the descriptor set at one source and one target.
    ///
    /// The set itself is allocated once and rewritten per submission, which
    /// removes two driver calls per frame; the one-in-flight rule is what
    /// makes rewriting safe, because nothing submitted can still be reading
    /// it.
    fn bind_set(
        &self,
        device: &Device,
        source_view: vk::ImageView,
        target: &TargetRef,
    ) -> Result<(), Error> {
        let sampled = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(source_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let luma = [vk::DescriptorImageInfo::default()
            .image_view(*target.planes.first().ok_or(Error::BadShader)?)
            .image_layout(vk::ImageLayout::GENERAL)];
        let chroma = [vk::DescriptorImageInfo::default()
            .image_view(*target.planes.get(1).ok_or(Error::BadShader)?)
            .image_layout(vk::ImageLayout::GENERAL)];
        // **Null for the subsampled layout, whose entry point never reads the
        // binding.** The descriptor is still written so one set serves both
        // pipelines; a binding no entry point statically uses may hold
        // nothing.
        let cr = [vk::DescriptorImageInfo::default()
            .image_view(
                target
                    .planes
                    .get(2)
                    .copied()
                    .unwrap_or(vk::ImageView::null()),
            )
            .image_layout(vk::ImageLayout::GENERAL)];
        let summary = [vk::DescriptorBufferInfo::default()
            .buffer(self.summary)
            .offset(0)
            .range(DIGEST_BYTES)];
        let packed = [vk::DescriptorImageInfo::default()
            .image_view(*target.planes.first().ok_or(Error::BadShader)?)
            .image_layout(vk::ImageLayout::GENERAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&sampled),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&luma),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&chroma),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&cr),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&summary),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&packed),
        ];
        // SAFETY: every borrowed structure outlives the call, and the set is
        // idle because its previous submission's fence has fired.
        unsafe { device.device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    fn dispatch(
        &mut self,
        device: &Device,
        source: &Imported,
        target: &TargetRef,
        view: vk::ImageView,
        dither: bool,
        groups: Option<(u32, u32)>,
    ) -> Result<(), Error> {
        self.bind_set(device, view, target)?;
        self.record(device, source, target, dither, groups)
    }

    fn record(
        &self,
        device: &Device,
        source: &Imported,
        target: &TargetRef,
        dither: bool,
        groups: Option<(u32, u32)>,
    ) -> Result<(), Error> {
        let vk_device = &device.device;
        let commands = self.command;

        // **Reset rather than recreated.** The buffer is one for the life of
        // the converter, and the flight rule guarantees its previous
        // submission has completed and been collected before this runs.
        // SAFETY: the buffer is this device's and not pending.
        unsafe { vk_device.reset_command_buffer(commands, vk::CommandBufferResetFlags::empty()) }
            .map_err(driver)?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: just reset and not recording.
        unsafe { vk_device.begin_command_buffer(commands, &begin) }.map_err(driver)?;

        let whole = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // The capture was last written by the display, which is not this
        // interface. Naming that as the previous owner is what makes its
        // contents legible.
        let acquire = vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(if source.foreign {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_queue_family_index(if source.foreign {
                device.queue_family
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .image(source.image)
            .subresource_range(whole);
        // The target's previous contents are nobody's -- this conversion
        // overwrites the whole picture -- so discarding them is correct. The
        // same image appearing in more than one slot transitions once, not
        // once per view.
        let mut prepare_images = [
            Some(target.luma_image),
            Some(target.chroma_image),
            Some(target.cr_image),
        ];
        for slot in 1..prepare_images.len() {
            if prepare_images[..slot].contains(&prepare_images[slot]) {
                prepare_images[slot] = None;
            }
        }
        let prepare: Vec<vk::ImageMemoryBarrier<'_>> = prepare_images
            .into_iter()
            .flatten()
            .map(|image| {
                vk::ImageMemoryBarrier::default()
                    .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(whole)
            })
            .collect();
        let mut opening_barriers = Vec::with_capacity(3);
        opening_barriers.push(acquire);
        opening_barriers.extend(prepare);

        let params = Params {
            width: i32::try_from(source.width).unwrap_or(i32::MAX),
            height: i32::try_from(source.height).unwrap_or(i32::MAX),
            dither: u32::from(dither),
            ten_bit: u32::from(target.depth.ten_bit()),
        };
        // SAFETY: `Params` is a plain repr(C) value of four four-byte fields
        // with no padding and no pointers, so its bytes are its representation.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(&params).cast::<u8>(),
                size_of::<Params>(),
            )
        };

        // One invocation per 2x2 block for the subsampled entry, one per pixel
        // for the full-chroma one, rounded up so an odd edge is covered. A
        // poke overrides this with a single workgroup: it needs the block
        // awake, not the picture converted.
        let (groups_x, groups_y) = groups.unwrap_or_else(|| match target.kind {
            Layout::SemiPlanar420 => (
                source.width.div_ceil(2).div_ceil(GROUP),
                source.height.div_ceil(2).div_ceil(GROUP),
            ),
            Layout::Planar444 | Layout::Packed444 => {
                (source.width.div_ceil(GROUP), source.height.div_ceil(GROUP))
            }
        });

        // **Zeroed here rather than on the processor**, so nothing has to wait
        // between clearing it and filling it: both are in this recording and
        // the barrier orders them.
        // SAFETY: recording, and the buffer is this device's.
        unsafe { vk_device.cmd_fill_buffer(commands, self.summary, 0, DIGEST_BYTES, 0) };
        let cleared = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.summary)
            .offset(0)
            .size(DIGEST_BYTES);
        // SAFETY: recording, and the barrier outlives the call.
        unsafe {
            vk_device.cmd_pipeline_barrier(
                commands,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[cleared],
                &[],
            );
        }

        // SAFETY: recording into a command buffer that has begun; every
        // borrowed structure outlives the call, and the handles are all this
        // device's.
        unsafe {
            vk_device.cmd_pipeline_barrier(
                commands,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &opening_barriers,
            );
            let pipeline = match target.kind {
                Layout::SemiPlanar420 => self.pipeline,
                Layout::Planar444 => self.pipeline_444,
                Layout::Packed444 => self.pipeline_packed,
            };
            vk_device.cmd_bind_pipeline(commands, vk::PipelineBindPoint::COMPUTE, pipeline);
            vk_device.cmd_bind_descriptor_sets(
                commands,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.set],
                &[],
            );
            vk_device.cmd_push_constants(
                commands,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytes,
            );
            vk_device.cmd_dispatch(commands, groups_x, groups_y, 1);
        }

        // **The picture is handed over in the layout its reader expects.**
        // An owned target stays writable and nothing is recorded; a lent
        // picture names its encoder's layout, and the transition rides in the
        // writer's own recording so the reader never touches a picture it
        // does not own.
        if target.final_layout != vk::ImageLayout::GENERAL {
            let mut handover_images = [
                Some(target.luma_image),
                Some(target.chroma_image),
                Some(target.cr_image),
            ];
            for slot in 1..handover_images.len() {
                if handover_images[..slot].contains(&handover_images[slot]) {
                    handover_images[slot] = None;
                }
            }
            let handover: Vec<vk::ImageMemoryBarrier<'_>> = handover_images
                .into_iter()
                .flatten()
                .map(|image| {
                    vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(target.final_layout)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(whole)
                })
                .collect();
            // SAFETY: recording; the barriers outlive the call.
            unsafe {
                vk_device.cmd_pipeline_barrier(
                    commands,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &handover,
                );
            }
        }

        // **Out of device memory once, rather than accumulated where the host
        // can see it.** The shader touches the summary thousands of times a
        // frame; this copy touches it once.
        let written = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.summary)
            .offset(0)
            .size(DIGEST_BYTES);
        let region = vk::BufferCopy::default().size(DIGEST_BYTES);
        // SAFETY: recording; every handle is this device's and the borrowed
        // structures outlive the calls.
        unsafe {
            vk_device.cmd_pipeline_barrier(
                commands,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[written],
                &[],
            );
            vk_device.cmd_copy_buffer(commands, self.summary, self.readback, &[region]);
            vk_device.end_command_buffer(commands).map_err(driver)?;
        }

        // **Reset rather than created.** The previous submission left it
        // signalled, and a submit will not signal a fence that already is. The
        // flight rule guarantees the previous submission was collected first.
        // SAFETY: the previous poll saw the fence fired, so nothing is using
        // it.
        unsafe { vk_device.reset_fences(&[self.fence]) }.map_err(driver)?;
        // **A signalled fence here makes the poll a no-op**, and the caller
        // would hand a half-written picture to an encoder reading it from
        // another queue. Nothing on this queue would notice, because a read
        // submitted after the conversion is ordered behind it anyway; only the
        // handover out of this interface is exposed to it.
        // SAFETY: the fence is this device's and was just reset.
        debug_assert!(
            matches!(unsafe { vk_device.get_fence_status(self.fence) }, Ok(false)),
            "the conversion fence was signalled at submit"
        );
        let submitted = [commands];
        let submits = [vk::SubmitInfo::default().command_buffers(&submitted)];
        // SAFETY: everything borrowed outlives the submit, and the command
        // buffer stays live until the fence fires and the poll collects it.
        unsafe {
            vk_device
                .queue_submit(device.queue, &submits, self.fence)
                .map_err(driver)
        }
    }
}

impl Device {
    /// Copy a converted frame out into ordinary memory.
    ///
    /// **A diagnostic, like the import's readback.** The loop never does this;
    /// the encoder takes the image on the device. It exists because a colour
    /// transform that is wrong in the matrix or in the range still produces a
    /// picture, and only comparing one against its source says which.
    ///
    /// Returns the luma plane and the interleaved chroma plane, each tightly
    /// packed.
    pub fn read_nv12(&self, nv12: &Nv12) -> Result<(Vec<u8>, Vec<u8>), Error> {
        // **Colour stays half of luma at either depth**, because the plane is
        // a quarter the samples and twice the width each. Only the luma figure
        // moves, which is why this is the one multiplication here.
        let luma_bytes = u64::from(nv12.width)
            * u64::from(nv12.height)
            * u64::from(nv12.depth.bytes_per_sample());
        let chroma_bytes = luma_bytes / 2;

        let info = vk::BufferCreateInfo::default()
            .size(luma_bytes + chroma_bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { self.device.create_buffer(&info, None) }.map_err(driver)?;

        let result = self.read_planes(
            &[
                (nv12.luma_image, nv12.width, nv12.height),
                (nv12.chroma_image, nv12.width / 2, nv12.height / 2),
            ],
            &[0, luma_bytes, luma_bytes + chroma_bytes],
            buffer,
        );
        // SAFETY: created above; the copy inside waited before returning.
        unsafe { self.device.destroy_buffer(buffer, None) };
        let all = result?;
        let split = usize::try_from(luma_bytes).unwrap_or(0);
        Ok((
            all.get(..split).unwrap_or_default().to_vec(),
            all.get(split..).unwrap_or_default().to_vec(),
        ))
    }

    /// The same diagnostic for a full-chroma frame: three full-resolution
    /// planes, luma then blue difference then red difference.
    pub fn read_planar_444(&self, frame: &Planar444) -> Result<[Vec<u8>; 3], Error> {
        let plane_bytes = u64::from(frame.width)
            * u64::from(frame.height)
            * u64::from(frame.depth.bytes_per_sample());
        let info = vk::BufferCreateInfo::default()
            .size(plane_bytes * 3)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { self.device.create_buffer(&info, None) }.map_err(driver)?;

        let result = self.read_planes(
            &[
                (frame.luma_image, frame.width, frame.height),
                (frame.cb_image, frame.width, frame.height),
                (frame.cr_image, frame.width, frame.height),
            ],
            &[0, plane_bytes, plane_bytes * 2, plane_bytes * 3],
            buffer,
        );
        // SAFETY: created above; the copy inside waited before returning.
        unsafe { self.device.destroy_buffer(buffer, None) };
        let all = result?;
        let (one, two) = (
            usize::try_from(plane_bytes).unwrap_or(0),
            usize::try_from(plane_bytes * 2).unwrap_or(0),
        );
        Ok([
            all.get(..one).unwrap_or_default().to_vec(),
            all.get(one..two).unwrap_or_default().to_vec(),
            all.get(two..).unwrap_or_default().to_vec(),
        ])
    }

    /// The same diagnostic for a packed full-chroma frame: the one word
    /// plane, which the caller may unpack with the orders the shader wrote.
    pub fn read_packed_444(&self, frame: &Packed444) -> Result<Vec<u8>, Error> {
        let bytes = u64::from(frame.pitch) * u64::from(frame.height);
        let info = vk::BufferCreateInfo::default()
            .size(bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { self.device.create_buffer(&info, None) }.map_err(driver)?;

        let result = self.read_planes(
            &[(frame.image, frame.width, frame.height)],
            &[0, bytes],
            buffer,
        );
        // SAFETY: created above; the copy inside waited before returning.
        unsafe { self.device.destroy_buffer(buffer, None) };
        result
    }

    /// Copy whole planes into one buffer, in the order and at the offsets
    /// given, and read the whole of it back.
    ///
    /// `splits` names one more offset than there are planes: the first is
    /// zero and the last is the buffer length, so the caller states where
    /// each plane goes and the lengths are derived rather than repeated.
    fn read_planes(
        &self,
        planes: &[(vk::Image, u32, u32)],
        splits: &[u64],
        buffer: vk::Buffer,
    ) -> Result<Vec<u8>, Error> {
        // SAFETY: the buffer was created on this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let index = self.host_visible_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let outcome = (|| -> Result<Vec<u8>, Error> {
            // SAFETY: neither handle is bound yet.
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.map_err(driver)?;
            self.copy_planes(planes, splits, buffer)?;

            let total = splits.last().copied().unwrap_or(0);
            // SAFETY: host visible, nothing else maps it, and the copy above
            // completed before this line.
            let mapped = unsafe {
                self.device
                    .map_memory(memory, 0, total, vk::MemoryMapFlags::empty())
            }
            .map_err(driver)?;
            let length = usize::try_from(total).unwrap_or(0);
            // SAFETY: the driver returned a mapping of at least `total` bytes,
            // valid until the unmap below.
            let all = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), length) };
            let copied = all.to_vec();
            // SAFETY: mapped just above and not referenced after this.
            unsafe { self.device.unmap_memory(memory) };
            Ok(copied)
        })();

        // SAFETY: nothing submitted still refers to it.
        unsafe { self.device.free_memory(memory, None) };
        outcome
    }

    /// Copy each named plane into the buffer at its offset, luma order.
    fn copy_planes(
        &self,
        planes: &[(vk::Image, u32, u32)],
        splits: &[u64],
        buffer: vk::Buffer,
    ) -> Result<(), Error> {
        let pool_info = vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family);
        // SAFETY: the create info outlives the call.
        let pool = unsafe { self.device.create_command_pool(&pool_info, None) }.map_err(driver)?;

        let outcome = (|| -> Result<(), Error> {
            let allocate = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: the allocate info outlives the call.
            let buffers =
                unsafe { self.device.allocate_command_buffers(&allocate) }.map_err(driver)?;
            let commands = *buffers.first().ok_or(Error::NoQueue)?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: freshly allocated and not recording.
            unsafe { self.device.begin_command_buffer(commands, &begin) }.map_err(driver)?;

            // The conversion wrote it and this reads it, so the write has to be
            // made visible before the copy rather than merely have happened.
            let settle: Vec<vk::ImageMemoryBarrier<'_>> = planes
                .iter()
                .map(|(image, _, _)| {
                    vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(*image)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                })
                .collect();

            let plain = |aspect| vk::ImageSubresourceLayers {
                aspect_mask: aspect,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            };

            // SAFETY: recording into a begun command buffer with borrowed
            // structures that outlive the call.
            unsafe {
                self.device.cmd_pipeline_barrier(
                    commands,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &settle,
                );
                for (at, (image, width, height)) in planes.iter().enumerate() {
                    let region = [vk::BufferImageCopy::default()
                        .buffer_offset(splits.get(at).copied().unwrap_or(0))
                        .image_subresource(plain(vk::ImageAspectFlags::COLOR))
                        .image_extent(vk::Extent3D {
                            width: *width,
                            height: *height,
                            depth: 1,
                        })];
                    self.device.cmd_copy_image_to_buffer(
                        commands,
                        *image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        buffer,
                        &region,
                    );
                }
                self.device.end_command_buffer(commands).map_err(driver)?;
            }

            // SAFETY: the create info outlives the call.
            let fence = unsafe {
                self.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            }
            .map_err(driver)?;
            let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
            // SAFETY: borrowed structures outlive the wait below.
            let waited = unsafe {
                self.device
                    .queue_submit(self.queue, &submits, fence)
                    .map_err(driver)
                    .and_then(|()| {
                        self.device
                            .wait_for_fences(&[fence], true, u64::MAX)
                            .map_err(driver)
                    })
            };
            // SAFETY: signalled or never submitted.
            unsafe { self.device.destroy_fence(fence, None) };
            waited
        })();

        // SAFETY: the submit waited, so nothing recorded is still running.
        unsafe { self.device.destroy_command_pool(pool, None) };
        outcome
    }
}

#[cfg(test)]
mod tests {

    /// Both descriptor kinds export, on every display device present.
    ///
    /// **This exists because a change to what an allocation declares took
    /// capture out on one card and passed everything here.** Nothing else in
    /// the suite exports a picture: the conversion tests convert and compare,
    /// and the fault reached a person as an output that would not switch, with
    /// a message naming neither the handle kind nor the card.
    #[test]
    #[ignore = "requires a display device"]
    fn a_conversion_target_exports_both_ways_on_every_device() {
        let mut seen = 0;
        for index in 0..8 {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            let Ok(device) = crate::vulkan::Device::for_display(&node) else {
                continue;
            };
            // **Both depths, because a ten-bit target is a different set of
            // formats and a device may export one and refuse the other.** That
            // is a refusal an encoder would meet at the handover, where the
            // message names neither the depth nor the card.
            for depth in [crate::convert::Depth::Eight, crate::convert::Depth::Ten] {
                let target = device
                    .allocate_planar(64, 64, depth)
                    .unwrap_or_else(|error| panic!("{node:?} has no {depth:?} target: {error}"));
                for display_interface in [true, false] {
                    let got = device.export_nv12(&target, display_interface);
                    assert!(
                        got.is_ok(),
                        "{node:?} refused a {depth:?} picture with \
                         display_interface={display_interface}: {:?}",
                        got.err()
                    );
                }
                // The importer picks its layout from this and cannot work it
                // out from the offsets, so a wrong answer here is a driver
                // handed the other depth's four-character code.
                let (_fd, exported) = device
                    .export_nv12(&target, true)
                    .expect("a descriptor to describe");
                assert_eq!(exported.depth, depth, "{node:?} described the wrong depth");
                device.release_nv12(target);
            }
            println!("{node:?}: exports both ways at both depths");
            seen += 1;
        }
        assert!(
            seen > 0,
            "no display device answered, so nothing was tested"
        );
    }
    use std::io::Seek;

    use super::*;

    /// The colour transform, computed on the processor from the same rules the
    /// shader implements, written from the definitions rather than transcribed
    /// from the shader so a mistake cannot be shared by both.
    fn reference(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let (rf, gf, bf) = (
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        );
        let kr = 0.2126_f64;
        let kb = 0.0722_f64;
        let kg = 1.0 - kr - kb;
        let y = kr * rf + kg * gf + kb * bf;
        let u = (bf - y) / (2.0 - 2.0 * kb);
        let v = (rf - y) / (2.0 - 2.0 * kr);
        // Walked rather than cast, because a cast here is exactly the lint the
        // data path forbids and this is eight comparisons in a test.
        let quantise = |value: f64| -> u8 {
            let scaled = (value * 255.0).round().clamp(0.0, 255.0);
            (0..=u8::MAX)
                .find(|candidate| f64::from(*candidate) >= scaled)
                .unwrap_or(u8::MAX)
        };
        (
            quantise(y * (219.0 / 255.0) + 16.0 / 255.0),
            quantise(u * (224.0 / 255.0) + 128.0 / 255.0),
            quantise(v * (224.0 / 255.0) + 128.0 / 255.0),
        )
    }

    /// The same transform at ten bits, and **not the eight-bit answer scaled**.
    ///
    /// The range constants are different numbers rather than one set
    /// multiplied out -- 235 of 255 is 0.92157 and 940 of 1023 is 0.91887 --
    /// so a reference that scaled would agree with a conversion that made the
    /// same mistake and prove nothing.
    fn reference_ten(r: u8, g: u8, b: u8) -> (u16, u16, u16) {
        let (rf, gf, bf) = (
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        );
        let kr = 0.2126_f64;
        let kb = 0.0722_f64;
        let kg = 1.0 - kr - kb;
        let y = kr * rf + kg * gf + kb * bf;
        let u = (bf - y) / (2.0 - 2.0 * kb);
        let v = (rf - y) / (2.0 - 2.0 * kr);
        let quantise = |value: f64| -> u16 {
            let scaled = (value * 1023.0).round().clamp(0.0, 1023.0);
            (0..=1023u16)
                .find(|candidate| f64::from(*candidate) >= scaled)
                .unwrap_or(1023)
        };
        (
            quantise(y * (876.0 / 1023.0) + 64.0 / 1023.0),
            quantise(u * (896.0 / 1023.0) + 512.0 / 1023.0),
            quantise(v * (896.0 / 1023.0) + 512.0 / 1023.0),
        )
    }

    /// **The ten-bit conversion lands on the ten-bit reference**, sample for
    /// sample, with the samples read where an encoder reads them.
    ///
    /// This is the check the depth needed and did not have. Two faults it
    /// catches, both of which shipped for a day and neither of which refuses
    /// anywhere: a plane written through a view narrower than its image, which
    /// fills one byte of every two-sample word and leaves half the picture
    /// untouched; and samples placed in the low ten bits of the word instead of
    /// the high ten, which decodes to a picture that is merely dark.
    #[test]
    fn the_ten_bit_conversion_matches_the_ten_bit_reference() {
        let width = u32::try_from(PATTERN.len()).unwrap() * 2;
        let height = 2;

        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        for (block, colour) in PATTERN.iter().enumerate() {
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let at = (dy * (width as usize) + block * 2 + dx) * 4;
                    pixels[at] = colour[0];
                    pixels[at + 1] = colour[1];
                    pixels[at + 2] = colour[2];
                    pixels[at + 3] = 255;
                }
            }
        }

        let device = Device::any().expect("a device that can convert");
        let source = device
            .upload_rgba(width, height, &pixels)
            .expect("upload the pattern");
        let target = device
            .allocate_planar(width, height, Depth::Ten)
            .expect("a ten-bit target");
        let mut converter = Converter::new(&device).expect("a pipeline");
        converter
            .run(&device, &source, &target.target(), false)
            .expect("convert");
        let (luma, chroma) = device.read_nv12(&target).expect("read the planes");

        // **Read as an encoder reads them**: little-endian words with the
        // sample in the high ten bits. Reading them any other way would agree
        // with a conversion that wrote them any other way.
        let sample = |plane: &[u8], at: usize| -> u16 {
            u16::from_le_bytes([plane[at * 2], plane[at * 2 + 1]]) >> 6
        };

        for (block, colour) in PATTERN.iter().enumerate() {
            let (want_y, want_u, want_v) = reference_ten(colour[0], colour[1], colour[2]);
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let at = dy * (width as usize) + block * 2 + dx;
                    let got = sample(&luma, at);
                    assert!(
                        got.abs_diff(want_y) <= 2,
                        "block {block} luma {got} wanted {want_y}"
                    );
                }
            }
            let at = block * 2;
            let (got_u, got_v) = (sample(&chroma, at), sample(&chroma, at + 1));
            assert!(
                got_u.abs_diff(want_u) <= 2 && got_v.abs_diff(want_v) <= 2,
                "block {block} chroma {got_u} {got_v} wanted {want_u} {want_v}"
            );
        }
    }

    /// The full-chroma conversion lands on the reference at both depths,
    /// sample for sample, with nothing averaged.
    ///
    /// **This is what the full-chroma entry exists for**: each pixel keeps
    /// its own colour planes, so the check is per pixel rather than per
    /// block, and a body that quietly fell back to the subsampled one would
    /// fail it on every block boundary. Runs by default for the same reason
    /// the subsampled one does: it needs a driver but not a card.
    #[test]
    fn the_444_conversion_matches_the_reference_at_both_depths() {
        for depth in [Depth::Eight, Depth::Ten] {
            let width = u32::try_from(PATTERN.len()).unwrap() * 2;
            let height = 2;

            let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
            for (block, colour) in PATTERN.iter().enumerate() {
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let at = (dy * (width as usize) + block * 2 + dx) * 4;
                        pixels[at] = colour[0];
                        pixels[at + 1] = colour[1];
                        pixels[at + 2] = colour[2];
                        pixels[at + 3] = 255;
                    }
                }
            }

            let device = Device::any().expect("a device that can convert");
            let source = device
                .upload_rgba(width, height, &pixels)
                .expect("upload the pattern");
            let target = device
                .allocate_planar_444(width, height, depth)
                .expect("a full-chroma target");
            let mut converter = Converter::new(&device).expect("a pipeline");
            converter
                .run(&device, &source, &target.target(), false)
                .expect("convert");
            let [luma, cb, cr] = device.read_planar_444(&target).expect("read the planes");

            for (block, colour) in PATTERN.iter().enumerate() {
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let at = dy * (width as usize) + block * 2 + dx;
                        let (got_y, got_u, got_v, want_y, want_u, want_v) = match depth {
                            Depth::Eight => {
                                let byte = |plane: &[u8], at: usize| plane[at];
                                let (want_y, want_u, want_v) =
                                    reference(colour[0], colour[1], colour[2]);
                                (
                                    u16::from(byte(&luma, at)),
                                    u16::from(byte(&cb, at)),
                                    u16::from(byte(&cr, at)),
                                    u16::from(want_y),
                                    u16::from(want_u),
                                    u16::from(want_v),
                                )
                            }
                            Depth::Ten => {
                                let sample = |plane: &[u8], at: usize| -> u16 {
                                    u16::from_le_bytes([plane[at * 2], plane[at * 2 + 1]]) >> 6
                                };
                                let (want_y, want_u, want_v) =
                                    reference_ten(colour[0], colour[1], colour[2]);
                                (
                                    sample(&luma, at),
                                    sample(&cb, at),
                                    sample(&cr, at),
                                    want_y,
                                    want_u,
                                    want_v,
                                )
                            }
                        };
                        assert!(
                            got_y.abs_diff(want_y) <= 2,
                            "block {block} {depth:?} luma {got_y} wanted {want_y}"
                        );
                        assert!(
                            got_u.abs_diff(want_u) <= 2 && got_v.abs_diff(want_v) <= 2,
                            "block {block} {depth:?} chroma {got_u} {got_v} \
                             wanted {want_u} {want_v}"
                        );
                    }
                }
            }
        }
    }

    /// The packed full-chroma conversion composes the documented word at both
    /// depths.
    ///
    /// **This checks what this side writes, not what an importer reads.** The
    /// packed orders are the importer's own documented layout, and the only
    /// check that catches a backwards order is the decoder comparison the
    /// phase gate runs; this test pins the composition so that at least one
    /// side of the contract is held still while the other is verified.
    #[test]
    fn the_packed_444_conversion_matches_the_reference_at_both_depths() {
        for depth in [Depth::Eight, Depth::Ten] {
            let width = u32::try_from(PATTERN.len()).unwrap() * 2;
            let height = 2;

            let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
            for (block, colour) in PATTERN.iter().enumerate() {
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let at = (dy * (width as usize) + block * 2 + dx) * 4;
                        pixels[at] = colour[0];
                        pixels[at + 1] = colour[1];
                        pixels[at + 2] = colour[2];
                        pixels[at + 3] = 255;
                    }
                }
            }

            let device = Device::any().expect("a device that can convert");
            let source = device
                .upload_rgba(width, height, &pixels)
                .expect("upload the pattern");
            let target = device
                .allocate_packed_444(width, height, depth)
                .expect("a packed full-chroma target");
            let mut converter = Converter::new(&device).expect("a pipeline");
            converter
                .run(&device, &source, &target.target(), false)
                .expect("convert");
            let words = device.read_packed_444(&target).expect("read the plane");

            for (block, colour) in PATTERN.iter().enumerate() {
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let at = dy * (width as usize) + block * 2 + dx;
                        let word = u32::from_le_bytes(
                            words
                                .get(at * 4..at * 4 + 4)
                                .expect("a word per pixel")
                                .try_into()
                                .expect("four bytes"),
                        );
                        let (got_y, got_u, got_v, want_y, want_u, want_v) = match depth {
                            Depth::Eight => {
                                let (want_y, want_u, want_v) =
                                    reference(colour[0], colour[1], colour[2]);
                                (
                                    (word >> 16) & 0xff,
                                    (word >> 8) & 0xff,
                                    word & 0xff,
                                    u32::from(want_y),
                                    u32::from(want_u),
                                    u32::from(want_v),
                                )
                            }
                            Depth::Ten => {
                                let (want_y, want_u, want_v) =
                                    reference_ten(colour[0], colour[1], colour[2]);
                                (
                                    (word >> 10) & 0x3ff,
                                    word & 0x3ff,
                                    (word >> 20) & 0x3ff,
                                    u32::from(want_y),
                                    u32::from(want_u),
                                    u32::from(want_v),
                                )
                            }
                        };
                        assert!(
                            got_y.abs_diff(want_y) <= 2,
                            "block {block} {depth:?} luma {got_y} wanted {want_y}"
                        );
                        assert!(
                            got_u.abs_diff(want_u) <= 2 && got_v.abs_diff(want_v) <= 2,
                            "block {block} {depth:?} chroma {got_u} {got_v} \
                             wanted {want_u} {want_v}"
                        );
                    }
                }
            }
        }
    }

    /// Saturated colours, each filling a whole 2x2 block so subsampling has
    /// nothing to average and the answer is exact.
    ///
    /// **Grey is deliberately not the whole list.** A grey pixel has equal
    /// channels, so every luma matrix returns the same luma for it and it
    /// carries no chroma at all; a test made of greys passes with the wrong
    /// coefficients, which is exactly how the desktop comparison nearly fooled
    /// us.
    const PATTERN: [[u8; 3]; 8] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
        [255, 255, 255],
        [0, 0, 0],
    ];

    /// The conversion agrees with the definition, on colours that can tell one
    /// matrix from another.
    ///
    /// **Runs by default, unlike the other tests that touch a device.** It
    /// needs a driver but not a graphics card: it passes on the software one
    /// with the same exactness, in a tenth of a second, which is why continuous
    /// integration installs a loader rather than skipping this. A colour
    /// regression has no other check that can catch it -- the comparison
    /// against a real desktop is nearly blind to the matrix, which is what
    /// prompted this.
    /// **The summary answers the only question asked of it**: is this picture
    /// the one before it.
    ///
    /// Three things have to hold together, and each fails differently. The
    /// same picture twice must agree, or the loop suppresses nothing. Two
    /// different pictures must disagree, or it suppresses a real frame. And
    /// **the same pixels in a different place must disagree** -- a sum and an
    /// exclusive-or are both blind to order, so without position folded in, a
    /// picture and its mirror reduce to the same value.
    #[test]
    fn the_summary_tells_one_picture_from_another() {
        let width = 64u32;
        let height = 64u32;
        let device = Device::any().expect("a device that can convert");
        let mut converter = Converter::new(&device).expect("a pipeline");
        let target = device.allocate_nv12(width, height).expect("a target");

        let mut digest_of = |pixels: &[u8]| {
            let source = device.upload_rgba(width, height, pixels).expect("upload");
            let got = converter
                .run(&device, &source, &target.target(), false)
                .expect("convert");
            device.release(source);
            got
        };

        let flat = vec![40u8; (width as usize) * (height as usize) * 4];
        let first = digest_of(&flat);
        let again = digest_of(&flat);
        assert_eq!(
            first, again,
            "the same picture twice gave two answers, so nothing could ever be suppressed"
        );

        // One pixel, one level. The smallest change the eight-bit output can
        // carry, and the one a weak summary misses.
        let mut nudged = flat.clone();
        nudged[0] = 41;
        nudged[1] = 41;
        nudged[2] = 41;
        let moved_one = digest_of(&nudged);
        assert_ne!(
            first, moved_one,
            "a picture with one pixel changed matched the one without it"
        );

        // **One pixel bright, moved to the same corner of the next block.**
        // Two pixels apart, so both are the first sample of their own 2x2
        // block: the four luma values pack identically and the chroma is the
        // same, and the only thing separating the two pictures is which block
        // it happened in. Moving the pixel anywhere else is a weaker test,
        // because its position inside the block changes the packing and would
        // pass even with the block's own position left out.
        let bright = |at: usize| {
            let mut pixels = flat.clone();
            pixels[at * 4] = 200;
            pixels[at * 4 + 1] = 200;
            pixels[at * 4 + 2] = 200;
            pixels
        };
        assert_ne!(
            digest_of(&bright(0)),
            digest_of(&bright(2)),
            "the same pixel in a different block gave the same answer, so the block's position \
             is not reaching the summary and a picture could move without being noticed"
        );

        device.release_nv12(target);
        converter.destroy(&device);
    }

    /// **One converter, used more than once**, which is the shape the loop
    /// runs in and which nothing covered while the fence was created per run.
    /// The second conversion is the one that matters: it is the first that can
    /// meet a fence the previous run left signalled.
    #[test]
    fn a_converter_is_correct_the_second_time_it_is_used() {
        let width = u32::try_from(PATTERN.len()).unwrap() * 2;
        let height = 2;
        let device = Device::any().expect("a device that can convert");
        let mut converter = Converter::new(&device).expect("a pipeline");

        // Two different pictures, so a target left holding the first is not
        // mistaken for a correct second conversion.
        for (round, fill) in [0u8, 255u8].into_iter().enumerate() {
            let pixels = vec![fill; (width as usize) * (height as usize) * 4];
            let source = device
                .upload_rgba(width, height, &pixels)
                .expect("upload the pattern");
            let target = device.allocate_nv12(width, height).expect("a target");
            converter
                .run(&device, &source, &target.target(), false)
                .expect("convert");
            let (luma, _) = device.read_nv12(&target).expect("read the planes");

            // Limited range: black is 16 and white is 235.
            let wanted = if fill == 0 { 16u8 } else { 235u8 };
            let worst = luma
                .iter()
                .map(|got| got.abs_diff(wanted))
                .max()
                .unwrap_or(0);
            assert!(
                worst <= 2,
                "round {round}: luma is {worst} off {wanted}, so the picture handed back was \
                 not the one just converted"
            );
            device.release_nv12(target);
            device.release(source);
        }
        converter.destroy(&device);
    }

    #[test]
    fn conversion_matches_the_reference() {
        let width = u32::try_from(PATTERN.len()).unwrap() * 2;
        let height = 2;

        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        for (block, colour) in PATTERN.iter().enumerate() {
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let x = block * 2 + dx;
                    let at = (dy * (width as usize) + x) * 4;
                    pixels[at] = colour[0];
                    pixels[at + 1] = colour[1];
                    pixels[at + 2] = colour[2];
                    pixels[at + 3] = 255;
                }
            }
        }

        let device = Device::any().expect("a device that can convert");
        let source = device
            .upload_rgba(width, height, &pixels)
            .expect("upload the pattern");
        let target = device.allocate_nv12(width, height).expect("a target");
        let mut converter = Converter::new(&device).expect("a pipeline");
        converter
            .run(&device, &source, &target.target(), false)
            .expect("convert");
        let (luma, chroma) = device.read_nv12(&target).expect("read the planes");

        let mut worst = 0u8;
        for (block, colour) in PATTERN.iter().enumerate() {
            let (want_y, want_u, want_v) = reference(colour[0], colour[1], colour[2]);
            // Every luma sample in the block is the same colour, so all four
            // must land on the same answer.
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let at = dy * (width as usize) + block * 2 + dx;
                    let got = luma[at];
                    worst = worst.max(got.abs_diff(want_y));
                    assert!(
                        got.abs_diff(want_y) <= 1,
                        "block {block} luma {got} wanted {want_y}"
                    );
                }
            }
            let at = block * 2;
            worst = worst.max(chroma[at].abs_diff(want_u));
            worst = worst.max(chroma[at + 1].abs_diff(want_v));
            assert!(
                chroma[at].abs_diff(want_u) <= 1 && chroma[at + 1].abs_diff(want_v) <= 1,
                "block {block} chroma {} {} wanted {want_u} {want_v}",
                chroma[at],
                chroma[at + 1]
            );
        }
        println!(
            "worst disagreement {worst} of 255, over {} colours",
            PATTERN.len()
        );

        converter.destroy(&device);
        device.release_nv12(target);
        device.release(source);
    }

    /// The exported descriptor carries the layout an encoder reads.
    ///
    /// **The colour plane must begin exactly one luma plane in.** An encoder
    /// registering a frame by pointer is given one address and one row length
    /// and assumes that; there is no field in which to tell it anything else.
    /// A driver left to lay out a two-plane image does not oblige, which is why
    /// the two planes are separate images bound at offsets of our choosing, and
    /// why this asserts equality rather than a bound.
    ///
    /// It still does not prove an encoder will accept the frame -- only an
    /// encoder producing a picture does that -- but it does catch the layout
    /// drifting from what the allocation was built to.
    ///
    /// Behind the flag, unlike the colour check: the software driver reports
    /// both planes starting at offset zero, which cannot describe one
    /// allocation, and handing a hardware encoder a frame is the entire point.
    ///
    ///   cargo test -p lowlat-capture --lib -- --ignored
    #[test]
    #[ignore = "needs a graphics device; the software one reports no plane offsets"]
    fn the_exported_layout_describes_the_descriptor() {
        let width = 256;
        let height = 128;
        let device = Device::any().expect("a device that can convert");
        let target = device.allocate_nv12(width, height).expect("a target");

        let (fd, layout) = device.export_nv12(&target, true).expect("export the frame");
        let size = std::fs::File::from(fd)
            .seek(std::io::SeekFrom::End(0))
            .expect("size of the exported descriptor");
        println!(
            "exported {size} bytes, luma offset {} pitch {}, chroma offset {} pitch {}",
            layout.planes[0].offset,
            layout.planes[0].pitch,
            layout.planes[1].offset,
            layout.planes[1].pitch
        );

        assert_eq!(layout.planes[0].offset, 0, "luma does not start the buffer");
        assert!(
            layout.planes[0].pitch >= width,
            "luma rows are shorter than the picture"
        );
        assert_eq!(
            layout.planes[1].offset,
            layout.planes[0].pitch * height,
            "colour must begin exactly one luma plane in, which is where an \
             encoder registering by pointer looks for it"
        );
        let needed = u64::from(layout.planes[1].offset)
            + u64::from(layout.planes[1].pitch) * u64::from(height / 2);
        assert!(
            size >= needed,
            "the descriptor is {size} bytes and the layout needs {needed}"
        );
        assert!(
            layout.planes[1].pitch >= width,
            "chroma rows are shorter than the picture"
        );

        device.release_nv12(target);
    }
}
