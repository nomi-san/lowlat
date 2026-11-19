//! The encoder's device on Linux: a compute context, its inputs pitched
//! allocations of that context, and completion observed through a marker
//! recorded behind each picture on the encoder's own stream.
//!
//! The runtime offers no asynchronous mode here, so nothing tells the collect
//! a picture is done; the marker is asked instead, and the lock absorbs the
//! rest of the wait (see [`Encoder::poll`]).

use crate::ffi::nvenc::NV_ENC_SUCCESS;

use super::{Api, Chroma, Config, Encoder, Error, IN_FLIGHT, Input, Session, widen};

impl Chroma {
    /// Rows one planar frame occupies at this chroma, luma included.
    ///
    /// Both layouts are one allocation at one pitch with the planes stacked,
    /// which is why the registration takes a single address either way: 4:2:0
    /// is luma then half as many rows of interleaved colour, 4:4:4 is three
    /// full planes.
    const fn rows_for(self, height: u32) -> u32 {
        match self {
            Self::Yuv420 => height + height.div_ceil(2),
            Self::Yuv444 => height * 3,
        }
    }
}

impl<'a> Api {
    /// Open a session against a compute context.
    ///
    /// The context is taken rather than borrowed: an encoder outliving the
    /// context it was opened against is a use-after-free that presents as a
    /// driver fault with no line number.
    pub fn open_session(&'a self, context: crate::cuda::Context) -> Result<Session<'a>, Error> {
        // Everything from here on runs against whatever context is current on
        // this thread, including allocations made for it, so bind it now
        // rather than leaving each call site to remember.
        context.make_current().map_err(|_| Error::NoContext)?;

        let open = self
            .functions
            .nvEncOpenEncodeSessionEx
            .ok_or(Error::MissingSymbol)?;

        let mut params = unsafe {
            core::mem::zeroed::<crate::ffi::nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>()
        };
        params.version = crate::ffi::versions::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = crate::ffi::nvenc::NV_ENC_DEVICE_TYPE_CUDA;
        params.device = context.raw().cast();
        params.apiVersion = crate::ffi::nvenc::NVENCAPI_VERSION;

        let mut encoder: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: both pointers are to live storage for the duration of the
        // call, and the parameter block is stamped with its own version.
        let status = unsafe { open(&raw mut params, &raw mut encoder) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        Ok(Session {
            api: self,
            encoder,
            _context: context,
        })
    }
}

/// A kernel module and a userspace library from two different driver releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    /// The release of the module the kernel has loaded.
    pub kernel: String,
    /// The release of the encoder library this process has mapped.
    pub library: String,
}

/// Where the loaded kernel module says which release it is.
const KERNEL_VERSION: &str = "/proc/driver/nvidia/version";

/// The release a kernel module's version file names.
///
/// The line reads `NVRM version: NVIDIA UNIX Open Kernel Module for x86_64
/// 620.70.06  Release Build ...`, and the closed module words it differently
/// around the same number, so the number is found by its shape rather than by
/// its place.
fn kernel_release(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("NVRM version:"))?
        .split_whitespace()
        .find(|word| is_release(word))
}

