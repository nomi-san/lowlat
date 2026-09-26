//! Memory that crosses to another device or process as a descriptor: an
//! allocation exported for the handle path, and the capture's buffers
//! imported for the encoder. The descriptor is the platform's own kind, so
//! this half of the runtime is written per platform.

use core::ffi::{c_int, c_void};
use std::os::fd::OwnedFd;

use super::{
    CUdeviceptr, Cuda, DestroyExternalMemory, Device, ExternalMemoryGetMappedBuffer,
    MEM_ACCESS_FLAGS_PROT_READWRITE, MEM_ALLOC_GRANULARITY_MINIMUM, MEM_ALLOCATION_TYPE_PINNED,
    MEM_LOCATION_TYPE_DEVICE, MemAccessDesc, MemAddressFree, MemAllocationProp,
    MemGenericAllocationHandle, MemLocation, MemRelease, MemUnmap, Result, check,
};

const MEM_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR: core::ffi::c_uint = 1;

/// A device allocation another interface can map.
///
/// Made through the virtual-memory interface rather than as a pitched
/// allocation, because only an allocation made that way can leave the
/// runtime as a descriptor. The descriptor is the platform's opaque kind,
/// which the display interfaces import as foreign memory; it is **this
/// allocation's for its life**, closed with it, so a consumer that keeps
/// one past the allocation duplicates it first. An import that takes
/// ownership of what it is given is given a duplicate.
pub struct Exportable {
    ptr: CUdeviceptr,
    /// The mapped length: the bytes asked for, rounded up to the granule.
    size: usize,
    handle: MemGenericAllocationHandle,
    fd: OwnedFd,
    unmap: MemUnmap,
    address_free: MemAddressFree,
    release: MemRelease,
}

impl core::fmt::Debug for Exportable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exportable")
            .field("ptr", &self.ptr)
            .field("size", &self.size)
            .field("fd", &self.fd)
            .finish_non_exhaustive()
    }
}

// SAFETY: a device allocation belongs to its context, not to a thread.
unsafe impl Send for Exportable {}
unsafe impl Sync for Exportable {}

impl Exportable {
    pub fn ptr(&self) -> CUdeviceptr {
        self.ptr
    }

    /// The whole mapped length, which is what an importer is told.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The descriptor, borrowed: it is closed with the allocation.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }
}

impl Drop for Exportable {
    fn drop(&mut self) {
        // SAFETY: mapped once at `ptr` for `size`, reserved once, created
        // once; the type is neither `Copy` nor `Clone`. The descriptor is
        // closed after, by its own drop, so the allocation's last
        // reference is not the one an importer may still hold.
        unsafe {
            let _ = (self.unmap)(self.ptr, self.size);
            let _ = (self.address_free)(self.ptr, self.size);
            let _ = (self.release)(self.handle);
        }
    }
}

impl Cuda {
    /// An allocation of at least `bytes` on `device`, exported.
    ///
    /// The size is rounded up to the interface's granule, which is what a
    /// mapping must be a multiple of; the caller lays its rows out inside
    /// `bytes` and the rest is slack.
    pub fn alloc_exportable(&self, device: &Device, bytes: usize) -> Result<Exportable> {
        let location = MemLocation {
            kind: MEM_LOCATION_TYPE_DEVICE,
            id: device.handle,
        };
        let prop = MemAllocationProp {
            kind: MEM_ALLOCATION_TYPE_PINNED,
            requested_handle_types: MEM_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR,
            location,
            win32_handle_meta_data: core::ptr::null_mut(),
            alloc_flags: [0; 8],
        };
        let mut granule: usize = 0;
        // SAFETY: both pointers are to live locals for the call.
        check(unsafe {
            (self.mem_get_allocation_granularity)(
                &raw mut granule,
                &raw const prop,
                MEM_ALLOC_GRANULARITY_MINIMUM,
            )
        })?;
        let granule = granule.max(1);
        let size = bytes.max(1).div_ceil(granule) * granule;

        let mut handle: MemGenericAllocationHandle = 0;
        // SAFETY: as above; the flags are documented as reserved and zero.
        check(unsafe { (self.mem_create)(&raw mut handle, size, &raw const prop, 0) })?;

        let mapped = self.map_exportable(handle, size, location);
        match mapped {
            Ok((ptr, fd)) => Ok(Exportable {
                ptr,
                size,
                handle,
                fd,
                unmap: self.mem_unmap,
                address_free: self.mem_address_free,
                release: self.mem_release,
            }),
            Err(e) => {
                // SAFETY: created above and not mapped, or unmapped by the
                // failed step.
                unsafe {
                    let _ = (self.mem_release)(handle);
                }
                Err(e)
            }
        }
    }

