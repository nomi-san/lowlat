// This Source Code Form is subject to the terms of the MIT License.
// If a copy of the MIT License was not distributed with this file,
// You can obtain one at https://spdx.org/licenses/MIT.html.

#include "gfx/mod.h"
GFX_PROTOTYPES(_vk_)

#include <string.h>
#include <unistd.h>

#include "gfx/viewport.h"
#include "gfx/fmt.h"
#include "vk-common.h"
#include "fmt-vk.h"

static
#include "shaders/fs.h"

static
#include "shaders/vs.h"

#define VK_NUM_STAGING 3
#define VK_NUM_DESC    (VK_NUM_STAGING + 1)

// Imports kept for hardware frames, evicted round robin past that: enough
// for every surface a decoder lending its own may hand out at once, 22, and
// a few more across a change of layout. Fewer than the surfaces a stream
// cycles through is an import every picture, since round robin then evicts
// each import just before it comes round again.
#define VK_NUM_IMPORTS 24

// One imported allocation, keyed by the allocation's number: an opaque
// descriptor as a buffer the plane images are copied from, or a kernel
// buffer descriptor as an image a plane, in its layout modifier, copied from.
struct vk_import {
	uint32_t id;
	bool dmabuf;
	VkBuffer buffer;
	VkImage planes[2];
	VkDeviceMemory memory[2];

	// What the planes were imported as, so a number seen again in another
	// layout is imported again.
	VkFormat formats[2];
	uint32_t w[2];
	uint32_t h[2];
};

struct vk {
	MTY_ColorFormat format;

	VkShaderModule vert;
	VkShaderModule frag;
	VkSampler linear;
	VkSampler nearest;
	VkDescriptorSetLayout dlayout;
	VkDescriptorPool dpool;
	VkDescriptorSet dset;
	VkPipelineLayout layout;
	VkPipeline pipeline;
	VkRenderPass rp;

	struct gfx_uniforms cb;
	struct vk_buffer vb;
	struct vk_buffer ib;
	struct vk_buffer ub;

	struct vk_image staging[VK_NUM_STAGING];
	struct vk_import imports[VK_NUM_IMPORTS];
	uint32_t next_import;
};


// Main

static bool vk_one_shot_buffer(const VkPhysicalDeviceMemoryProperties *pdprops, VkDevice device,
	VkBufferUsageFlagBits usage, const void *data, size_t size, struct vk_buffer *buf)
{
	bool r = vk_allocate_buffer(pdprops, device, usage, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT, size, buf);
	if (!r)
		goto except;

	r = vk_buffer_upload(device, buf->mem, data, size);
	if (!r)
		goto except;

	except:

	if (!r)
		vk_destroy_buffer(device, buf);

	return r;
}

struct gfx *mty_vk_create(MTY_Device *device, uint8_t layer)
{
	struct vk *ctx = MTY_Alloc(1, sizeof(struct vk));

	MTY_VkDeviceObjects *dobjs = (MTY_VkDeviceObjects *) device;
	const VkPhysicalDeviceMemoryProperties *pdprops = dobjs->physicalDeviceMemoryProperties;
	VkDevice _device = dobjs->device;

	bool r = true;

	if (!vkproc_global_init()) {
		r = false;
		goto except;
	}

	// Vertex shader
	VkShaderModuleCreateInfo shci = {
		.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
		.codeSize = sizeof(VERT),
		.pCode = VERT,
	};

	VkResult e = vkCreateShaderModule(_device, &shci, NULL, &ctx->vert);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Fragment shader
	shci.codeSize = sizeof(FRAG);
	shci.pCode = FRAG;

	e = vkCreateShaderModule(_device, &shci, NULL, &ctx->frag);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Vertex buffer
	const float vertices[] = {
		-1.0f, -1.0f,	// Position 0 -- top left     (-1x, -1y)
		 0.0f,  0.0f,	// TexCoord 0 -- top left     ( 0x,  0y)

		 1.0f, -1.0f,	// Position 1 -- top right    ( 1x, -1y)
		 1.0f,  0.0f,	// TexCoord 1 -- top right    ( 1x,  0y)

		 1.0f,  1.0f,	// Position 2 -- bottom right ( 1x,  1y)
		 1.0f,  1.0f,	// TexCoord 2 -- bottom right ( 1x,  1y)

		-1.0f,  1.0f,	// Position 3 -- bottom left  (-1x,  1y)
		 0.0f,  1.0f	// TexCoord 3 -- bottom left  ( 0x,  1y)
	};

	r = vk_one_shot_buffer(pdprops, _device, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, vertices, sizeof(vertices), &ctx->vb);
	if (!r)
		goto except;

