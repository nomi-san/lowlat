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

/// The compiled shader, committed rather than built.
///
/// A build-time shader compiler would be a dependency for something that
/// changes a handful of times in a project's life, and it would have to be
/// present on every machine that builds this. The source sits beside it and
/// `scripts/build-shaders.sh` regenerates it.
const CONVERT: &[u8] = include_bytes!("../shaders/convert.spv");

/// Invocations per workgroup, per axis. Each one owns a 2x2 block, so a group
/// covers 16 by 16 pixels.
const GROUP: u32 = 8;

/// What the shader is told, per dispatch.
///
/// Laid out to match the shader's own block exactly. Two signed extents then a
/// flag, which is twelve bytes and needs no padding on either side.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Params {
    width: i32,
    height: i32,
    dither: u32,
}

/// How a converted frame is laid out, for whatever imports it next.
#[derive(Debug, Clone, Copy)]
pub struct Exported {
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    /// Luma first, then the interleaved chroma.
    pub planes: [PlaneLayout; 2],
}

/// A converted frame: luma and chroma in one image, ready to hand on.
pub struct Nv12 {
    image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    /// One view per plane, single-component formats the device can write.
    planes: [vk::ImageView; 2],
    pub width: u32,
    pub height: u32,
}

impl core::fmt::Debug for Nv12 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nv12")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Everything the conversion needs that outlives a single frame.
pub struct Converter {
    sampler: vk::Sampler,
    layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    commands: vk::CommandPool,
}

impl core::fmt::Debug for Converter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converter").finish_non_exhaustive()
    }
}

