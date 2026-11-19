//! A brand file: the drivers' identity as a build names it
//! (docs/11-drivers.md section 1.10).
//!
//! **Shared with the build script**, which reads the file the build names
//! here and writes what it says as the crate's constants; the crate's tests
//! read brand files of their own through it. So nothing here names the crate,
//! and nothing allocates.
//!
//! One setting a line, `key = value`, the value running to the line's end; a
//! line starting `#` is a comment, a blank line nothing. **Every key is
//! required, and a key not known or given twice is refused**: a build takes
//! the identity it is given whole, or fails, and never fills a gap with
//! another brand's.

use core::fmt;

use super::edid;

/// A GUID as the file writes it: `{4a914f53-543f-46f9-b99b-d349a5aaf3e5}`,
/// the braces optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guid {
    pub(crate) data1: u32,
    pub(crate) data2: u16,
    pub(crate) data3: u16,
    pub(crate) data4: [u8; 8],
}

/// The EDID every monitor starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Monitor<'a> {
    /// A dumped EDID's file, its path from the brand file's folder.
    Dumped(&'a str),
    /// The block generated for this maker, product and name.
    Generated {
        maker: &'a str,
        product: u16,
        name: &'a str,
    },
}

/// What a brand file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Brand<'a> {
    /// The short name the driver's files, hardware ID, service and settings
    /// folder are named from.
    pub(crate) id: &'a str,
    /// The driver's provider, and the adapter's maker.
    pub(crate) provider: &'a str,
    /// The adapter's name.
    pub(crate) device: &'a str,
    /// The control interface.
    pub(crate) control: Guid,
    /// The monitors' container, the slot in its last byte.
    pub(crate) container: Guid,
    pub(crate) monitor: Monitor<'a>,
    /// The input device's name.
    pub(crate) input: &'a str,
    /// The input driver's control interface.
    pub(crate) input_control: Guid,
    /// The input devices' container, a device's kind and slot in its last
    /// two bytes.
    pub(crate) input_container: Guid,
}

/// Why a brand file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error<'a> {
    /// A line neither a setting, a comment nor blank: its number.
    Line(usize),
    /// A key not known.
    Unknown(&'a str),
    /// A key given twice.
    Twice(&'a str),
    /// A required key not given.
    Missing(&'static str),
    /// A key whose value is out of its form.
    Invalid(&'static str),
    /// A dumped EDID and the generated block's settings both, or neither.
    Edid,
    /// A GUID the brand already gives another of its keys.
    Shared(&'static str),
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(number) => write!(f, "line {number} is not `key = value`"),
            Self::Unknown(key) => write!(f, "`{key}` is not a key of a brand file"),
            Self::Twice(key) => write!(f, "`{key}` is given twice"),
            Self::Missing(key) => write!(f, "`{key}` is missing"),
            Self::Invalid(key) => write!(f, "`{key}` is out of its form"),
            Self::Edid => f.write_str(
                "the EDID is `edid` alone, or `edid-maker`, `edid-product` and `edid-name` together",
            ),
            Self::Shared(key) => write!(f, "`{key}` is a GUID the brand gives another key"),
        }
    }
}

const KEYS: [&str; 12] = [
    "id",
    "provider",
    "device",
    "control",
    "container",
    "edid",
    "edid-maker",
    "edid-product",
    "edid-name",
    "input",
    "input-control",
    "input-container",
];

/// The longest provider's or adapter's name.
const MAX_NAME: usize = 64;
/// The longest short name.
const MAX_ID: usize = 32;

/// Read a brand file.
pub(crate) fn read(text: &str) -> Result<Brand<'_>, Error<'_>> {
    let mut values: [Option<&str>; KEYS.len()] = [None; KEYS.len()];
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(Error::Line(index + 1))?;
        let key = key.trim();
        let slot = KEYS
            .iter()
            .position(|known| *known == key)
            .and_then(|at| values.get_mut(at))
            .ok_or(Error::Unknown(key))?;
        if slot.replace(value.trim()).is_some() {
            return Err(Error::Twice(key));
        }
    }
    let value = |key: &'static str| {
        KEYS.iter()
            .position(|known| *known == key)
            .and_then(|at| values.get(at).copied().flatten())
    };
    let required = |key: &'static str| value(key).ok_or(Error::Missing(key));
    let checked = |key: &'static str, ok: bool| ok.then_some(()).ok_or(Error::Invalid(key));

    let id = required("id")?;
    checked("id", id_valid(id))?;
    let provider = required("provider")?;
    checked("provider", name_valid(provider))?;
    let device = required("device")?;
    checked("device", name_valid(device))?;
    let control = guid(required("control")?).ok_or(Error::Invalid("control"))?;
    let container = guid(required("container")?).ok_or(Error::Invalid("container"))?;
    let input = required("input")?;
    checked("input", name_valid(input))?;
    let input_control = guid(required("input-control")?).ok_or(Error::Invalid("input-control"))?;
    let input_container =
        guid(required("input-container")?).ok_or(Error::Invalid("input-container"))?;
    // Each GUID names one thing: one copied into another's key would have a
    // controller open the other driver, or two kinds of device share one
    // container.
    let guids = [
        ("control", control),
        ("container", container),
        ("input-control", input_control),
        ("input-container", input_container),
    ];
    for (at, (key, one)) in guids.iter().enumerate() {
        if guids.iter().take(at).any(|(_, earlier)| earlier == one) {
            return Err(Error::Shared(key));
        }
    }
    let monitor = match (
        value("edid"),
        value("edid-maker"),
        value("edid-product"),
        value("edid-name"),
    ) {
        (Some(file), None, None, None) => {
            checked("edid", !file.is_empty())?;
            Monitor::Dumped(file)
        }
        (None, Some(maker), Some(product), Some(name)) => {
            checked("edid-maker", edid::maker_valid(maker.as_bytes()))?;
            checked("edid-name", edid::name_valid(name.as_bytes()))?;
            let product = hex(product, 4)
                .and_then(|value| u16::try_from(value).ok())
                .ok_or(Error::Invalid("edid-product"))?;
            Monitor::Generated {
                maker,
                product,
                name,
            }
        }
        _ => return Err(Error::Edid),
    };
    Ok(Brand {
        id,
        provider,
        device,
        control,
        container,
        monitor,
        input,
        input_control,
        input_container,
    })
}