	// Index buffer
	const uint16_t indices[] = {
		0, 1, 2, 2, 3, 0
	};

	r = vk_one_shot_buffer(pdprops, _device, VK_BUFFER_USAGE_INDEX_BUFFER_BIT, indices, sizeof(indices), &ctx->ib);
	if (!r)
		goto except;

	// Uniform buffer
	r = vk_one_shot_buffer(pdprops, _device, VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT, &ctx->cb, sizeof(ctx->cb), &ctx->ub);
	if (!r)
		goto except;

	// Linear sampler
	VkSamplerCreateInfo sci = {
		.sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
		.magFilter = VK_FILTER_LINEAR,
		.minFilter = VK_FILTER_LINEAR,
		.addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
		.addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
		.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
	};

	e = vkCreateSampler(_device, &sci, NULL, &ctx->linear);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Nearest sampler
	sci.magFilter = sci.minFilter = VK_FILTER_NEAREST;

	e = vkCreateSampler(_device, &sci, NULL, &ctx->nearest);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Render pass
	VkAttachmentDescription ad = {
		.format = VKPROC_FORMAT,
		.samples = VK_SAMPLE_COUNT_1_BIT,
		.loadOp = layer == 0 ? VK_ATTACHMENT_LOAD_OP_CLEAR :
			VK_ATTACHMENT_LOAD_OP_LOAD,
		.storeOp = VK_ATTACHMENT_STORE_OP_STORE,
		.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
		.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
		.initialLayout = layer == 0 ? VK_IMAGE_LAYOUT_UNDEFINED :
			VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
		.finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
	};

	VkRenderPassCreateInfo rpci = {
		.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
		.attachmentCount = 1,
		.pAttachments = &ad,
		.subpassCount = 1,
		.pSubpasses = &(VkSubpassDescription) {
			.colorAttachmentCount = 1,
			.pColorAttachments = &(VkAttachmentReference) {
				.layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
			},
		},
	};

	e = vkCreateRenderPass(_device, &rpci, NULL, &ctx->rp);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Descriptor set layout
	VkDescriptorSetLayoutCreateInfo lci = {
		.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
		.bindingCount = VK_NUM_DESC,
		.pBindings = (VkDescriptorSetLayoutBinding []) {
			{0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_FRAGMENT_BIT},
			{1, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_FRAGMENT_BIT},
			{2, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_FRAGMENT_BIT},
			{3, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,         1, VK_SHADER_STAGE_FRAGMENT_BIT},
		},
	};

	e = vkCreateDescriptorSetLayout(_device, &lci, NULL, &ctx->dlayout);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Descriptor pool
	VkDescriptorPoolCreateInfo dpi = {
		.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
		.flags = VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT,
		.poolSizeCount = 2,
		.pPoolSizes = (VkDescriptorPoolSize []) {{
			.type = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
			.descriptorCount = VK_NUM_STAGING,
		}, {
			.type = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
			.descriptorCount = 1,
		}},
		.maxSets = 1,
	};

	e = vkCreateDescriptorPool(_device, &dpi, NULL, &ctx->dpool);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Descriptor set
	VkDescriptorSetAllocateInfo dsai = {
		.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
		.descriptorPool = ctx->dpool,
		.descriptorSetCount = 1,
		.pSetLayouts = &ctx->dlayout,
	};

	e = vkAllocateDescriptorSets(_device, &dsai, &ctx->dset);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Pipeline layout
	VkPipelineLayoutCreateInfo pci = {
		.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
		.setLayoutCount = 1,
		.pSetLayouts = &ctx->dlayout,
	};

