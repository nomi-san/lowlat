//! The machine's own libavcodec, resolved by name at runtime, and only an
//! LGPL build of it.
//!
//! **Nothing is shipped and nothing is linked.** The pair `libavutil` +
//! `libavcodec` is looked for on the machine (docs/10-client.md section 5.1):
//! in the environment first, then beside the running executable, then the
//! dynamic linker's own way; majors 4 through 9, the highest that opens
//! winning. Before any other entry point is called the pair is asked its
//! licence, and one that does not answer `LGPL` is closed at once and refused.
//!
//! **No header is pinned.** What is relied on is the same on every major
//! accepted: the entry points below, the leading fields of a frame and a
//! packet, the padding a unit needs and one error code. Everything numbered
//! that has moved between majors, or could -- codec identifiers, pixel
//! formats -- is resolved by name at load. The two field layouts are checked
//! at load against the library that loaded, before a unit is ever fed.

use core::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::ffi::CString;
use std::path::{Path, PathBuf};

use lowlat_common::dynlib::Library;

/// The pairs accepted, highest first: `(libavcodec major, libavutil major,
/// FFmpeg major)`.
const MAJORS: [(u32, u32, u32); 6] = [
    (63, 61, 9),
    (62, 60, 8),
    (61, 59, 7),
    (60, 58, 6),
    (59, 57, 5),
    (58, 56, 4),
];

/// The directory the pair is taken from, over any other place.
pub const DIR_VARIABLE: &str = "LOWLAT_FFMPEG_DIR";
/// The FFmpeg major (4 through 9) the pair must be, over the walk.
pub const VERSION_VARIABLE: &str = "LOWLAT_FFMPEG_VERSION";

/// Zero bytes every unit fed to a decoder must be followed by
/// (`AV_INPUT_BUFFER_PADDING_SIZE`); a packet the library sizes carries
/// them itself.
pub const PADDING: usize = 64;
/// `AVERROR(EAGAIN)`: the decoder wants a picture taken before it takes
/// another unit, or has no picture ready yet.
pub const EAGAIN: c_int = -(libc::EAGAIN);
/// `AV_NOPTS_VALUE`, what a fresh packet's timestamps read.
pub const NOPTS: i64 = i64::MIN;

/// Opaque to us: only ever a pointer.
#[derive(Debug)]
pub enum AVCodec {}
#[derive(Debug)]
pub enum AVCodecContext {}
#[derive(Debug)]
pub enum AVDictionary {}
#[derive(Debug)]
pub enum AVBufferRef {}

/// The leading fields of a decoded frame, the same on every major accepted.
/// Only ever read through a pointer the library handed out; the rest of the
/// structure is the library's and is never named.
#[repr(C)]
#[derive(Debug)]
pub struct Frame {
    pub data: [*mut u8; 8],
    pub linesize: [c_int; 8],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
}

/// The leading fields of a packet, likewise.
#[repr(C)]
#[derive(Debug)]
pub struct Packet {
    pub buf: *mut AVBufferRef,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
}

pub type FrameAlloc = unsafe extern "C" fn() -> *mut Frame;
pub type FrameFree = unsafe extern "C" fn(*mut *mut Frame);
pub type FrameUnref = unsafe extern "C" fn(*mut Frame);
pub type PacketAlloc = unsafe extern "C" fn() -> *mut Packet;
pub type PacketFree = unsafe extern "C" fn(*mut *mut Packet);
pub type PacketUnref = unsafe extern "C" fn(*mut Packet);
/// A reference-counted buffer of the size given, padded as a decoder
/// requires, owned by the packet.
pub type NewPacket = unsafe extern "C" fn(*mut Packet, c_int) -> c_int;
pub type DictSet =
    unsafe extern "C" fn(*mut *mut AVDictionary, *const c_char, *const c_char, c_int) -> c_int;
