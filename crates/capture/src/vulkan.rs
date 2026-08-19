//! The device that imports a captured framebuffer and converts it.
//!
//! Import and conversion are one interface on one device, chosen because it is
//! the only one that takes a buffer's tiling modifier explicitly and runs the
//! same compute shader on every driver here. Encoding stays where it is; this
//! hands it a plain untiled result.
//!
//! **The device is picked to match the display, not by preference.** A frame
//! captured from one card cannot be imported by another without a copy through
//! system memory, which is the thing this whole path exists to avoid.

use std::ffi::CStr;
use std::path::Path;

use ash::vk;

/// What went wrong.
///
/// Driver results are carried as their raw code rather than as a formatted
/// message, so the error type stays `Copy` and allocation free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The loader is absent or refused to load.
    NoLoader,
    /// A call into the driver failed.
    Driver(i32),
    /// No device on this machine drives the display node that was asked for.
    /// Either the node is wrong or its driver does not report which one it is.
    NoDeviceForNode,
    /// The device that drives the display cannot import a captured buffer.
    /// Named individually because the answer differs per extension and a
    /// missing one is a deployment fact rather than a bug.
    Unsupported(&'static str),
    /// The device exposes no queue that can run the conversion.
    NoQueue,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLoader => f.write_str("no usable driver loader on this system"),
            Self::Driver(code) => write!(f, "driver call failed, result {code}"),
            Self::NoDeviceForNode => f.write_str("no device reports driving that display node"),
            Self::Unsupported(what) => write!(f, "the display's device does not support {what}"),
            Self::NoQueue => f.write_str("the display's device exposes no usable queue"),
        }
    }
}

impl std::error::Error for Error {}

fn driver(result: vk::Result) -> Error {
    Error::Driver(result.as_raw())
}

/// Everything the import needs from the device, and nothing it does not.
///
/// Each is checked by name at startup so a machine that cannot do this says
/// which part is missing, rather than failing at the first import with a
/// result code that names no cause.
const REQUIRED: [&CStr; 4] = [
    // Take a buffer in by file descriptor.
    ash::khr::external_memory_fd::NAME,
    // ...and specifically one shared the way a display buffer is shared.
    ash::ext::external_memory_dma_buf::NAME,
    // Describe that buffer's tiling rather than assuming it is untiled. This
    // is the one that makes the whole approach possible: both drivers here
    // hand out a tiled or compressed buffer and neither can be read without
    // being told how.
    ash::ext::image_drm_format_modifier::NAME,
    // Take ownership of an image that was last written by something outside
    // this interface entirely, which is what the display is.
    ash::ext::queue_family_foreign::NAME,
];

/// The device the display is on, ready to import from it.
pub struct Device {
    /// Dropped last. Every handle below is scoped to it.
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("queue_family", &self.queue_family)
            .finish_non_exhaustive()
    }
}

/// A device node's major and minor numbers.
///
/// Decomposed here rather than through the C helpers, which is four lines of
/// shifting against one more dependency.
fn node_numbers(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).ok()?;
    let minor = u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).ok()?;
    Some((major, minor))
}

impl Device {
    /// Open the device that drives a given display node.
    pub fn for_display(node: &Path) -> Result<Self, Error> {
        let (major, minor) = node_numbers(node).ok_or(Error::NoDeviceForNode)?;

        // SAFETY: loads the system driver loader. Nothing is passed in and the
        // handle is kept for the lifetime of everything derived from it.
        let entry = unsafe { ash::Entry::load() }.map_err(|_| Error::NoLoader)?;

        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let create = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: the create info borrows `application`, which outlives it, and
        // names no extensions or layers.
        let instance = unsafe { entry.create_instance(&create, None) }.map_err(driver)?;

        // Everything after the instance exists can fail, and the instance has
        // to be released exactly once on that path. Resolving it all into one
        // result keeps that to a single place.
        let opened = Self::find(&instance, major, minor).and_then(|physical| {
            Self::open(&instance, physical)
                .map(|(device, queue, family)| (physical, device, queue, family))
        });

        match opened {
            Ok((physical, device, queue, queue_family)) => Ok(Self {
                _entry: entry,
                instance,
                physical,
                device,
                queue,
                queue_family,
            }),
            Err(error) => {
                // SAFETY: nothing created from this instance outlives the
                // failed call, so it is the only thing left to release.
                unsafe { instance.destroy_instance(None) };
                Err(error)
            }
        }
    }