	e = vkCreatePipelineLayout(_device, &pci, NULL, &ctx->layout);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	// Pipeline
	VkGraphicsPipelineCreateInfo pinfo = {
		.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
		.stageCount = 2,
		.pStages = (VkPipelineShaderStageCreateInfo []) {{
			.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
			.stage = VK_SHADER_STAGE_VERTEX_BIT,
			.module = ctx->vert,
			.pName = "main",
		}, {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
			.stage = VK_SHADER_STAGE_FRAGMENT_BIT,
			.module = ctx->frag,
			.pName = "main",
		}},
		.pVertexInputState = &(VkPipelineVertexInputStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
			.vertexBindingDescriptionCount = 1,
			.pVertexBindingDescriptions = &(VkVertexInputBindingDescription) {
				.stride = 4 * sizeof(float),
				.inputRate = VK_VERTEX_INPUT_RATE_VERTEX,
			},
			.vertexAttributeDescriptionCount = 2,
			.pVertexAttributeDescriptions = (VkVertexInputAttributeDescription []) {{
				.location = 0,
				.format = VK_FORMAT_R32G32_SFLOAT,
				.offset = 0,
			}, {
				.location = 1,
				.format = VK_FORMAT_R32G32_SFLOAT,
				.offset = 2 * sizeof(float),
			}},
		},
		.pInputAssemblyState = &(VkPipelineInputAssemblyStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
			.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
		},
		.pRasterizationState = &(VkPipelineRasterizationStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
			.lineWidth = 1.0f,
			.cullMode = VK_CULL_MODE_BACK_BIT,
			.frontFace = VK_FRONT_FACE_CLOCKWISE,
		},
		.pMultisampleState = &(VkPipelineMultisampleStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
			.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
		},
		.pColorBlendState = &(VkPipelineColorBlendStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
			.attachmentCount = 1,
			.pAttachments = &(VkPipelineColorBlendAttachmentState) {
				.blendEnable = layer == 0 ? VK_FALSE : VK_TRUE,
				.srcColorBlendFactor = VK_BLEND_FACTOR_SRC_ALPHA,
				.dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
				.colorBlendOp = VK_BLEND_OP_ADD,
				.srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE,
				.dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
				.alphaBlendOp = VK_BLEND_OP_ADD,
				.colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT |
					VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT,
			},
		},
		.pViewportState = &(VkPipelineViewportStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
			.viewportCount = 1,
			.scissorCount = 1,
		},
		.pDynamicState = &(VkPipelineDynamicStateCreateInfo) {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
			.dynamicStateCount = 2,
			.pDynamicStates = (VkDynamicState []) {
				VK_DYNAMIC_STATE_VIEWPORT,
				VK_DYNAMIC_STATE_SCISSOR,
			},
		},
		.layout = ctx->layout,
		.renderPass = ctx->rp,
	};

	e = vkCreateGraphicsPipelines(_device, VK_NULL_HANDLE, 1, &pinfo, NULL, &ctx->pipeline);
	if (e != VK_SUCCESS) {
		r = false;
		goto except;
	}

	except:

	if (!r)
		mty_vk_destroy((struct gfx **) &ctx, device);

	return (struct gfx *) ctx;
}

static bool vk_refresh_image(struct gfx *gfx, MTY_Device *device, MTY_Context *context, MTY_ColorFormat fmt,
	uint8_t plane, const uint8_t *image, uint32_t full_w, uint32_t w, uint32_t h, uint8_t bpp)
{
	struct vk *ctx = (struct vk *) gfx;

	bool r = true;

	struct vk_image *img = &ctx->staging[plane];
	VkFormat format = FMT_PLANES[fmt][plane];

	MTY_VkDeviceObjects *dobjs = (MTY_VkDeviceObjects *) device;
	const VkPhysicalDeviceMemoryProperties *pdprops = dobjs->physicalDeviceMemoryProperties;
	VkDevice _device = dobjs->device;

	VkCommandBuffer cmd = (VkCommandBuffer) context;

	// Resize
	if (!img->image || w != img->w || h != img->h || format != img->format) {
		vk_destroy_image(_device, img);

		r = vk_create_image(pdprops, _device, format, full_w, w, h, bpp, img);
		if (!r)
			goto except;
	}

	except:

	if (r) {
		// Upload to staging buffer
		r = vk_buffer_upload(_device, img->buf.mem, image, full_w * h * bpp);

		// GPU copy from staging
		if (r)
			vk_image_gpu_copy(img, cmd, full_w, w, h);

	} else {
		vk_destroy_image(_device, img);
	}

	return r;
}

// Hardware frames

static void vk_import_destroy(VkDevice device, struct vk_import *import)
{
	if (import->buffer)
		vkDestroyBuffer(device, import->buffer, NULL);

	for (uint8_t x = 0; x < 2; x++) {
		if (import->planes[x])
			vkDestroyImage(device, import->planes[x], NULL);

		if (import->memory[x])
			vkFreeMemory(device, import->memory[x], NULL);
	}

	memset(import, 0, sizeof(struct vk_import));
}

static int32_t vk_import_memory_type(const VkPhysicalDeviceMemoryProperties *pdprops, uint32_t bits)
{
	for (uint32_t x = 0; x < pdprops->memoryTypeCount; x++)
		if (bits & (1u << x))
			return (int32_t) x;

	return -1;
}

// The frame's allocation as a buffer: the descriptor's whole size, which the
// allocation was exported with.
static bool vk_import_opaque(const MTY_VkDeviceObjects *dobjs, const MTY_HardwareFrame *frame,
	struct vk_import *import)
{
	VkDevice device = dobjs->device;