pub type DictFree = unsafe extern "C" fn(*mut *mut AVDictionary);
/// An option read off an object whose first field is its option class; the
/// codec context is one, a frame is not.
pub type OptGetInt = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut i64) -> c_int;
pub type GetPixFmt = unsafe extern "C" fn(*const c_char) -> c_int;
pub type GetPixFmtName = unsafe extern "C" fn(c_int) -> *const c_char;
pub type FindDecoderByName = unsafe extern "C" fn(*const c_char) -> *const AVCodec;
pub type AllocContext = unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext;
pub type FreeContext = unsafe extern "C" fn(*mut *mut AVCodecContext);
pub type Open =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int;
pub type SendPacket = unsafe extern "C" fn(*mut AVCodecContext, *const Packet) -> c_int;
pub type ReceiveFrame = unsafe extern "C" fn(*mut AVCodecContext, *mut Frame) -> c_int;
type Version = unsafe extern "C" fn() -> c_uint;
type Licence = unsafe extern "C" fn() -> *const c_char;

/// Why no pair could be used. The walk keeps the most telling refusal: a
/// pair found and refused for its licence says more than none found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Refusal {
    /// No pair opened anywhere it was looked for.
    Absent,
    /// A pair opened and reported majors other than its names promised.
    Version,
    /// A pair opened and lacks an entry point it must export.
    MissingSymbol,
    /// A pair opened and its frames or packets are not laid out as every
    /// major accepted lays them out.
    Layout,
    /// A pair opened and decodes neither codec.
    NoDecoder,
    /// A pair opened and answered a licence other than the LGPL.
    Licence,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Absent => "no codec library pair found",
            Self::Version => "codec library pair reports other majors than its names",
            Self::MissingSymbol => "codec library is missing an entry point",
            Self::Layout => "codec library lays its frames out unexpectedly",
            Self::NoDecoder => "codec library decodes neither codec",
            Self::Licence => "codec library is not an LGPL build",
        })
    }
}

impl std::error::Error for Refusal {}

/// Where the pair was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A directory: the environment's, the caller's, or the executable's.
    Directory(PathBuf),
    /// The dynamic linker's own search.
    Default,
}

/// The pixel formats a decoder may hand out, as this library numbers them.
/// A name the library does not know is `-1`, which no frame ever carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats {
    pub yuv420p: c_int,
    pub yuv444p: c_int,
    /// The full-range twins older majors hand out for the same layouts.
    pub yuvj420p: c_int,
    pub yuvj444p: c_int,
    /// Ten bits in the low bits of sixteen.
    pub yuv420p10le: c_int,
    pub yuv444p10le: c_int,
}

/// The loaded pair.
///
/// The entry points are public because the backend calls them directly;
/// every call is `unsafe` at the site, with the argument contract stated
/// there.
#[derive(Debug)]
pub struct Lavc {
    pub frame_alloc: FrameAlloc,
    pub frame_free: FrameFree,
    pub frame_unref: FrameUnref,
    pub packet_alloc: PacketAlloc,
    pub packet_free: PacketFree,
    pub packet_unref: PacketUnref,
    pub new_packet: NewPacket,
    pub dict_set: DictSet,
    pub dict_free: DictFree,
    pub opt_get_int: OptGetInt,
    pub get_pix_fmt_name: GetPixFmtName,
    pub find_decoder_by_name: FindDecoderByName,
    pub alloc_context: AllocContext,
    pub free_context: FreeContext,
    pub open: Open,
    pub send_packet: SendPacket,
    pub receive_frame: ReceiveFrame,
    pub formats: Formats,
    /// The FFmpeg major, 4 through 9.
    pub major: u32,
    /// `libavcodec`'s own version, `(major, minor, micro)`.
    pub version: (u32, u32, u32),
    /// What the pair answered, for the log and the listing.
    pub licence: String,
    pub origin: Origin,
    pub decodes_h264: bool,
    pub decodes_hevc: bool,
    /// Last, so both outlive the addresses taken from them.
    _avcodec: Library,
    _avutil: Library,
}

/// The one thing the walk needs of a place: how to name a library in it.
#[derive(Debug, Clone)]
enum Place {
    Directory(PathBuf),
    Default,
}

impl Place {
    fn name(&self, file: &str) -> Option<CString> {
        match self {
            Self::Directory(dir) => {
                CString::new(dir.join(file).into_os_string().into_encoded_bytes()).ok()
            }
            Self::Default => CString::new(file).ok(),
        }
    }