impl Converter {
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
        ];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings outlive the call.
        let layout = unsafe { vk_device.create_descriptor_set_layout(&layout_info, None) }
            .map_err(driver)?;

        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(u32::try_from(size_of::<Params>()).unwrap_or(12))];
        let layouts = [layout];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&ranges);
        // SAFETY: both slices outlive the call.
        let pipeline_layout =
            unsafe { vk_device.create_pipeline_layout(&pipeline_layout_info, None) }
                .map_err(driver)?;

        let pipeline = Self::build_pipeline(vk_device, pipeline_layout)?;

        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes)
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET);
        // SAFETY: the sizes outlive the call.
        let pool = unsafe { vk_device.create_descriptor_pool(&pool_info, None) }.map_err(driver)?;

        let commands_info =
            vk::CommandPoolCreateInfo::default().queue_family_index(device.queue_family);
        // SAFETY: the create info outlives the call.
        let commands =
            unsafe { vk_device.create_command_pool(&commands_info, None) }.map_err(driver)?;

        Ok(Self {
            sampler,
            layout,
            pipeline_layout,
            pipeline,
            pool,
            commands,
        })
    }

    fn build_pipeline(
        device: &ash::Device,
        layout: vk::PipelineLayout,
    ) -> Result<vk::Pipeline, Error> {
        // The committed shader is a byte array; the interface wants words. A
        // misaligned or odd-length blob is a corrupt file rather than a
        // runtime condition, so it is refused rather than patched around.
        if CONVERT.len() % 4 != 0 {
            return Err(Error::BadShader);
        }
        let mut words = Vec::with_capacity(CONVERT.len() / 4);
        for chunk in CONVERT.chunks_exact(4) {
            let quad: [u8; 4] = chunk.try_into().map_err(|_| Error::BadShader)?;
            words.push(u32::from_le_bytes(quad));
        }

        let module_info = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: the words outlive the call.
        let module = unsafe { device.create_shader_module(&module_info, None) }.map_err(driver)?;

        let entry = c"main";
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
    pub fn destroy(self, device: &Device) {
        let vk_device = &device.device;
        // SAFETY: every handle came from this device and the wait means no
        // submitted work still refers to any of them.
        unsafe {
            let _ = vk_device.device_wait_idle();
            vk_device.destroy_command_pool(self.commands, None);
            vk_device.destroy_descriptor_pool(self.pool, None);
            vk_device.destroy_pipeline(self.pipeline, None);
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
        // Both dimensions round up to even. A plane at half resolution has no
        // meaning for an odd one, and the shader's last block would write
        // outside the chroma plane.
        let width = width.next_multiple_of(2);
        let height = height.next_multiple_of(2);

        // **Untiled, and handed out as a descriptor.** An encoder has to be
        // able to take this without a copy, and untiled is the one arrangement
        // every encoder here reads. The alternative -- writing into something
        // the device prefers and copying to something the encoder accepts --
        // is a whole frame moved per frame, which is the cost this path exists
        // to avoid. Whether writing untiled from a shader is measurably slower
        // than writing tiled is a question for a benchmark, not a guess.
        let modifiers = [LINEAR];
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
            // Mutable format and extended usage together are what let the
            // planes be written: the two-plane format reports no write support
            // under any modifier on any device here, while each of its
            // single-component planes reports it under all of them.
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list)
            .push_next(&mut external);
        // SAFETY: every borrowed structure outlives the call.
        let image = unsafe { self.device.create_image(&create, None) }.map_err(driver)?;

        match self.finish_nv12(image, width, height) {
            Ok(nv12) => Ok(nv12),
            Err(error) => {
                // SAFETY: nothing else refers to it; finishing is what failed.
                unsafe { self.device.destroy_image(image, None) };
                Err(error)
            }
        }
    }

    fn finish_nv12(&self, image: vk::Image, width: u32, height: u32) -> Result<Nv12, Error> {
        // SAFETY: the image was created on this device.
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let index = self.device_local_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        let mut exportable = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // The image is the only thing in this allocation, which is what an
        // interface taking it by descriptor expects.
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate = allocate
            .push_next(&mut exportable)
            .push_next(&mut dedicated);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;
        // SAFETY: neither handle is bound yet and both are this device's.
        if let Err(result) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            // SAFETY: nothing is bound to it.
            unsafe { self.device.free_memory(memory, None) };
            return Err(driver(result));
        }

        let mut planes = [vk::ImageView::null(); 2];
        for (at, (aspect, format)) in [
            (vk::ImageAspectFlags::PLANE_0, vk::Format::R8_UNORM),
            (vk::ImageAspectFlags::PLANE_1, vk::Format::R8G8_UNORM),
        ]
        .into_iter()
        .enumerate()
        {
            // The view's usage is narrowed to what this view is for. Without
            // it the view inherits the image's usage, which includes one the
            // two-plane format does not support and the driver rejects.
            let mut usage =
                vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::STORAGE);
            let info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: aspect,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .push_next(&mut usage);
            // SAFETY: the chain outlives the call.
            match unsafe { self.device.create_image_view(&info, None) } {
                Ok(view) => {
                    if let Some(slot) = planes.get_mut(at) {
                        *slot = view;
                    }
                }
                Err(result) => {
                    for view in planes
                        .into_iter()
                        .filter(|view| *view != vk::ImageView::null())
                    {
                        // SAFETY: created just above on this device.
                        unsafe { self.device.destroy_image_view(view, None) };
                    }
                    // SAFETY: nothing refers to them any more.
                    unsafe { self.device.free_memory(memory, None) };
                    return Err(driver(result));
                }
            }
        }

        Ok(Nv12 {
            image,
            memory,
            planes,
            width,
            height,
        })
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
    /// rather than moving it, so the image stays usable and both ends have to
    /// be released.
    ///
    /// The layout that comes back is not decoration. An importer is told where
    /// each plane starts and how long its rows are, and those are the driver's
    /// numbers rather than ones computed from the width: a plane is padded to
    /// whatever alignment it wants, and a reader walking rows by width lands
    /// progressively further into the wrong place.
    pub fn export_nv12(&self, nv12: &Nv12) -> Result<(OwnedFd, Exported), Error> {
        let external = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(nv12.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: the info outlives the call and the memory is this device's.
        let fd = unsafe { external.get_memory_fd(&info) }.map_err(driver)?;
        // SAFETY: the driver returned a fresh owned descriptor.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        let mut planes = [PlaneLayout {
            offset: 0,
            pitch: 0,
        }; 2];
        // **Memory planes, not image planes.** For an image laid out by a
        // display-interface modifier the two are addressed by different aspect
        // names, and asking with the wrong one is rejected rather than
        // answered approximately.
        for (at, aspect) in [
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        ]
        .into_iter()
        .enumerate()
        {
            let subresource = vk::ImageSubresource {
                aspect_mask: aspect,
                mip_level: 0,
                array_layer: 0,
            };
            // SAFETY: the image is this device's and was created with a
            // modifier, which is what makes these aspects valid.
            let layout = unsafe {
                self.device
                    .get_image_subresource_layout(nv12.image, subresource)
            };
            if let Some(slot) = planes.get_mut(at) {
                *slot = PlaneLayout {
                    offset: u32::try_from(layout.offset).unwrap_or(0),
                    pitch: u32::try_from(layout.row_pitch).unwrap_or(0),
                };
            }
        }

        Ok((
            fd,
            Exported {
                width: nv12.width,
                height: nv12.height,
                modifier: LINEAR,
                planes,
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
            self.device.destroy_image(nv12.image, None);
            self.device.free_memory(nv12.memory, None);
        }
    }
}

