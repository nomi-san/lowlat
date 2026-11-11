// This Source Code Form is subject to the terms of the MIT License.
// If a copy of the MIT License was not distributed with this file,
// You can obtain one at https://spdx.org/licenses/MIT.html.

#pragma once

#include <stdbool.h>
#include <stdint.h>

typedef struct MTY_Device MTY_Device;
typedef struct MTY_Context MTY_Context;
typedef struct MTY_Surface MTY_Surface;

typedef struct {
	void *device;
	const void *physicalDeviceMemoryProperties;

	// What a renderer imports another device's pictures with: the physical
	// device and the queue family its context draws on, whether it imports
	// a compute runtime's opaque descriptors and kernel buffer descriptors
	// with a layout modifier, and the entry point that says which memory a
	// descriptor can be imported as.
	void *physicalDevice;
	uint32_t queueFamily;
	bool importsOpaque;
	bool importsDmaBuf;
	void *getMemoryFdProperties;
} MTY_VkDeviceObjects;