    fn origin(&self) -> Origin {
        match self {
            Self::Directory(dir) => Origin::Directory(dir.clone()),
            Self::Default => Origin::Default,
        }
    }
}

/// What the environment says, read once per load.
#[derive(Debug, Default)]
struct Environment {
    dir: Option<PathBuf>,
    /// The major asked for; a value that is not a number is zero, which no
    /// pair has, so a variable set wrongly refuses rather than walks.
    major: Option<u32>,
}

impl Environment {
    fn read() -> Self {
        Self {
            dir: std::env::var_os(DIR_VARIABLE)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            major: std::env::var(VERSION_VARIABLE)
                .ok()
                .map(|v| v.trim().parse::<u32>().unwrap_or(0)),
        }
    }
}

impl Lavc {
    /// Find and open the pair, or say why none could be used.
    ///
    /// `dir` is a directory the caller names, below the environment's and
    /// above the executable's. Where a pair is named -- by the environment
    /// or by the caller -- a pair that fails is the answer, never a walk to
    /// the next place.
    pub fn load(dir: Option<&Path>) -> Result<Self, Refusal> {
        Self::load_with(Environment::read(), dir)
    }

    fn load_with(environment: Environment, dir: Option<&Path>) -> Result<Self, Refusal> {
        let majors: Vec<(u32, u32, u32)> = match environment.major {
            Some(wanted) => MAJORS.iter().copied().filter(|m| m.2 == wanted).collect(),
            None => MAJORS.to_vec(),
        };
        if majors.is_empty() {
            lowlat_common::log_warn!(
                "decode: {VERSION_VARIABLE} names a major this library does not accept, wanted={}",
                environment.major.unwrap_or(0)
            );
            return Err(Refusal::Absent);
        }
        let named = environment
            .dir
            .clone()
            .or_else(|| dir.map(Path::to_path_buf));
        let places: Vec<Place> = match named {
            Some(dir) => vec![Place::Directory(dir)],
            None => {
                let mut places = Vec::with_capacity(2);
                if let Some(beside) = std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(Path::to_path_buf))
                {
                    places.push(Place::Directory(beside));
                }
                places.push(Place::Default);
                places
            }
        };
        let mut worst = Refusal::Absent;
        for place in &places {
            for &(avcodec, avutil, major) in &majors {
                match Self::try_pair(place, avcodec, avutil, major) {
                    Ok(loaded) => {
                        lowlat_common::log_info!(
                            "decode: libavcodec loaded, major={} version={}.{}.{} licence=\"{}\" from={}",
                            loaded.major,
                            loaded.version.0,
                            loaded.version.1,
                            loaded.version.2,
                            loaded.licence,
                            match &loaded.origin {
                                Origin::Directory(dir) => dir.display().to_string(),
                                Origin::Default => "the default search".to_string(),
                            }
                        );
                        return Ok(loaded);
                    }
                    Err(Refusal::Absent) => {}
                    Err(refusal) => worst = worst.max(refusal),
                }
            }
        }
        Err(worst)
    }

    /// One pair in one place: opened, checked, and kept or closed.
    fn try_pair(place: &Place, avcodec: u32, avutil: u32, major: u32) -> Result<Self, Refusal> {
        let avutil_name = place
            .name(&format!("libavutil.so.{avutil}"))
            .ok_or(Refusal::Absent)?;
        let avcodec_name = place
            .name(&format!("libavcodec.so.{avcodec}"))
            .ok_or(Refusal::Absent)?;
        // `libavutil` first: `libavcodec` needs it, and a copy already in the
        // process is what the linker binds it to.
        let util = Library::open(&avutil_name).ok_or(Refusal::Absent)?;
        let Some(codec) = Library::open(&avcodec_name) else {
            util.close();
            return Err(Refusal::Absent);
        };
        match Self::check(place, util, codec, avcodec, avutil, major) {
            Ok(loaded) => Ok(loaded),
            Err((refusal, util, codec)) => {
                lowlat_common::log_info!(
                    "decode: libavcodec refused, major={major} reason={refusal} from={}",
                    match place {
                        Place::Directory(dir) => dir.display().to_string(),
                        Place::Default => "the default search".to_string(),
                    }
                );
                // Reverse order of opening, so nothing is closed under a
                // library that still names it.
                codec.close();
                util.close();
                Err(refusal)
            }
        }
    }

    /// Everything asked of a pair before it is trusted, in the order the
    /// rules require: the versions and the licence through the four
    /// constant-returning entry points, nothing else until the licence has
    /// answered; then the table, the formats, the codecs and the layouts.
    #[allow(
        clippy::type_complexity,
        reason = "the pair travels back with its refusal"
    )]
    fn check(
        place: &Place,
        util: Library,
        codec: Library,
        avcodec: u32,
        avutil: u32,
        major: u32,
    ) -> Result<Self, (Refusal, Library, Library)> {
        macro_rules! symbol {
            ($library:expr, $name:literal) => {
                // SAFETY: every signature is the library's documented one,
                // unchanged across the majors accepted.
                match unsafe { $library.symbol($name) } {
                    Some(f) => f,
                    None => return Err((Refusal::MissingSymbol, util, codec)),
                }
            };
        }
        let avcodec_version: Version = symbol!(codec, c"avcodec_version");
        let avutil_version: Version = symbol!(util, c"avutil_version");
        let avcodec_licence: Licence = symbol!(codec, c"avcodec_license");
        let avutil_licence: Licence = symbol!(util, c"avutil_license");
        // SAFETY: constant-returning entry points with no arguments.
        let (codec_version, util_version) = unsafe { (avcodec_version(), avutil_version()) };
        if codec_version >> 16 != avcodec || util_version >> 16 != avutil {
            return Err((Refusal::Version, util, codec));
        }
        // SAFETY: as above; each returns a pointer to a static string.
        let licence = unsafe { text(avcodec_licence()) };
        let util_licence = unsafe { text(avutil_licence()) };
        if !(licence.starts_with("LGPL") && util_licence.starts_with("LGPL")) {
            lowlat_common::log_info!(
                "decode: libavcodec answered licence=\"{licence}\" avutil=\"{util_licence}\""
            );
            return Err((Refusal::Licence, util, codec));
        }

        let get_pix_fmt: GetPixFmt = symbol!(util, c"av_get_pix_fmt");
        let find_decoder_by_name: FindDecoderByName =
            symbol!(codec, c"avcodec_find_decoder_by_name");
        let frame_alloc: FrameAlloc = symbol!(util, c"av_frame_alloc");
        let frame_free: FrameFree = symbol!(util, c"av_frame_free");
        let packet_alloc: PacketAlloc = symbol!(codec, c"av_packet_alloc");
        let packet_free: PacketFree = symbol!(codec, c"av_packet_free");
        let new_packet: NewPacket = symbol!(codec, c"av_new_packet");
        let loaded = Self {
            frame_alloc,
            frame_free,
            frame_unref: symbol!(util, c"av_frame_unref"),
            packet_alloc,
            packet_free,
            packet_unref: symbol!(codec, c"av_packet_unref"),
            new_packet,
            dict_set: symbol!(util, c"av_dict_set"),
            dict_free: symbol!(util, c"av_dict_free"),
            opt_get_int: symbol!(util, c"av_opt_get_int"),
            get_pix_fmt_name: symbol!(util, c"av_get_pix_fmt_name"),
            find_decoder_by_name,
            alloc_context: symbol!(codec, c"avcodec_alloc_context3"),
            free_context: symbol!(codec, c"avcodec_free_context"),
            open: symbol!(codec, c"avcodec_open2"),
            send_packet: symbol!(codec, c"avcodec_send_packet"),
            receive_frame: symbol!(codec, c"avcodec_receive_frame"),
            // SAFETY: a name lookup on a static table; unknown names answer -1.
            formats: unsafe {
                Formats {
                    yuv420p: get_pix_fmt(c"yuv420p".as_ptr()),
                    yuv444p: get_pix_fmt(c"yuv444p".as_ptr()),
                    yuvj420p: get_pix_fmt(c"yuvj420p".as_ptr()),
                    yuvj444p: get_pix_fmt(c"yuvj444p".as_ptr()),
                    yuv420p10le: get_pix_fmt(c"yuv420p10le".as_ptr()),
                    yuv444p10le: get_pix_fmt(c"yuv444p10le".as_ptr()),
                }
            },
            major,
            version: (
                codec_version >> 16,
                (codec_version >> 8) & 0xff,
                codec_version & 0xff,
            ),
            licence,
            origin: place.origin(),
            // SAFETY: a name lookup on the library's own registry.
            decodes_h264: unsafe { !find_decoder_by_name(c"h264".as_ptr()).is_null() },
            decodes_hevc: unsafe { !find_decoder_by_name(c"hevc".as_ptr()).is_null() },
            _avcodec: codec,
            _avutil: util,
        };
        if !(loaded.decodes_h264 || loaded.decodes_hevc) {
            return Err((Refusal::NoDecoder, loaded._avutil, loaded._avcodec));
        }
        if !loaded.layout_holds() {
            return Err((Refusal::Layout, loaded._avutil, loaded._avcodec));
        }
        Ok(loaded)
    }

    /// The two views checked against this library: a fresh frame points its
    /// extended data at its own first field and reports no format; a fresh
    /// packet sized to sixteen bytes reports sixteen, a buffer, and no
    /// timestamp. Every major accepted sets exactly these.
    fn layout_holds(&self) -> bool {
        // SAFETY: allocated and freed through the library's own pair of
        // entry points; the fields read are the leading ones of the view.
        unsafe {
            let mut frame = (self.frame_alloc)();
            if frame.is_null() {
                return false;
            }
            let extended = core::ptr::addr_of!((*frame).extended_data).read();
            let format = core::ptr::addr_of!((*frame).format).read();
            let width = core::ptr::addr_of!((*frame).width).read();
            let frame_ok = extended.cast_const() == core::ptr::addr_of!((*frame).data).cast()
                && format == -1
                && width == 0;
            (self.frame_free)(&raw mut frame);

            let mut packet = (self.packet_alloc)();
            if packet.is_null() {
                return false;
            }
            let sized = (self.new_packet)(packet, 16) == 0;
            let size = core::ptr::addr_of!((*packet).size).read();
            let data = core::ptr::addr_of!((*packet).data).read();
            let pts = core::ptr::addr_of!((*packet).pts).read();
            let packet_ok = sized && size == 16 && !data.is_null() && pts == NOPTS;
            (self.packet_free)(&raw mut packet);
            frame_ok && packet_ok
        }
    }

    /// The name of a pixel format, for a log line. "unknown" for none.
    pub fn format_name(&self, format: c_int) -> String {
        // SAFETY: a lookup on a static table; null for an unknown number.
        unsafe { text((self.get_pix_fmt_name)(format)) }
    }
}