/// The release of the encoder library in a process's memory map.
///
/// **Read off the map rather than off the disk**, because the map names the
/// file the loader actually resolved the versioned alias to, which is the one
/// the kernel module is refusing; a path looked up afresh is only what a new
/// process would get.
fn library_release(maps: &str) -> Option<&str> {
    const NAME: &str = "libnvidia-encode.so.";
    maps.lines().find_map(|line| {
        let after = &line[line.find(NAME)? + NAME.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        let release = after[..end].trim_end_matches('.');
        // **An alias is not a release.** A library mapped under `.so.1` says
        // nothing about which driver it belongs to, and comparing that `1`
        // with the module's number would call every such machine stale.
        is_release(release).then_some(release)
    })
}

/// Digits in at least two dotted groups, which is every release the vendor
/// has numbered and no alias.
fn is_release(word: &str) -> bool {
    word.contains('.')
        && word
            .split('.')
            .all(|group| !group.is_empty() && group.bytes().all(|byte| byte.is_ascii_digit()))
}

impl Api {
    /// Whether the kernel module and this library are different releases, and
    /// which two.
    ///
    /// **This is what a driver update without a reboot looks like.** The
    /// package replaces the libraries at once and the module only at the next
    /// boot, and the two refuse each other: seen with a 615.78.08 module under
    /// 620.70.06 libraries, where the library loaded, the function table came
    /// back, and every session refused to open with a status that names
    /// neither number. Nothing but a reboot (or the module reloaded) ends it,
    /// so it is worth telling apart from a device that is merely busy.
    ///
    /// A method of the loaded runtime because the library has to be mapped
    /// for its release to be read. Nothing when either number cannot be
    /// found, which is not evidence of anything.
    pub fn stale_driver(&self) -> Option<Stale> {
        let kernel = std::fs::read_to_string(KERNEL_VERSION).ok()?;
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        let kernel = kernel_release(&kernel)?;
        let library = library_release(&maps)?;
        (kernel != library).then(|| Stale {
            kernel: kernel.to_string(),
            library: library.to_string(),
        })
    }
}

impl<'a> Session<'a> {
    /// Configure the encoder for low latency and initialise it.
    pub fn initialize(
        self,
        cuda: &crate::cuda::Cuda,
        config: Config,
    ) -> Result<Encoder<'a>, Error> {
        let (encode_config, init_params, outputs) = self.configure(&config, false)?;

        // Our own stream, so completion can be marked behind each picture.
        // Without this the encoder runs on the default stream, where a marker
        // would also catch unrelated work.
        let stream = Box::new(cuda.create_stream().map_err(|_| Error::NoContext)?);
        if let Some(set_streams) = self.api.functions.nvEncSetIOCudaStreams {
            // SAFETY: the encoder is live, and the pointer is to the boxed
            // handle, which outlives the encoder because it is owned by the
            // value returned below and does not move with it. One stream
            // serves both directions: there is a single producer here.
            let status =
                unsafe { set_streams(self.encoder, stream.handle_ptr(), stream.handle_ptr()) };
            if status != NV_ENC_SUCCESS {
                return Err(Error::Status(status));
            }
        }
        let events = [
            cuda.create_event().map_err(|_| Error::NoContext)?,
            cuda.create_event().map_err(|_| Error::NoContext)?,
            cuda.create_event().map_err(|_| Error::NoContext)?,
            cuda.create_event().map_err(|_| Error::NoContext)?,
        ];

        let mut encoder = Encoder {
            session: self,
            config,
            stream,
            events,
            encode_config,
            init_params,
            outputs,
            next_output: 0,
            inputs: Vec::with_capacity(IN_FLIGHT),
            pending: std::collections::VecDeque::with_capacity(IN_FLIGHT),
            collected: Vec::new(),
            last_lock: None,
        };
        encoder.allocate_inputs(cuda)?;
        Ok(encoder)
    }
}

/// One slot of the input pool: the allocation and its registration.
///
/// The buffer is declared first so it is freed before the registration that
/// names it is dropped.
#[derive(Debug)]
pub(super) struct InputSlot {
    buffer: crate::cuda::DeviceBuffer,
    pub(super) input: Input,
}