impl Brand<'_> {
    /// What of another brand's identity this one shares, if anything: an
    /// identifier the system or the controller knows a driver by, which two
    /// drivers on one machine must not have in common. A container that
    /// differs only in the bytes its devices replace -- the display's last,
    /// the input's last two -- is the same.
    pub(crate) fn shared_with(&self, other: &Brand<'_>) -> Option<&'static str> {
        let display = |guid: Guid| {
            let [a, b, c, d, e, f, g, _] = guid.data4;
            (guid.data1, guid.data2, guid.data3, [a, b, c, d, e, f, g, 0])
        };
        let input = |guid: Guid| {
            let [a, b, c, d, e, f, _, _] = guid.data4;
            (guid.data1, guid.data2, guid.data3, [a, b, c, d, e, f, 0, 0])
        };
        if self.id == other.id {
            Some("id")
        } else if self.control == other.control {
            Some("control interface")
        } else if display(self.container) == display(other.container) {
            Some("container")
        } else if self.input_control == other.input_control {
            Some("input control interface")
        } else if input(self.input_container) == input(other.input_container) {
            Some("input container")
        } else {
            None
        }
    }
}

/// A short name: lower-case letters and digits, a letter first, at most
/// [`MAX_ID`].
fn id_valid(id: &str) -> bool {
    id.len() <= MAX_ID
        && id.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// A name a person sees, as the setup information quotes it: printable
/// ASCII, at most [`MAX_NAME`], without the quote and the percent sign the
/// setup information would have to double, the backslash, or the `@` its
/// placeholders are marked with.
fn name_valid(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|c| (0x20..=0x7e).contains(&c) && !matches!(c, b'"' | b'%' | b'\\' | b'@'))
}