	VkExternalMemoryBufferCreateInfo ext = {
		.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
		.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
	};

	VkBufferCreateInfo bi = {
		.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
		.pNext = &ext,
		.size = frame->size,
		.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
	};

	if (vkCreateBuffer(device, &bi, NULL, &import->buffer) != VK_SUCCESS)
		return false;

	VkMemoryRequirements req = {0};
	vkGetBufferMemoryRequirements(device, import->buffer, &req);

	int32_t type = vk_import_memory_type(dobjs->physicalDeviceMemoryProperties, req.memoryTypeBits);
	if (type < 0)
		return false;

	// The import takes ownership of the descriptor it is given, so it is
	// given a duplicate and the caller's stays the caller's.
	int fd = dup(frame->fd);
	if (fd < 0)
		return false;

	VkImportMemoryFdInfoKHR from = {
		.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
		.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
		.fd = fd,
	};

	VkMemoryAllocateInfo ai = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
		.pNext = &from,
		.allocationSize = frame->size,
		.memoryTypeIndex = (uint32_t) type,
	};

	VkResult e = vkAllocateMemory(device, &ai, NULL, &import->memory[0]);
	if (e != VK_SUCCESS) {
		close(fd); // A failed import takes nothing
		MTY_Log("Hardware frame import failed: 'vkAllocateMemory' returned %d", e);
		return false;
	}

	return vkBindBufferMemory(device, import->buffer, import->memory[0], 0) == VK_SUCCESS;
}

// One plane of the frame's buffer as an image in its layout modifier, at the
// plane's offset and pitch. The import is told the whole buffer: a plane past
// its start read through an import told only the plane's own size faults the
// device on one driver.
static bool vk_import_plane(const MTY_VkDeviceObjects *dobjs, const MTY_HardwareFrame *frame, uint8_t plane,
	VkFormat format, uint32_t w, uint32_t h, struct vk_import *import)
{
	VkDevice device = dobjs->device;

	VkSubresourceLayout layout = {
		.offset = frame->offset[plane],
		.rowPitch = frame->pitch[plane],
	};

	VkImageDrmFormatModifierExplicitCreateInfoEXT explicit = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
		.drmFormatModifier = frame->modifier,
		.drmFormatModifierPlaneCount = 1,
		.pPlaneLayouts = &layout,
	};

	VkExternalMemoryImageCreateInfo ext = {
		.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
		.pNext = &explicit,
		.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
	};

	VkImageCreateInfo ii = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
		.pNext = &ext,
		.imageType = VK_IMAGE_TYPE_2D,
		.format = format,
		.extent = {w, h, 1},
		.mipLevels = 1,
		.arrayLayers = 1,
		.samples = VK_SAMPLE_COUNT_1_BIT,
		.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
		.usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
		.sharingMode = VK_SHARING_MODE_EXCLUSIVE,
		.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
	};

	if (vkCreateImage(device, &ii, NULL, &import->planes[plane]) != VK_SUCCESS)
		return false;

	int fd = dup(frame->fd);
	if (fd < 0)
		return false;

	PFN_vkGetMemoryFdPropertiesKHR get_fd_props = (PFN_vkGetMemoryFdPropertiesKHR) dobjs->getMemoryFdProperties;

	VkMemoryFdPropertiesKHR fdp = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
	};

	VkMemoryDedicatedRequirements dreq = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS,
	};

	VkMemoryRequirements2 req = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2,
		.pNext = &dreq,
	};

	VkImageMemoryRequirementsInfo2 rinfo = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
		.image = import->planes[plane],
	};

	vkGetImageMemoryRequirements2(device, &rinfo, &req);

	int32_t type = -1;
	if (get_fd_props(device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fdp) == VK_SUCCESS)
		type = vk_import_memory_type(dobjs->physicalDeviceMemoryProperties,
			req.memoryRequirements.memoryTypeBits & fdp.memoryTypeBits);

	if (type < 0) {
		close(fd);
		return false;
	}

	VkMemoryDedicatedAllocateInfo ded = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
		.image = import->planes[plane],
	};

	VkImportMemoryFdInfoKHR from = {
		.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
		.pNext = &ded,
		.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
		.fd = fd,
	};

	VkMemoryAllocateInfo ai = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
		.pNext = &from,
		.allocationSize = frame->size > req.memoryRequirements.size ? frame->size : req.memoryRequirements.size,
		.memoryTypeIndex = (uint32_t) type,
	};

	VkResult e = vkAllocateMemory(device, &ai, NULL, &import->memory[plane]);
	if (e != VK_SUCCESS) {
		close(fd); // A failed import takes nothing
		MTY_Log("Hardware frame import failed: 'vkAllocateMemory' returned %d", e);
		return false;
	}

	return vkBindImageMemory(device, import->planes[plane], import->memory[plane], 0) == VK_SUCCESS;
}