impl Encoder<'_> {
    /// Write a frame into one slot of the input pool.
    ///
    /// The two planes are one allocation: luma at row zero, chroma directly
    /// after the last luma row. That is the layout the format declared at
    /// registration means, and the encoder derives the chroma offset from the
    /// height it was given rather than being told separately.
    pub(super) fn upload(
        &self,
        slot: usize,
        frame: &lowlat_capture::Frame<'_>,
    ) -> Result<(), Error> {
        // **Refused rather than written into the top of a larger pool.** A
        // frame from ordinary memory has two planes and full chroma needs
        // three, so there is no widening to do here the way there is for the
        // depth: the type cannot carry the picture. Written anyway, the
        // subsampled bytes land in the top of a pool registered as full
        // chroma, and what comes out is a picture with a third of its colour
        // and two thirds of whatever the allocation held. Nothing else
        // refuses it, because the address and the row length are both what the
        // registration promised.
        if self.config.chroma == Chroma::Yuv444 {
            return Err(Error::ChromaMismatch);
        }
        let held = self.inputs.get(slot).ok_or(Error::NoContext)?;
        let width = usize::try_from(frame.width).unwrap_or(0);
        let rows = usize::try_from(frame.height).unwrap_or(0);
        let chroma_width = width.div_ceil(2) * 2;
        let chroma_rows = rows.div_ceil(2);

        // **A frame from ordinary memory is eight bits and the pool may not
        // be.** Widened here rather than refused, because this is the path a
        // synthetic source and the device tests take; a picture converted on
        // the device already arrives at the pool's depth and is handed over by
        // reference without passing through this at all.
        if self.config.depth.ten_bit() {
            let luma = widen(&frame.luma, width, rows);
            let chroma = widen(&frame.chroma, chroma_width, chroma_rows);
            held.buffer
                .write_rows(0, &luma, width * 2, width * 2, rows)
                .map_err(|_| Error::NoContext)?;
            return held
                .buffer
                .write_rows(
                    rows,
                    &chroma,
                    chroma_width * 2,
                    chroma_width * 2,
                    chroma_rows,
                )
                .map_err(|_| Error::NoContext);
        }

        held.buffer
            .write_rows(0, frame.luma.bytes, frame.luma.stride, width, rows)
            .map_err(|_| Error::NoContext)?;
        held.buffer
            .write_rows(
                rows,
                frame.chroma.bytes,
                frame.chroma.stride,
                chroma_width,
                chroma_rows,
            )
            .map_err(|_| Error::NoContext)
    }

    /// Allocate and register the input pool, one surface per in-flight
    /// picture.
    fn allocate_inputs(&mut self, cuda: &crate::cuda::Cuda) -> Result<(), Error> {
        // One planar frame per allocation: luma rows, then half as many rows
        // of interleaved chroma, at the driver's preferred pitch.
        //
        // **The depth widens a row, it does not add rows.** Ten bits are one
        // sample to a sixteen-bit word, so the picture is the same shape at
        // twice the bytes across; sizing this by rows alone hands the encoder
        // half the picture it was promised.
        let width =
            usize::try_from(self.config.width).unwrap_or(0) * self.config.depth.bytes_per_sample();
        let rows = usize::try_from(self.config.chroma.rows_for(self.config.height)).unwrap_or(0);
        for _ in 0..IN_FLIGHT {
            let buffer = cuda
                .alloc_pitch(width, rows)
                .map_err(|_| Error::NoContext)?;
            let input = self.register(&buffer)?;
            self.inputs.push(InputSlot { buffer, input });
        }
        Ok(())
    }

    /// Register a device allocation holding one planar frame.
    pub fn register(&self, buffer: &crate::cuda::DeviceBuffer) -> Result<Input, Error> {
        self.register_ptr(buffer.ptr(), buffer.pitch())
    }

    /// Register one planar frame by address.
    ///
    /// **One address and one row length, and the colour plane is assumed to
    /// begin exactly one luma plane in.** There is no field here in which to
    /// say otherwise, which is why a frame produced elsewhere has to be laid
    /// out that way before it arrives.
    pub fn register_ptr(
        &self,
        ptr: crate::ffi::cuda::CUdeviceptr,
        pitch: usize,
    ) -> Result<Input, Error> {
        use crate::ffi::nvenc as f;
        let register = self
            .functions()
            .nvEncRegisterResource
            .ok_or(Error::MissingSymbol)?;

        let mut params = unsafe { core::mem::zeroed::<f::NV_ENC_REGISTER_RESOURCE>() };
        params.version = crate::ffi::versions::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = f::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
        params.width = self.config.width;
        params.height = self.config.height;
        params.pitch = u32::try_from(pitch).map_err(|_| Error::MissingSymbol)?;
        params.resourceToRegister = ptr as *mut core::ffi::c_void;
        params.bufferFormat = self.config.chroma.buffer_format(self.config.depth);
        params.bufferUsage = f::NV_ENC_INPUT_IMAGE;

        // SAFETY: the block is stamped and live for the call.
        let status = unsafe { register(self.session.encoder, &raw mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(Input {
            registered: params.registeredResource,
            width: self.config.width,
            height: self.config.height,
            pitch: params.pitch,
        })
    }
}

impl Encoder<'_> {
    /// Nothing to name on the picture: the marker is recorded behind it once
    /// it is queued.
    pub(super) fn arm(&mut self, _slot: usize, _pic: &mut crate::ffi::nvenc::NV_ENC_PIC_PARAMS) {}

    /// Record the slot's marker behind the work queued so far.
    pub(super) fn mark(&self, slot: usize) -> Result<(), Error> {
        self.events[slot]
            .record(&self.stream)
            .map_err(|_| Error::NoContext)
    }

    /// Whether the slot's marker has passed: asked, never waited on.
    pub(super) fn passed(&self, slot: usize) -> Result<bool, Error> {
        self.events[slot].ready().map_err(|_| Error::NoContext)
    }

    /// Nothing to wait for before a teardown: what is still queued is
    /// unmapped as it stands.
    pub(super) fn settle(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::{kernel_release, library_release};

    /// **The number is found in both modules' wording**: the open module's
    /// line as this machine prints it, and the closed module's, which puts
    /// the architecture before the words instead of after.
    #[test]
    fn the_kernel_release_is_read_from_either_module() {
        let open = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  620.70.06  \
                    Release Build  (root@)  \nGCC version:  gcc version 16.2.1 20260810 (GCC) \n";
        assert_eq!(kernel_release(open), Some("620.70.06"));
        let closed = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Thu Feb 22 \
                      01:44:30 UTC 2024\n";
        assert_eq!(kernel_release(closed), Some("550.54.14"));
        assert_eq!(kernel_release("GCC version:  gcc version 16.2.1\n"), None);
        assert_eq!(kernel_release(""), None);
    }

    /// **Both numbers are there to be read on a machine with the driver**,
    /// which the two tests beside this cannot say: they read text written
    /// here, and this reads what the kernel and the loader wrote.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn both_releases_are_found_on_this_machine() {
        let _api = crate::nvenc::Api::load().expect("encoder runtime");
        let kernel = std::fs::read_to_string(super::KERNEL_VERSION).expect("the module's file");
        let maps = std::fs::read_to_string("/proc/self/maps").expect("the map");
        let kernel = kernel_release(&kernel).expect("the module's release");
        let library = library_release(&maps).expect("the library's release");
        println!("kernel module {kernel}, library {library}");
    }

    /// **The mapped file's release, and never an alias's number**: a file
    /// replaced underneath a running process is still named with its release,
    /// and a library mapped as `.so.1` names none.
    #[test]
    fn the_library_release_is_read_from_the_map() {
        let line = |path: &str| {
            format!(
                "7f10a0000000-7f10a0004000 r--p 00000000 00:1b 1234 {path}\n\
                 7f10a0004000-7f10a0040000 r-xp 00004000 00:1b 1234 {path}\n"
            )
        };
        assert_eq!(
            library_release(&line("/usr/lib/libnvidia-encode.so.620.70.06")),
            Some("620.70.06")
        );
        assert_eq!(
            library_release(&line("/usr/lib/libnvidia-encode.so.615.78.08 (deleted)")),
            Some("615.78.08")
        );
        assert_eq!(
            library_release(&line("/usr/lib/libnvidia-encode.so.1")),
            None
        );
        assert_eq!(library_release(&line("/usr/lib/libc.so.6")), None);
    }
}