/// A static string of the library's as text; empty for null.
///
/// # Safety
///
/// `s` is null or points at a NUL-terminated string that outlives the copy.
unsafe fn text(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding an LGPL pair, for the tests that need one. Named
    /// by a variable of the tests' own so the loader's variables are never
    /// set in a process whose tests run side by side.
    fn pair_dir() -> Option<PathBuf> {
        std::env::var_os("LOWLAT_LAVC_TEST_DIR").map(PathBuf::from)
    }

    /// The codec libraries in the process map, one line each.
    #[cfg(target_os = "linux")]
    fn mapped() -> Vec<String> {
        std::fs::read_to_string("/proc/self/maps")
            .expect("the process map")
            .lines()
            .filter(|l| l.contains("libavcodec") || l.contains("libavutil"))
            .filter_map(|l| l.split_whitespace().last().map(str::to_string))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// **The pair is trusted only on its own word.** Whatever the default
    /// search finds is accepted with an LGPL answer or refused with any
    /// other, and a refused pair leaves nothing new in the process map.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_default_search_answers_with_its_own_licence() {
        let before = mapped();
        match Lavc::load_with(Environment::default(), None) {
            Ok(loaded) => {
                println!(
                    "loaded major {} version {:?} licence {:?} from {:?}",
                    loaded.major, loaded.version, loaded.licence, loaded.origin
                );
                assert!(loaded.licence.starts_with("LGPL"));
                assert!((4..=9).contains(&loaded.major));
            }
            Err(refusal @ (Refusal::Licence | Refusal::Absent)) => {
                println!("refused: {refusal}");
                assert_eq!(mapped(), before, "a refused pair stayed mapped");
            }
            Err(other) => panic!("a pair opened and was refused for {other}"),
        }
    }

    /// A directory the caller names is the pair, opened at the highest major
    /// present there; the formats are what this library numbers them, which
    /// on a 4.x pair is not what a 5.x and later pair numbers them.
    #[test]
    #[ignore = "needs an LGPL pair named by LOWLAT_LAVC_TEST_DIR"]
    fn a_named_directory_loads_at_its_highest_major() {
        let dir = pair_dir().expect("LOWLAT_LAVC_TEST_DIR");
        let loaded = Lavc::load_with(Environment::default(), Some(&dir)).expect("the pair");
        assert_eq!(loaded.origin, Origin::Directory(dir.clone()));
        let named = MAJORS
            .iter()
            .find(|m| m.2 == loaded.major)
            .expect("an accepted major");
        assert_eq!(
            loaded.version.0, named.0,
            "the reported major is the name's"
        );
        assert!(loaded.decodes_h264 && loaded.decodes_hevc);
        assert_eq!(loaded.formats.yuv420p, 0);
        assert_eq!(loaded.formats.yuv444p, 5);
        let ten = if loaded.major == 4 { 64 } else { 62 };
        assert_eq!(loaded.formats.yuv420p10le, ten, "major {}", loaded.major);
        assert_eq!(
            loaded.formats.yuv444p10le,
            ten + 6,
            "major {}",
            loaded.major
        );
        assert_eq!(
            loaded.format_name(loaded.formats.yuv420p10le),
            "yuv420p10le"
        );
        println!(
            "loaded major {} version {:?} licence {:?}",
            loaded.major, loaded.version, loaded.licence
        );
        // The environment's directory takes the same road.
        let again = Lavc::load_with(
            Environment {
                dir: Some(dir.clone()),
                major: None,
            },
            Some(Path::new("/nowhere")),
        )
        .expect("the environment's pair");
        assert_eq!(again.origin, Origin::Directory(dir));
    }

    /// A major the environment names is the only one tried: present, it
    /// loads; absent, the answer is a refusal and never a walk to another.
    #[test]
    #[ignore = "needs an LGPL pair named by LOWLAT_LAVC_TEST_DIR"]
    fn a_named_major_is_the_only_one_tried() {
        let dir = pair_dir().expect("LOWLAT_LAVC_TEST_DIR");
        let loaded = Lavc::load_with(Environment::default(), Some(&dir)).expect("the pair");
        let same = Lavc::load_with(
            Environment {
                dir: None,
                major: Some(loaded.major),
            },
            Some(&dir),
        )
        .expect("the same major, named");
        assert_eq!(same.major, loaded.major);
        for absent in [1, 3, 10, 0] {
            assert_eq!(
                Lavc::load_with(
                    Environment {
                        dir: None,
                        major: Some(absent),
                    },
                    Some(&dir),
                )
                .err(),
                Some(Refusal::Absent),
                "major {absent}"
            );
        }
    }

    /// A directory with no pair in it refuses as absent, whatever the
    /// machine has elsewhere.
    #[test]
    fn an_empty_directory_is_absent_and_not_a_walk() {
        let dir = std::env::temp_dir().join(format!("lowlat-lavc-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        assert_eq!(
            Lavc::load_with(Environment::default(), Some(&dir)).err(),
            Some(Refusal::Absent)
        );
        assert_eq!(
            Lavc::load_with(
                Environment {
                    dir: Some(dir.clone()),
                    major: None
                },
                None
            )
            .err(),
            Some(Refusal::Absent)
        );
        let _ = std::fs::remove_dir(&dir);
    }

    /// The refusals rank: a pair refused for its licence is what the walk
    /// reports over one that was merely absent.
    #[test]
    fn the_most_telling_refusal_wins() {
        assert!(Refusal::Licence > Refusal::Layout);
        assert!(Refusal::Layout > Refusal::MissingSymbol);
        assert!(Refusal::MissingSymbol > Refusal::Version);
        assert!(Refusal::Version > Refusal::Absent);
        assert_eq!(Refusal::Absent.max(Refusal::Licence), Refusal::Licence);
    }
}