// The frame's import: the one made before under the same number in the same
// layout, else a fresh one in the oldest slot.
static struct vk_import *vk_import_frame(struct vk *ctx, const MTY_VkDeviceObjects *dobjs,
	const MTY_HardwareFrame *frame, const VkFormat formats[2], const uint32_t w[2], const uint32_t h[2])
{
	for (uint32_t x = 0; x < VK_NUM_IMPORTS; x++) {
		struct vk_import *import = &ctx->imports[x];

		if ((import->buffer || import->planes[0]) && import->id == frame->id && import->dmabuf == frame->dmabuf &&
			!memcmp(import->formats, formats, sizeof(import->formats)) &&
			!memcmp(import->w, w, sizeof(import->w)) && !memcmp(import->h, h, sizeof(import->h)))
			return import;
	}

	// The last draw's work was waited for when it was presented, so nothing
	// reads the import replaced here.
	struct vk_import *import = &ctx->imports[ctx->next_import];
	ctx->next_import = (ctx->next_import + 1) % VK_NUM_IMPORTS;
	vk_import_destroy(dobjs->device, import);

	bool r = frame->dmabuf ?
		vk_import_plane(dobjs, frame, 0, formats[0], w[0], h[0], import) &&
		vk_import_plane(dobjs, frame, 1, formats[1], w[1], h[1], import) :
		vk_import_opaque(dobjs, frame, import);

	if (!r) {
		vk_import_destroy(dobjs->device, import);
		return NULL;
	}

	import->id = frame->id;
	import->dmabuf = frame->dmabuf;
	memcpy(import->formats, formats, sizeof(import->formats));
	memcpy(import->w, w, sizeof(import->w));
	memcpy(import->h, h, sizeof(import->h));

	return import;
}

static void vk_image_barrier(VkCommandBuffer cmd, VkImage image, VkImageLayout from, VkImageLayout to,
	VkAccessFlags src_access, VkAccessFlags dst_access, uint32_t src_family, uint32_t dst_family,
	VkPipelineStageFlags src_stage, VkPipelineStageFlags dst_stage)
{
	VkImageMemoryBarrier b = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
		.srcAccessMask = src_access,
		.dstAccessMask = dst_access,
		.oldLayout = from,
		.newLayout = to,
		.srcQueueFamilyIndex = src_family,
		.dstQueueFamilyIndex = dst_family,
		.image = image,
		.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
		.subresourceRange.levelCount = 1,
		.subresourceRange.layerCount = 1,
	};

	vkCmdPipelineBarrier(cmd, src_stage, dst_stage, 0, 0, NULL, 0, NULL, 1, &b);
}