    /// Reserve, map, open for access and export; on a failure, undo what
    /// was done. The handle itself is the caller's to release.
    fn map_exportable(
        &self,
        handle: MemGenericAllocationHandle,
        size: usize,
        location: MemLocation,
    ) -> Result<(CUdeviceptr, OwnedFd)> {
        let mut ptr: CUdeviceptr = 0;
        // SAFETY: the out pointer is to a live local; no alignment or
        // address is asked for, and the flags are reserved.
        check(unsafe { (self.mem_address_reserve)(&raw mut ptr, size, 0, 0, 0) })?;

        // SAFETY: the range was reserved above at this size.
        let mapped = unsafe { (self.mem_map)(ptr, size, 0, handle, 0) };
        if let Err(e) = check(mapped) {
            // SAFETY: reserved above, not mapped.
            unsafe {
                let _ = (self.mem_address_free)(ptr, size);
            }
            return Err(e);
        }

        let access = MemAccessDesc {
            location,
            flags: MEM_ACCESS_FLAGS_PROT_READWRITE,
        };
        let mut fd: c_int = -1;
        // SAFETY: the descriptor is a live local; the range is mapped. The
        // out parameter is what the interface documents for this kind: a
        // pointer to an `int`.
        let opened = unsafe { (self.mem_set_access)(ptr, size, &raw const access, 1) };
        let exported = check(opened).and_then(|()| {
            check(unsafe {
                (self.mem_export_to_shareable_handle)(
                    (&raw mut fd).cast::<c_void>(),
                    handle,
                    MEM_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR,
                    0,
                )
            })
        });
        match exported {
            Ok(()) => {
                // SAFETY: a descriptor the runtime just gave us, owned by
                // nothing else.
                let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
                Ok((ptr, fd))
            }
            Err(e) => {
                // SAFETY: mapped and reserved above.
                unsafe {
                    let _ = (self.mem_unmap)(ptr, size);
                    let _ = (self.mem_address_free)(ptr, size);
                }
                Err(e)
            }
        }
    }
}

/// Memory that belongs to another interface, borrowed by this one.
///
/// **The descriptor is consumed.** This runtime takes ownership of a handle it
/// imports and closes it when the import is released, so a caller that also
/// closed it would close a descriptor the driver still holds.
pub struct External {
    handle: crate::ffi::cuda::CUexternalMemory,
    // Debug is by hand: the two function pointers have no useful rendering and
    // the handle is an address that means nothing outside the runtime.
    destroy: DestroyExternalMemory,
    map: ExternalMemoryGetMappedBuffer,
}

impl core::fmt::Debug for External {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("External").finish_non_exhaustive()
    }
}

// SAFETY: an import belongs to its context, not to a thread, as an allocation
// does.
unsafe impl Send for External {}

impl External {
    /// Address one region of the imported memory.
    ///
    /// The offset and the row length are the exporter's, not ours to compute.
    /// A frame laid out for this encoder puts its colour plane exactly one luma
    /// plane in, and the pitch is the same for both, which is the single figure
    /// registration is given.
    pub fn plane(&self, offset: u64, size: u64, pitch: usize) -> Result<Plane> {
        let mut descriptor =
            unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_EXTERNAL_MEMORY_BUFFER_DESC>() };
        descriptor.offset = offset;
        descriptor.size = size;

        let mut ptr: CUdeviceptr = 0;
        // SAFETY: the descriptor is live for the call and the import is this
        // runtime's.
        check(unsafe { (self.map)(&raw mut ptr, self.handle, &raw const descriptor) })?;
        Ok(Plane { ptr, pitch })
    }
}

impl Drop for External {
    fn drop(&mut self) {
        // SAFETY: imported by this runtime and not released twice. Any address
        // taken from it is invalid afterwards, which is why a plane borrows the
        // import rather than outliving it.
        unsafe {
            let _ = (self.destroy)(self.handle);
        }
    }
}

/// One plane inside imported memory, addressed the way registration wants it.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    ptr: CUdeviceptr,
    pitch: usize,
}

impl Plane {
    pub fn ptr(&self) -> CUdeviceptr {
        self.ptr
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }
}

impl Cuda {
    /// Take memory another interface allocated.
    ///
    /// **The handle is the platform's opaque kind, not the display stack's.**
    /// This runtime's handle enumeration has no name for a display-interface
    /// descriptor at all, so a frame destined here has to be exported the other
    /// way; the same allocation can produce both.
    ///
    /// # Safety
    ///
    /// A context must be current on this thread, and `fd` must be a handle this
    /// runtime can import: one exported for the platform's opaque kind, whose
    /// `size` is the whole allocation behind it.
    pub unsafe fn import(&self, fd: std::os::fd::OwnedFd, size: u64) -> Result<External> {
        use std::os::fd::IntoRawFd;

        let mut descriptor =
            unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_EXTERNAL_MEMORY_HANDLE_DESC>() };
        descriptor.type_ = crate::ffi::cuda::CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD;
        descriptor.handle.fd = fd.into_raw_fd();
        descriptor.size = size;