    /// The device that reports driving this display node.
    ///
    /// Matched on the node numbers the driver itself reports, which is exact.
    /// Matching on a name or an index instead breaks the moment a machine has
    /// two cards from the same vendor, or reorders them across a reboot.
    fn find(instance: &ash::Instance, major: u32, minor: u32) -> Result<vk::PhysicalDevice, Error> {
        // SAFETY: enumerating from a live instance.
        let candidates = unsafe { instance.enumerate_physical_devices() }.map_err(driver)?;
        for physical in candidates {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the chain is built from stack values that outlive the
            // call, and the device came from this instance.
            unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
            if drm.has_primary != 0
                && u32::try_from(drm.primary_major) == Ok(major)
                && u32::try_from(drm.primary_minor) == Ok(minor)
            {
                return Ok(physical);
            }
        }
        Err(Error::NoDeviceForNode)
    }

    /// Check what the chosen device can do, then open it.
    fn open(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<(ash::Device, vk::Queue, u32), Error> {
        // SAFETY: the device came from this instance.
        let available =
            unsafe { instance.enumerate_device_extension_properties(physical) }.map_err(driver)?;
        for wanted in REQUIRED {
            let present = available
                .iter()
                .any(|extension| extension.extension_name_as_c_str() == Ok(wanted));
            if !present {
                return Err(Error::Unsupported(
                    wanted.to_str().unwrap_or("a required interface"),
                ));
            }
        }

        // Compute alone. The conversion is a shader and a copy; nothing here
        // draws, so asking for graphics would reject devices that could serve
        // us perfectly well.
        // SAFETY: the device came from this instance.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = families
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .and_then(|at| u32::try_from(at).ok())
            .ok_or(Error::NoQueue)?;

        let priorities = [1.0_f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let names: Vec<*const core::ffi::c_char> =
            REQUIRED.iter().map(|name| name.as_ptr()).collect();
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&names);
        // SAFETY: every borrowed slice outlives the call, and the extension
        // names are static.
        let device = unsafe { instance.create_device(physical, &create, None) }.map_err(driver)?;
        // SAFETY: the family index came from this device's own enumeration and
        // one queue was requested from it.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        Ok((device, queue, queue_family))
    }

    /// What the driver calls itself, for a startup log line.
    pub fn name(&self) -> String {
        let mut properties = vk::PhysicalDeviceProperties2::default();
        // SAFETY: the device came from this instance and the chain is one
        // stack value that outlives the call.
        unsafe {
            self.instance
                .get_physical_device_properties2(self.physical, &mut properties)
        };
        properties
            .properties
            .device_name_as_c_str()
            .ok()
            .and_then(|name| name.to_str().ok())
            .unwrap_or("unknown")
            .to_string()
    }

    /// The queue conversion work is submitted to.
    pub fn queue(&self) -> vk::Queue {
        self.queue
    }

    /// The family that queue belongs to, which image ownership transfers name.
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: both handles are live until here, and nothing derived from
        // them outlives this type. The wait is what makes that true: work still
        // running would otherwise be holding memory that is about to go.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {

    /// The node numbers have to come out of the encoding the kernel uses, which
    /// is not a plain pair of bytes. The values here are the display and render
    /// nodes on a machine with two cards: 226:0, 226:1, 226:128, 226:129.
    #[test]
    fn node_numbers_decompose() {
        // Built the way the kernel builds them, so the test does not merely
        // restate the implementation.
        fn makedev(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8)
                | ((major & !0xfff) << 32)
                | (minor & 0xff)
                | ((minor & !0xff) << 12)
        }
        for (major, minor) in [(226, 0), (226, 1), (226, 128), (226, 129), (4095, 1048575)] {
            let rdev = makedev(major, minor);
            let got = (
                u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).unwrap(),
                u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).unwrap(),
            );
            assert_eq!(
                got,
                (u32::try_from(major).unwrap(), u32::try_from(minor).unwrap())
            );
        }
    }
}