// The plane images filled from the frame's import, a device-side copy a
// plane: from the buffer at each plane's offset and pitch, or from each
// plane's image, acquired from the device that wrote it with its content
// kept and given back after the copy.
static bool vk_refresh_hardware(struct vk *ctx, const MTY_VkDeviceObjects *dobjs, VkCommandBuffer cmd,
	const MTY_HardwareFrame *frame, const MTY_RenderDesc *desc)
{
	uint8_t bpp = FMT_INFO[desc->format].bpp;
	uint8_t planes = FMT_INFO[desc->format].planes;

	if (frame->dmabuf ? (planes != 2 || !dobjs->importsDmaBuf) : !dobjs->importsOpaque)
		return false;

	uint32_t _hdiv = desc->chroma == MTY_CHROMA_420 ? 2 : 1;
	uint32_t _wdiv = desc->chroma != MTY_CHROMA_444 ? 2 : 1;

	VkFormat formats[2] = {VK_FORMAT_UNDEFINED, VK_FORMAT_UNDEFINED};
	uint32_t w[2] = {0}, h[2] = {0};

	for (uint8_t x = 0; x < 2 && x < planes; x++) {
		formats[x] = FMT_PLANES[desc->format][x];
		w[x] = desc->cropWidth / (x > 0 ? _wdiv : 1);
		h[x] = desc->cropHeight / (x > 0 ? _hdiv : 1);
	}

	// The opaque buffer is laid out whatever the plane count; its key
	// carries the first two planes alone.
	struct vk_import *import = vk_import_frame(ctx, dobjs, frame, formats, w, h);
	if (!import)
		return false;

	VkDevice device = dobjs->device;
	uint32_t family = dobjs->queueFamily;

	if (!frame->dmabuf) {
		// The allocation was written by the device that exported it.
		VkBufferMemoryBarrier b = {
			.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
			.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
			.srcQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL,
			.dstQueueFamilyIndex = family,
			.buffer = import->buffer,
			.size = VK_WHOLE_SIZE,
		};

		vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL,
			1, &b, 0, NULL);
	}

	for (uint8_t x = 0; x < planes; x++) {
		uint32_t hdiv = x > 0 ? _hdiv : 1;
		uint32_t wdiv = x > 0 ? _wdiv : 1;
		uint8_t pack = x == 1 && planes == 2 ? 2 : 1;
		uint32_t pw = desc->cropWidth / wdiv;
		uint32_t ph = desc->cropHeight / hdiv;
		VkFormat format = FMT_PLANES[desc->format][x];

		struct vk_image *img = &ctx->staging[x];

		if (!img->image || pw != img->w || ph != img->h || format != img->format) {
			vk_destroy_image(device, img);

			if (!vk_create_image(dobjs->physicalDeviceMemoryProperties, device, format, pw, pw, ph, pack * bpp, img))
				return false;
		}

		vk_image_barrier(cmd, img->image, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
			0, VK_ACCESS_TRANSFER_WRITE_BIT, VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED,
			VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);

		if (frame->dmabuf) {
			VkImage src = import->planes[x];

			vk_image_barrier(cmd, src, VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
				0, VK_ACCESS_TRANSFER_READ_BIT, VK_QUEUE_FAMILY_FOREIGN_EXT, family,
				VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);

			VkImageCopy region = {
				.srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
				.dstSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
				.extent = {pw, ph, 1},
			};

			vkCmdCopyImage(cmd, src, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, img->image,
				VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);

			vk_image_barrier(cmd, src, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
				VK_ACCESS_TRANSFER_READ_BIT, 0, family, VK_QUEUE_FAMILY_FOREIGN_EXT,
				VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT);

		} else {
			VkBufferImageCopy region = {
				.bufferOffset = frame->offset[x],
				.bufferRowLength = frame->pitch[x] / (pack * bpp),
				.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
				.imageExtent = {pw, ph, 1},
			};

			vkCmdCopyBufferToImage(cmd, import->buffer, img->image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1,
				&region);
		}

		vk_image_barrier(cmd, img->image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
			VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
			VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED, VK_PIPELINE_STAGE_TRANSFER_BIT,
			VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT);
	}

	return true;
}

// Whether a buffer's layout modifier is one the device imports for a plane
// format, as an image a copy reads.
static bool vk_modifier_imports(const MTY_VkDeviceObjects *dobjs, VkFormat format, uint64_t modifier)
{
	VkPhysicalDevice pdevice = dobjs->physicalDevice;

	VkDrmFormatModifierPropertiesListEXT list = {
		.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
	};

	VkFormatProperties2 fp = {
		.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
		.pNext = &list,
	};

	vkGetPhysicalDeviceFormatProperties2(pdevice, format, &fp);
	if (list.drmFormatModifierCount == 0)
		return false;

	VkDrmFormatModifierPropertiesEXT *mods = MTY_Alloc(list.drmFormatModifierCount,
		sizeof(VkDrmFormatModifierPropertiesEXT));
	list.pDrmFormatModifierProperties = mods;
	vkGetPhysicalDeviceFormatProperties2(pdevice, format, &fp);

	bool offered = false;
	for (uint32_t x = 0; x < list.drmFormatModifierCount && !offered; x++)
		offered = mods[x].drmFormatModifier == modifier && mods[x].drmFormatModifierPlaneCount == 1 &&
			(mods[x].drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_TRANSFER_SRC_BIT);

	MTY_Free(mods);

	if (!offered)
		return false;

	VkPhysicalDeviceImageDrmFormatModifierInfoEXT modinfo = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
		.drmFormatModifier = modifier,
		.sharingMode = VK_SHARING_MODE_EXCLUSIVE,
	};

	VkPhysicalDeviceExternalImageFormatInfo extinfo = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
		.pNext = &modinfo,
		.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
	};

	VkPhysicalDeviceImageFormatInfo2 ifi = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
		.pNext = &extinfo,
		.format = format,
		.type = VK_IMAGE_TYPE_2D,
		.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
		.usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
	};

	VkExternalImageFormatProperties extprops = {
		.sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES,
	};

	VkImageFormatProperties2 ifp = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
		.pNext = &extprops,
	};

	if (vkGetPhysicalDeviceImageFormatProperties2(pdevice, &ifi, &ifp) != VK_SUCCESS)
		return false;

	return extprops.externalMemoryProperties.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT;
}