        let mut handle: crate::ffi::cuda::CUexternalMemory = core::ptr::null_mut();
        // SAFETY: the descriptor is live for the call. On success the runtime
        // owns the descriptor; on failure it does not, and it leaks rather
        // than being closed twice, which is the safer of the two.
        check(unsafe { (self.import_external_memory)(&raw mut handle, &raw const descriptor) })?;
        Ok(External {
            handle,
            destroy: self.destroy_external_memory,
            map: self.external_memory_get_mapped_buffer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Error, PciAddress};
    use super::*;

    /// The address of whichever card is driving a connected output, read from
    /// the kernel rather than assumed. Returns `None` on a machine with no
    /// display, which is where most of continuous integration runs.
    fn display_address() -> Option<PciAddress> {
        let entries = std::fs::read_dir("/sys/class/drm").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
            if status.trim() != "connected" {
                continue;
            }
            // card1-DP-4 -> card1 -> its device link, whose name is the address.
            let name = entry.file_name();
            let card = name.to_str()?.split('-').next()?.to_string();
            let link = std::fs::canonicalize(format!("/sys/class/drm/{card}/device")).ok()?;
            return PciAddress::parse(link.file_name()?.to_str()?);
        }
        None
    }

    /// Needs the vendor driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_selected_device_is_the_one_driving_the_display() {
        let cuda = Cuda::load().expect("compute runtime did not load");
        let count = cuda.device_count().expect("device count");
        assert!(count > 0, "runtime loaded but reports no devices");

        for ordinal in 0..count {
            let device = cuda.device(ordinal).expect("device");
            let mut name = [0u8; 96];
            let len = cuda.device_name(&device, &mut name).expect("name");
            println!(
                "device {ordinal}: {} at {}",
                String::from_utf8_lossy(&name[..len]),
                device.address()
            );
        }

        let Some(display) = display_address() else {
            println!("no connected output; selection by address not exercised");
            return;
        };
        println!("display is at {display}");

        let device = cuda
            .device_at(display)
            .expect("no compute device at the display's address");
        assert_eq!(device.address(), display);

        let context = cuda.retain_primary(&device).expect("primary context");
        assert!(!context.raw().is_null());

        // The refusal is the half worth proving: an address that exists on the
        // machine but belongs to another device must not silently succeed.
        let absent = PciAddress::parse("ffff:ff:ff.f").expect("parse");
        assert_eq!(
            cuda.device_at(absent).unwrap_err(),
            Error::NoSuchDevice(absent),
            "selection fell back to another device"
        );
    }

    /// An exported allocation's descriptor is one this runtime itself
    /// imports as foreign memory, and what was written through the
    /// allocation reads back through the import: the descriptor is live
    /// and names the same bytes. Off by default, as above.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn an_exported_allocation_reads_back_through_its_descriptor() {
        let cuda = Cuda::load().expect("compute runtime did not load");
        let device = cuda.any_device().expect("a device");
        let context = cuda.retain_primary(&device).expect("primary context");
        context.make_current().expect("current");

        const PITCH: usize = 4096;
        const ROWS: usize = 64;
        let exportable = cuda
            .alloc_exportable(&device, PITCH * ROWS)
            .expect("exportable allocation");
        assert!(exportable.size() >= PITCH * ROWS, "rounded down");
        println!(
            "{} bytes asked, {} mapped, descriptor {:?}",
            PITCH * ROWS,
            exportable.size(),
            exportable.fd()
        );

        let pattern: Vec<u8> = (0..PITCH * ROWS)
            .map(|i| u8::try_from(i % 251).expect("under 256"))
            .collect();
        // SAFETY: the allocation covers the rows.
        unsafe { cuda.write_rows(exportable.ptr(), PITCH, &pattern, PITCH, PITCH, ROWS) }
            .expect("write");

        // The import takes ownership of what it is given, so it is given a
        // duplicate and the allocation keeps its own.
        let dup = exportable.fd().try_clone_to_owned().expect("duplicate");
        let size = u64::try_from(exportable.size()).expect("size");
        // SAFETY: the context is current and the descriptor is an opaque
        // one this runtime exported.
        let external = unsafe { cuda.import(dup, size) }.expect("import");
        let plane = external
            .plane(0, u64::try_from(PITCH * ROWS).expect("size"), PITCH)
            .expect("plane");
        let mut back = vec![0u8; PITCH * ROWS];
        // SAFETY: the import covers the rows.
        unsafe { cuda.read_rows(plane.ptr(), PITCH, &mut back, PITCH, PITCH, ROWS) }.expect("read");
        assert!(
            back == pattern,
            "the import does not see what the allocation holds"
        );
    }
}