impl Converter {
    /// Convert one captured frame into the target.
    ///
    /// Blocks until the device has finished. That is right for a diagnostic and
    /// wrong for the loop, which will submit and poll; the recording below does
    /// not change when that happens, only the wait at the end.
    pub fn run(
        &self,
        device: &Device,
        source: &Imported,
        target: &Nv12,
        dither: bool,
    ) -> Result<(), Error> {
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

        let outcome = self.dispatch(device, source, target, view, dither);

        // SAFETY: created above; the dispatch waited, so nothing refers to it.
        unsafe { vk_device.destroy_image_view(view, None) };
        outcome
    }

    fn dispatch(
        &self,
        device: &Device,
        source: &Imported,
        target: &Nv12,
        view: vk::ImageView,
        dither: bool,
    ) -> Result<(), Error> {
        let vk_device = &device.device;

        let layouts = [self.layout];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pool)
            .set_layouts(&layouts);
        // SAFETY: the allocate info outlives the call.
        let sets = unsafe { vk_device.allocate_descriptor_sets(&allocate) }.map_err(driver)?;
        let set = *sets.first().ok_or(Error::NoMemoryType)?;

        let sampled = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let luma = [vk::DescriptorImageInfo::default()
            .image_view(*target.planes.first().ok_or(Error::BadShader)?)
            .image_layout(vk::ImageLayout::GENERAL)];
        let chroma = [vk::DescriptorImageInfo::default()
            .image_view(*target.planes.get(1).ok_or(Error::BadShader)?)
            .image_layout(vk::ImageLayout::GENERAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&sampled),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&luma),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&chroma),
        ];
        // SAFETY: every borrowed structure outlives the call.
        unsafe { vk_device.update_descriptor_sets(&writes, &[]) };

        let result = self.record(device, source, target, set, dither);

        // SAFETY: the record waited before returning, so the set is idle.
        unsafe { vk_device.free_descriptor_sets(self.pool, &sets) }.map_err(driver)?;
        result
    }

    fn record(
        &self,
        device: &Device,
        source: &Imported,
        target: &Nv12,
        set: vk::DescriptorSet,
        dither: bool,
    ) -> Result<(), Error> {
        let vk_device = &device.device;
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.commands)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the allocate info outlives the call.
        let buffers = unsafe { vk_device.allocate_command_buffers(&allocate) }.map_err(driver)?;
        let commands = *buffers.first().ok_or(Error::NoQueue)?;

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: freshly allocated and not recording.
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
        // The target has never been written, so its previous contents are
        // nobody's and discarding them is correct.
        let prepare = vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(target.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::PLANE_0 | vk::ImageAspectFlags::PLANE_1,
                ..whole
            });

        let params = Params {
            width: i32::try_from(source.width).unwrap_or(i32::MAX),
            height: i32::try_from(source.height).unwrap_or(i32::MAX),
            dither: u32::from(dither),
        };
        // SAFETY: `Params` is a plain repr(C) value of three four-byte fields
        // with no padding and no pointers, so its bytes are its representation.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(&params).cast::<u8>(),
                size_of::<Params>(),
            )
        };

        // One invocation per 2x2 block, rounded up so an odd edge is covered.
        let groups_x = source.width.div_ceil(2).div_ceil(GROUP);
        let groups_y = source.height.div_ceil(2).div_ceil(GROUP);

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
                &[acquire, prepare],
            );
            vk_device.cmd_bind_pipeline(commands, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            vk_device.cmd_bind_descriptor_sets(
                commands,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[set],
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
            vk_device.end_command_buffer(commands).map_err(driver)?;
        }

        // SAFETY: the create info outlives the call.
        let fence = unsafe { vk_device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(driver)?;
        let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
        // SAFETY: everything borrowed outlives the wait, which is what makes
        // freeing it afterwards safe.
        let waited = unsafe {
            vk_device
                .queue_submit(device.queue, &submits, fence)
                .map_err(driver)
                .and_then(|()| {
                    vk_device
                        .wait_for_fences(&[fence], true, u64::MAX)
                        .map_err(driver)
                })
        };
        // SAFETY: signalled or never submitted; nothing waits on it.
        unsafe {
            vk_device.destroy_fence(fence, None);
            vk_device.free_command_buffers(self.commands, &buffers);
        }
        waited
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
        let luma_bytes = u64::from(nv12.width) * u64::from(nv12.height);
        let chroma_bytes = luma_bytes / 2;

        let info = vk::BufferCreateInfo::default()
            .size(luma_bytes + chroma_bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { self.device.create_buffer(&info, None) }.map_err(driver)?;

        let result = self.read_nv12_into(nv12, buffer, luma_bytes, chroma_bytes);
        // SAFETY: created above; the copy inside waited before returning.
        unsafe { self.device.destroy_buffer(buffer, None) };
        result
    }

    fn read_nv12_into(
        &self,
        nv12: &Nv12,
        buffer: vk::Buffer,
        luma_bytes: u64,
        chroma_bytes: u64,
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        // SAFETY: the buffer was created on this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let index = self.host_visible_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let outcome = (|| -> Result<(Vec<u8>, Vec<u8>), Error> {
            // SAFETY: neither handle is bound yet.
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.map_err(driver)?;
            self.copy_planes(nv12, buffer, luma_bytes)?;

            let total = luma_bytes + chroma_bytes;
            // SAFETY: host visible, nothing else maps it, and the copy above
            // completed before this line.
            let mapped = unsafe {
                self.device
                    .map_memory(memory, 0, total, vk::MemoryMapFlags::empty())
            }
            .map_err(driver)?;
            let length = usize::try_from(total).unwrap_or(0);
            let split = usize::try_from(luma_bytes).unwrap_or(0);
            // SAFETY: the driver returned a mapping of at least `total` bytes,
            // valid until the unmap below.
            let all = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), length) };
            let luma = all.get(..split).unwrap_or_default().to_vec();
            let chroma = all.get(split..).unwrap_or_default().to_vec();
            // SAFETY: mapped just above and not referenced after this.
            unsafe { self.device.unmap_memory(memory) };
            Ok((luma, chroma))
        })();

        // SAFETY: nothing submitted still refers to it.
        unsafe { self.device.free_memory(memory, None) };
        outcome
    }

    /// Copy both planes into one buffer, luma first.
    fn copy_planes(&self, nv12: &Nv12, buffer: vk::Buffer, split: u64) -> Result<(), Error> {
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
            let settle = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(nv12.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::PLANE_0 | vk::ImageAspectFlags::PLANE_1,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });

            let regions = [
                vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::PLANE_0,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: nv12.width,
                        height: nv12.height,
                        depth: 1,
                    }),
                vk::BufferImageCopy::default()
                    .buffer_offset(split)
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::PLANE_1,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    // Half resolution in both directions, which the interface
                    // expects stated in the plane's own texels rather than the
                    // image's.
                    .image_extent(vk::Extent3D {
                        width: nv12.width / 2,
                        height: nv12.height / 2,
                        depth: 1,
                    }),
            ];

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
                    &[settle],
                );
                self.device.cmd_copy_image_to_buffer(
                    commands,
                    nv12.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buffer,
                    &regions,
                );
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
        let converter = Converter::new(&device).expect("a pipeline");
        converter
            .run(&device, &source, &target, false)
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

    /// The exported descriptor is real and the layout is self-consistent.
    ///
    /// **This does not prove the plane offset is the right one, and it was
    /// written believing it did.** Substituting the offset a naive
    /// implementation would compute -- width times height -- passes every
    /// assertion here, because at these dimensions that value equals the
    /// minimum a consistent layout could have. What the test does catch is a
    /// layout query that failed or returned nothing, the wrong aspect names, a
    /// descriptor that is not a real buffer, and any size that cannot hold what
    /// the layout claims. **The offset being the driver's own is settled by an
    /// encoder importing it and producing a picture**, and by nothing short of
    /// that.
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

        let (fd, layout) = device.export_nv12(&target).expect("export the frame");
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
        assert!(
            layout.planes[1].offset >= layout.planes[0].pitch * height,
            "chroma starts inside the luma plane"
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