bool mty_vk_valid_hardware_frame(MTY_Device *device, MTY_Context *context, const void *shared_resource)
{
	const MTY_VkDeviceObjects *dobjs = (const MTY_VkDeviceObjects *) device;
	if (!dobjs)
		return false;

	const MTY_HardwareFrame *frame = shared_resource;

	if (!frame)
		return dobjs->importsOpaque || dobjs->importsDmaBuf;

	if (!frame->dmabuf)
		return dobjs->importsOpaque;

	if (!dobjs->importsDmaBuf)
		return false;

	if (frame->format != MTY_COLOR_FORMAT_2PLANES && frame->format != MTY_COLOR_FORMAT_2PLANES_16)
		return false;

	// Asked before anything is drawn, so before the renderer that loads the
	// entry points exists.
	if (!vkproc_global_init())
		return false;

	bool r = true;
	for (uint8_t x = 0; x < 2 && r; x++)
		r = vk_modifier_imports(dobjs, FMT_PLANES[frame->format][x], frame->modifier);

	vkproc_global_destroy();

	return r;
}

bool mty_vk_render(struct gfx *gfx, MTY_Device *device, MTY_Context *context,
	const void *image, const MTY_RenderDesc *desc, MTY_Surface *dest)
{
	struct vk *ctx = (struct vk *) gfx;

	MTY_VkDeviceObjects *dobjs = (MTY_VkDeviceObjects *) device;
	VkDevice _device = dobjs->device;

	VkCommandBuffer cmd = (VkCommandBuffer) context;
	VkFramebuffer fb = (VkFramebuffer) dest;

	// Don't do anything until we have real data
	if (desc->format != MTY_COLOR_FORMAT_UNKNOWN)
		ctx->format = desc->format;

	// If format == MTY_COLOR_FORMAT_UNKNOWN, texture refreshing/loading is skipped and the previous frame is rendered
	if (ctx->format == MTY_COLOR_FORMAT_UNKNOWN)
		return true;

	// Refresh textures and load texture data: a hardware frame's from its
	// import, by a copy on the device, and only when it is loaded, which a
	// repeat that names no format is not.
	if (desc->hardware) {
		if (desc->format != MTY_COLOR_FORMAT_UNKNOWN &&
			!vk_refresh_hardware(ctx, dobjs, cmd, (const MTY_HardwareFrame *) image, desc))
			return false;

	} else if (!fmt_reload_textures(gfx, device, context, image, desc, vk_refresh_image)) {
		return false;
	}

	// Begin render pass
	VkRenderPassBeginInfo rpi = {
		.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
		.renderPass = ctx->rp,
		.framebuffer = fb,
		.renderArea.extent = {
			.width = desc->viewWidth,
			.height = desc->viewHeight,
		},
		.clearValueCount = 1,
		.pClearValues = &(VkClearValue) {
			.color.float32 = {0, 0, 0, 1}
		},
	};

	vkCmdBeginRenderPass(cmd, &rpi, VK_SUBPASS_CONTENTS_INLINE);
	vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, ctx->pipeline);

	// Viewport
	VkViewport vp = {.maxDepth = 1.0f};
	mty_viewport(desc, &vp.x, &vp.y, &vp.width, &vp.height);

	vkCmdSetViewport(cmd, 0, 1, &vp);

	// Scissor
	VkRect2D scissor = {
		.extent.width = desc->viewWidth,
		.extent.height = desc->viewHeight,
	};

	vkCmdSetScissor(cmd, 0, 1, &scissor);

	// Update uniform buffer and descriptor
	struct gfx_uniforms cb =  {
		.width = (float) desc->cropWidth,
		.height = (float) desc->cropHeight,
		.vp_height = vp.height,
		.effects[0] = desc->effects[0],
		.effects[1] = desc->effects[1],
		.levels[0] = desc->levels[0],
		.levels[1] = desc->levels[1],
		.planes = FMT_INFO[ctx->format].planes,
		.rotation = desc->rotation,
		.conversion = FMT_CONVERSION(ctx->format, desc->fullRangeYUV, desc->multiplyYUV),
	};

	if (memcmp(&ctx->cb, &cb, sizeof(struct gfx_uniforms))) {
		if (!vk_buffer_upload(_device, ctx->ub.mem, &cb, sizeof(struct gfx_uniforms)))
			return false;

		VkWriteDescriptorSet dw = {
			.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
			.dstSet = ctx->dset,
			.dstBinding = VK_NUM_STAGING,
			.descriptorType = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
			.descriptorCount = 1,
			.pBufferInfo = &(VkDescriptorBufferInfo) {
				.buffer = ctx->ub.buf,
				.range = VK_WHOLE_SIZE,
			},
		};

		vkUpdateDescriptorSets(_device, 1, &dw, 0, NULL);

		ctx->cb = cb;
	}

	// Update image descriptors
	VkDescriptorImageInfo ii[VK_NUM_STAGING] = {0};
	VkWriteDescriptorSet dw[VK_NUM_DESC] = {0};

	for (uint8_t x = 0; x < VK_NUM_STAGING; x++) {
		// Vulkan must have a texture bound to each slot -- there should always be at least one
		ii[x].imageLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
		ii[x].imageView = ctx->staging[x].view ? ctx->staging[x].view : ctx->staging[0].view;
		ii[x].sampler = desc->filter == MTY_FILTER_NEAREST ? ctx->nearest : ctx->linear;

		dw[x].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
		dw[x].dstSet = ctx->dset;
		dw[x].dstBinding = x;
		dw[x].descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
		dw[x].descriptorCount = 1;
		dw[x].pImageInfo = &ii[x];
	}

	vkUpdateDescriptorSets(_device, VK_NUM_STAGING, dw, 0, NULL);

	// Bind vertex/index bufers, uniforms and draw
	VkDeviceSize offset = 0;
	vkCmdBindVertexBuffers(cmd, 0, 1, &ctx->vb.buf, &offset);
	vkCmdBindIndexBuffer(cmd, ctx->ib.buf, 0, VK_INDEX_TYPE_UINT16);

	vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS,
		ctx->layout, 0, 1, &ctx->dset, 0, NULL);

	vkCmdDrawIndexed(cmd, 6, 1, 0, 0, 0);

	// End render pass
	vkCmdEndRenderPass(cmd);

	return true;
}