/// Hexadecimal digits alone, one to `most` of them.
fn hex(text: &str, most: usize) -> Option<u64> {
    if !(1..=most).contains(&text.len()) || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

fn guid(text: &str) -> Option<Guid> {
    let text = text
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))
        .unwrap_or(text);
    let mut groups = text.split('-');
    let mut group = |len: usize| {
        let group = groups.next()?;
        (group.len() == len).then(|| hex(group, len)).flatten()
    };
    let (data1, data2, data3, high, low) = (group(8)?, group(4)?, group(4)?, group(4)?, group(12)?);
    if groups.next().is_some() {
        return None;
    }
    Some(Guid {
        data1: u32::try_from(data1).ok()?,
        data2: u16::try_from(data2).ok()?,
        data3: u16::try_from(data3).ok()?,
        data4: ((high << 48) | low).to_be_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;

    use super::*;

    /// lowlat's own, the file the build takes when it is given none.
    const OWN: &str = include_str!("../lowlat.brand");

    const ACME: &str = "\
# A company's.
id = acme2
provider = Acme Corp.
device = Acme Display Adapter (virtual)

control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63
container = {E2A4C6D8-1B3D-4F5A-8C7E-9A0B2C4D6E00}
input = Acme Input Devices
input-control = {0b6c9a1e-7d42-4c3a-9f18-2e5d7a6b4c11}
input-container = 9e8d7c6b-5a49-4382-a1b0-c9d8e7f60000
edid = monitors/acme.edid
";

    #[test]
    fn lowlats_own_file_reads_as_its_identity() {
        let own = read(OWN).expect("lowlat's own");
        assert_eq!(own.id, "lowlat");
        assert_eq!(own.provider, "Lowlat");
        assert_eq!(own.device, "Lowlat Virtual Display");
        assert_eq!(
            own.control,
            Guid {
                data1: 0x4a91_4f53,
                data2: 0x543f,
                data3: 0x46f9,
                data4: [0xb9, 0x9b, 0xd3, 0x49, 0xa5, 0xaa, 0xf3, 0xe5],
            }
        );
        assert_eq!(
            own.container,
            Guid {
                data1: 0x3bb8_0aa8,
                data2: 0x3343,
                data3: 0x4e3d,
                data4: [0x99, 0xd4, 0xcc, 0xc1, 0x66, 0xf4, 0x62, 0x00],
            }
        );
        assert_eq!(
            own.monitor,
            Monitor::Generated {
                maker: "LWL",
                product: 1,
                name: "LowlatDisplay"
            }
        );
        assert_eq!(own.input, "Lowlat Virtual Input");
        assert_eq!(
            own.input_control,
            Guid {
                data1: 0x32e3_9bbb,
                data2: 0xdbe3,
                data3: 0x4fb6,
                data4: [0xab, 0x8c, 0x78, 0x3e, 0x5b, 0x21, 0x4f, 0xed],
            }
        );
        assert_eq!(
            own.input_container,
            Guid {
                data1: 0x5234_c8de,
                data2: 0xc3e9,
                data3: 0x4480,
                data4: [0x8d, 0x9c, 0xf7, 0x55, 0x4c, 0x66, 0x00, 0x00],
            }
        );
    }

    #[test]
    fn a_companys_file_reads_with_a_dumped_edid_and_shares_nothing_with_lowlats() {
        let acme = read(ACME).expect("a company's");
        assert_eq!(acme.id, "acme2");
        assert_eq!(acme.provider, "Acme Corp.");
        assert_eq!(acme.device, "Acme Display Adapter (virtual)");
        assert_eq!(acme.control.data1, 0x6f1c_2b7e);
        assert_eq!(
            acme.control.data4,
            [0x9d, 0x2a, 0x7c, 0x4b, 0x8e, 0x1f, 0x0a, 0x63]
        );
        assert_eq!(acme.container.data2, 0x1b3d, "upper-case digits, braces");
        assert_eq!(acme.monitor, Monitor::Dumped("monitors/acme.edid"));
        assert_eq!(acme.input, "Acme Input Devices");
        assert_eq!(acme.input_control.data1, 0x0b6c_9a1e);
        assert_eq!(
            acme.input_container.data4[..6],
            [0xa1, 0xb0, 0xc9, 0xd8, 0xe7, 0xf6]
        );
        let own = read(OWN).expect("lowlat's own");
        assert_eq!(acme.shared_with(&own), None);
        assert_eq!(own.shared_with(&own), Some("id"));
    }

    /// **A brand that copies an identifier of another is refused that
    /// identifier**: the id, the control interface, or the container -- the
    /// last even where only its slot byte differs.
    #[test]
    fn a_shared_identifier_is_named() {
        let own = read(OWN).expect("lowlat's own");
        let copy = |from: &str, to: &str| -> String { ACME.replace(from, to) };
        let lowlat_id = copy("id = acme2", "id = lowlat");
        assert_eq!(
            read(&lowlat_id).expect("read").shared_with(&own),
            Some("id")
        );
        let control = copy(
            "6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
            "{4a914f53-543f-46f9-b99b-d349a5aaf3e5}",
        );
        assert_eq!(
            read(&control).expect("read").shared_with(&own),
            Some("control interface")
        );
        let container = copy(
            "{E2A4C6D8-1B3D-4F5A-8C7E-9A0B2C4D6E00}",
            "{3bb80aa8-3343-4e3d-99d4-ccc166f462ff}",
        );
        assert_eq!(
            read(&container).expect("read").shared_with(&own),
            Some("container")
        );
        let input_control = copy(
            "{0b6c9a1e-7d42-4c3a-9f18-2e5d7a6b4c11}",
            "{32e39bbb-dbe3-4fb6-ab8c-783e5b214fed}",
        );
        assert_eq!(
            read(&input_control).expect("read").shared_with(&own),
            Some("input control interface")
        );
        let input_container = copy(
            "9e8d7c6b-5a49-4382-a1b0-c9d8e7f60000",
            "5234c8de-c3e9-4480-8d9c-f7554c6604ff",
        );
        assert_eq!(
            read(&input_container).expect("read").shared_with(&own),
            Some("input container"),
            "only the kind and slot bytes differ"
        );
    }

    /// **A brand's four GUIDs are four**: one given for two keys is refused
    /// at the second, whichever two they are.
    #[test]
    fn a_guid_given_twice_in_one_brand_is_refused() {
        let control = "6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63";
        let twice = ACME.replace("{0b6c9a1e-7d42-4c3a-9f18-2e5d7a6b4c11}", control);
        assert_eq!(
            read(&twice).map(|_| ()),
            Err(Error::Shared("input-control"))
        );
        let container = ACME.replace(
            "9e8d7c6b-5a49-4382-a1b0-c9d8e7f60000",
            "{E2A4C6D8-1B3D-4F5A-8C7E-9A0B2C4D6E00}",
        );
        assert_eq!(
            read(&container).map(|_| ()),
            Err(Error::Shared("input-container"))
        );
        let crossed = ACME.replace("{E2A4C6D8-1B3D-4F5A-8C7E-9A0B2C4D6E00}", control);
        assert_eq!(read(&crossed).map(|_| ()), Err(Error::Shared("container")));
    }

    #[test]
    fn a_generated_edid_takes_its_three_settings() {
        let text = ACME.replace(
            "edid = monitors/acme.edid",
            "edid-maker = ACM\nedid-product = 0A1\nedid-name = Acme",
        );
        assert_eq!(
            read(&text).expect("read").monitor,
            Monitor::Generated {
                maker: "ACM",
                product: 0x0a1,
                name: "Acme"
            }
        );
    }

    /// **Every way out of form is refused, and says which key**: each check
    /// the file has, broken once.
    #[test]
    fn a_value_out_of_its_form_is_refused_by_key() {
        let cases: [(&str, &str, Error<'_>); 25] = [
            (
                "input = Acme Input Devices",
                "input = Acme \"Input\"",
                Error::Invalid("input"),
            ),
            (
                "input-control = {0b6c9a1e-7d42-4c3a-9f18-2e5d7a6b4c11}",
                "input-control = {0b6c9a1e-7d42-4c3a-9f18}",
                Error::Invalid("input-control"),
            ),
            (
                "input-container = 9e8d7c6b-5a49-4382-a1b0-c9d8e7f60000",
                "input-container = 9e8d7c6b-5a49-4382-a1b0-c9d8e7f6000g",
                Error::Invalid("input-container"),
            ),
            ("id = acme2", "id = Acme", Error::Invalid("id")),
            ("id = acme2", "id = 2acme", Error::Invalid("id")),
            ("id = acme2", "id = acme-corp", Error::Invalid("id")),
            ("id = acme2", "id =", Error::Invalid("id")),
            (
                "id = acme2",
                "id = abcdefghijklmnopqrstuvwxyzabcdefg",
                Error::Invalid("id"),
            ),
            (
                "provider = Acme Corp.",
                "provider = Acme \"Corp\"",
                Error::Invalid("provider"),
            ),
            (
                "provider = Acme Corp.",
                "provider = 100% Acme",
                Error::Invalid("provider"),
            ),
            (
                "provider = Acme Corp.",
                "provider = Acme\\Corp",
                Error::Invalid("provider"),
            ),
            (
                "provider = Acme Corp.",
                "provider = acme@corp",
                Error::Invalid("provider"),
            ),
            (
                "provider = Acme Corp.",
                "provider = Acm\u{e9}",
                Error::Invalid("provider"),
            ),
            (
                "provider = Acme Corp.",
                "provider = ",
                Error::Invalid("provider"),
            ),
            (
                "device = Acme Display Adapter (virtual)",
                "device = Acme\tDisplay",
                Error::Invalid("device"),
            ),
            (
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a6",
                Error::Invalid("control"),
            ),
            (
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63-00",
                Error::Invalid("control"),
            ),
            (
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
                "control = 6f1c2b7e-+a51-4f0e-9d2a-7c4b8e1f0a63",
                Error::Invalid("control"),
            ),
            (
                "control = 6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
                "control = {6f1c2b7e-3a51-4f0e-9d2a-7c4b8e1f0a63",
                Error::Invalid("control"),
            ),
            (
                "container = {E2A4C6D8-1B3D-4F5A-8C7E-9A0B2C4D6E00}",
                "container = E2A4C6D81B3D4F5A8C7E9A0B2C4D6E00",
                Error::Invalid("container"),
            ),
            (
                "edid = monitors/acme.edid",
                "edid =",
                Error::Invalid("edid"),
            ),
            (
                "edid = monitors/acme.edid",
                "edid-maker = AC\nedid-product = 1\nedid-name = Acme",
                Error::Invalid("edid-maker"),
            ),
            (
                "edid = monitors/acme.edid",
                "edid-maker = ACM\nedid-product = 10000\nedid-name = Acme",
                Error::Invalid("edid-product"),
            ),
            (
                "edid = monitors/acme.edid",
                "edid-maker = ACM\nedid-product = 0x1\nedid-name = Acme",
                Error::Invalid("edid-product"),
            ),
            (
                "edid = monitors/acme.edid",
                "edid-maker = ACM\nedid-product = 1\nedid-name = Acme Display 14",
                Error::Invalid("edid-name"),
            ),
        ];
        for (from, to, error) in cases {
            assert!(ACME.contains(from), "{from:?} is in the file");
            let text = ACME.replace(from, to);
            assert_eq!(read(&text).map(|_| ()), Err(error), "{to:?}");
        }
    }

    #[test]
    fn a_file_out_of_its_shape_is_refused() {
        let missing = ACME.replace("device = Acme Display Adapter (virtual)\n", "");
        assert_eq!(read(&missing).map(|_| ()), Err(Error::Missing("device")));
        let no_input = ACME.replace("input = Acme Input Devices\n", "");
        assert_eq!(read(&no_input).map(|_| ()), Err(Error::Missing("input")));
        let twice = format!("{ACME}id = acme3\n");
        assert_eq!(read(&twice).map(|_| ()), Err(Error::Twice("id")));
        let unknown = format!("{ACME}colour = blue\n");
        assert_eq!(read(&unknown).map(|_| ()), Err(Error::Unknown("colour")));
        let line = format!("{ACME}\njust words\n");
        assert_eq!(read(&line).map(|_| ()), Err(Error::Line(13)));
        let both = format!("{ACME}edid-maker = ACM\nedid-product = 1\nedid-name = Acme\n");
        assert_eq!(read(&both).map(|_| ()), Err(Error::Edid));
        let neither = ACME.replace("edid = monitors/acme.edid\n", "");
        assert_eq!(read(&neither).map(|_| ()), Err(Error::Edid));
        let part = ACME.replace(
            "edid = monitors/acme.edid",
            "edid-maker = ACM\nedid-name = Acme",
        );
        assert_eq!(read(&part).map(|_| ()), Err(Error::Edid), "no product");
        let crlf = ACME.replace('\n', "\r\n");
        assert_eq!(read(&crlf).map(|brand| brand.id), Ok("acme2"), "CRLF lines");
    }
}