void mty_vk_clear(struct gfx *gfx, MTY_Device *device, MTY_Context *context,
	uint32_t width, uint32_t height, float r, float g, float b, float a, MTY_Surface *dest)
{
	struct vk *ctx = (struct vk *) gfx;

	VkCommandBuffer cmd = (VkCommandBuffer) context;
	VkFramebuffer fb = (VkFramebuffer) dest;

	VkRenderPassBeginInfo rpi = {
		.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
		.renderPass = ctx->rp,
		.framebuffer = fb,
		.renderArea.extent = {
			.width = width,
			.height = height,
		},
		.clearValueCount = 1,
		.pClearValues = &(VkClearValue) {
			.color.float32 = {r, g, b, a}
		},
	};

	vkCmdBeginRenderPass(cmd, &rpi, VK_SUBPASS_CONTENTS_INLINE);
	vkCmdEndRenderPass(cmd);
}

void mty_vk_destroy(struct gfx **gfx, MTY_Device *device)
{
	if (!gfx || !*gfx)
		return;

	struct vk *ctx = (struct vk *) *gfx;

	MTY_VkDeviceObjects *dobjs = (MTY_VkDeviceObjects *) device;

	if (dobjs) {
		VkDevice _device = dobjs->device;

		if (_device) {
			for (uint8_t x = 0; x < VK_NUM_STAGING; x++)
				vk_destroy_image(_device, &ctx->staging[x]);

			for (uint32_t x = 0; x < VK_NUM_IMPORTS; x++)
				vk_import_destroy(_device, &ctx->imports[x]);

			if (ctx->pipeline)
				vkDestroyPipeline(_device, ctx->pipeline, NULL);

			if (ctx->layout)
				vkDestroyPipelineLayout(_device, ctx->layout, NULL);

			if (ctx->dpool) {
				if (ctx->dset)
					vkFreeDescriptorSets(_device, ctx->dpool, 1, &ctx->dset);

				vkDestroyDescriptorPool(_device, ctx->dpool, NULL);
			}

			if (ctx->dlayout)
				vkDestroyDescriptorSetLayout(_device, ctx->dlayout, NULL);

			if (ctx->rp)
				vkDestroyRenderPass(_device, ctx->rp, NULL);

			if (ctx->nearest)
				vkDestroySampler(_device, ctx->nearest, NULL);

			if (ctx->linear)
				vkDestroySampler(_device, ctx->linear, NULL);

			vk_destroy_buffer(_device, &ctx->ub);
			vk_destroy_buffer(_device, &ctx->ib);
			vk_destroy_buffer(_device, &ctx->vb);

			if (ctx->frag)
				vkDestroyShaderModule(_device, ctx->frag, NULL);

			if (ctx->vert)
				vkDestroyShaderModule(_device, ctx->vert, NULL);
		}
	}

	vkproc_global_destroy();

	MTY_Free(ctx);
	*gfx = NULL;
}
