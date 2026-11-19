//! The virtual displays from a shell (docs/11-drivers.md section 1.9): the
//! commands `lowlat-vdd` and `lowlatd display` take after their names. A
//! person's own displays are kept -- a command's process ends with it -- and
//! `hold` keeps a bound one for as long as it runs.

use std::io::BufRead;
use std::time::Duration;

use crate::{Controller, Error, GpuChoice, Lifetime, Mode, Monitor, Pointer, Settings};

/// The commands, each after the program's name.
const COMMANDS: [&str; 9] = [
    "list",
    "add WxH[@Hz] [--restore] [--slot N]",
    "hold WxH[@Hz] [--slot N] [--for SECONDS]",
    "remove SLOT",
    "mode SLOT WxH[@Hz]",
    "modes [add|remove WxH@Hz]",
    "gpu [auto|igpu|nvidia|amd|intel|VVVV|VVVV:DDDD]",
    "pointer [hardware|drawn]",
    "edid [FILE|none]",
];

/// The makers a person may name, by the number the file keeps.
const MAKERS: [(&str, u16); 3] = [("nvidia", 0x10de), ("amd", 0x1002), ("intel", 0x8086)];

/// A GPU as a person names it: a maker by name, or anything the file itself
/// reads -- `auto`, `igpu`, a maker's number, a GPU's maker's and own number
/// -- read by the file's own reader, so the two can never disagree. `None`
/// for what neither knows.
fn gpu_named(text: &str) -> Option<Option<GpuChoice>> {
    if let Some(&(_, maker)) = MAKERS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(text))
    {
        return Some(Some(GpuChoice::Maker(maker)));
    }
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let (settings, skipped) = Settings::parse(format!("gpu {text}").as_bytes());
    (skipped == 0).then_some(settings.gpu)
}

/// Run a command line -- what follows the program's name -- and answer the
/// process's outcome: 0 done, 1 refused, 2 a command line not understood.
/// `program` is what a person typed before it, named in the usage and in a
/// refusal.
pub fn run(program: &str, args: &[String]) -> i32 {
    match command(args) {
        Ok(()) => 0,
        Err(Fail::Usage) => {
            eprintln!("usage:");
            for line in COMMANDS {
                eprintln!("  {program} {line}");
            }
            2
        }
        Err(Fail::Refused(why)) => {
            eprintln!("{program}: {why}");
            1
        }
    }
}

enum Fail {
    Usage,
    Refused(String),
}

fn refused(error: Error) -> Fail {
    Fail::Refused(match error {
        Error::Absent => "the display driver is not installed or not started".into(),
        Error::Version(v) => format!("the display driver speaks version {v}, not this one's"),
        Error::Denied => "the display driver refused this account".into(),
        Error::Starting => "the display driver is still starting".into(),
        Error::Full => "every slot is taken".into(),
        Error::SlotTaken => "that slot is taken".into(),
        Error::NotOwner => "that display belongs to another program".into(),
        Error::NoMonitor => "no display at that slot".into(),
        Error::Mode => "no display can be that mode".into(),
        Error::Argument => "those options contradict each other".into(),
        Error::Busy => "that display is being plugged again; try again".into(),
        Error::Display(code) => format!("the system's display configuration refused ({code})"),
        Error::NotReady(slot) => {
            format!("slot {slot} is plugged but the system has not finished; `list` shows it")
        }
        Error::File => {
            "the settings file could not be read or written (is the driver installed?)".into()
        }
        Error::Io(code) => format!("the display driver answered error {code}"),
    })
}

/// `WxH` or `WxH@Hz`, 60 when no rate is given.
fn mode(text: &str) -> Result<Mode, Fail> {
    let full = if text.contains('@') {
        text.to_string()
    } else {
        format!("{text}@60")
    };
    let mode = Mode::parse(&full).ok_or(Fail::Usage)?;
    if !mode.is_valid() {
        return Err(Fail::Refused(format!(
            "{mode} is outside 640x480 to 7680x4320 at 24 to 480 Hz"
        )));
    }
    Ok(mode)
}

fn slot(text: &str) -> Result<u8, Fail> {
    text.parse::<u8>()
        .ok()
        .filter(|slot| usize::from(*slot) < crate::MAX_MONITORS)
        .ok_or(Fail::Usage)
}

/// The value after `--name`, and whether the flag was there.
fn option<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>, Fail> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(at) => args
            .get(at + 1)
            .map(|value| Some(value.as_str()))
            .ok_or(Fail::Usage),
    }
}

fn show(monitor: &Monitor) {
    let lifetime = match (monitor.kept, monitor.restore, monitor.owned) {
        (true, true, _) => "kept, restored",
        (true, false, _) => "kept",
        (false, _, true) => "bound to this command",
        (false, _, false) => "bound to another program",
    };
    if monitor.active {
        let turned = if monitor.turned { " turned" } else { "" };
        println!(
            "slot {}: {}{turned} at {},{} {} ({lifetime})",
            monitor.slot, monitor.mode, monitor.x, monitor.y, monitor.name
        );
    } else {
        println!(
            "slot {}: {} not shown ({lifetime})",
            monitor.slot, monitor.mode
        );
    }
}

fn command(args: &[String]) -> Result<(), Fail> {
    let verb = args.first().map(String::as_str).ok_or(Fail::Usage)?;
    let rest = args.get(1..).unwrap_or(&[]);
    match verb {
        "list" => {
            let controller = Controller::open().map_err(refused)?;
            let monitors = controller.list().map_err(refused)?;
            if monitors.is_empty() {
                println!("no virtual displays");
            }
            monitors.iter().for_each(show);
            Ok(())
        }
        "add" => {
            let asked = mode(rest.first().ok_or(Fail::Usage)?)?;
            let restore = rest.iter().any(|a| a == "--restore");
            let at = option(rest, "--slot")?.map(slot).transpose()?;
            let mut controller = Controller::open().map_err(refused)?;
            let monitor = controller
                .plug(asked, Lifetime::Kept, restore, at)
                .map_err(refused)?;
            show(&monitor);
            Ok(())
        }
        "hold" => {
            let asked = mode(rest.first().ok_or(Fail::Usage)?)?;
            let at = option(rest, "--slot")?.map(slot).transpose()?;
            let seconds = option(rest, "--for")?
                .map(|s| s.parse::<u64>().map_err(|_| Fail::Usage))
                .transpose()?;
            let mut controller = Controller::open().map_err(refused)?;
            let monitor = controller
                .plug(asked, Lifetime::Bound, false, at)
                .map_err(refused)?;
            show(&monitor);
            match seconds {
                Some(seconds) => std::thread::sleep(Duration::from_secs(seconds)),
                None => {
                    println!("held; press Enter to unplug");
                    let _ = std::io::stdin().lock().lines().next();
                }
            }
            // Closing the handle unplugs it; said here so a person sees it.
            drop(controller);
            println!("slot {}: unplugged", monitor.slot);
            Ok(())
        }
        "remove" => {
            let at = slot(rest.first().ok_or(Fail::Usage)?)?;
            let mut controller = Controller::open().map_err(refused)?;
            controller.unplug(at).map_err(refused)?;
            println!("slot {at}: removed");
            Ok(())
        }
        "mode" => {
            let at = slot(rest.first().ok_or(Fail::Usage)?)?;
            let asked = mode(rest.get(1).ok_or(Fail::Usage)?)?;
            let mut controller = Controller::open().map_err(refused)?;
            let monitor = controller.set_mode(at, asked).map_err(refused)?;
            show(&monitor);
            Ok(())
        }
        "modes" => match rest.first().map(String::as_str) {
            None => {
                let settings = crate::settings().map_err(refused)?;
                if settings.modes().is_empty() {
                    println!("no custom modes");
                }
                for mode in settings.modes() {
                    println!("{mode}");
                }
                Ok(())
            }
            Some(change @ ("add" | "remove")) => {
                let asked = mode(rest.get(1).ok_or(Fail::Usage)?)?;
                let mut controller = Controller::open().map_err(refused)?;
                let mut done = false;
                controller
                    .change_settings(|settings| {
                        done = if change == "add" {
                            settings.add_mode(asked)
                        } else {
                            settings.remove_mode(asked)
                        };
                    })
                    .map_err(refused)?;
                if !done {
                    return Err(Fail::Refused(if change == "add" {
                        "the file holds as many custom modes as it takes".into()
                    } else {
                        format!("{asked} is not a custom mode")
                    }));
                }
                if change == "add" {
                    println!("added {asked}; displays plugged from now list it");
                } else {
                    println!("removed {asked}; displays plugged from now leave it out");
                }
                Ok(())
            }
            Some(_) => Err(Fail::Usage),
        },
        "gpu" => match rest.first().map(String::as_str) {
            None => {
                let settings = crate::settings().map_err(refused)?;
                match settings.gpu {
                    None => println!("gpu auto (the high-performance one)"),
                    Some(choice @ GpuChoice::Exact(_)) => println!("gpu {choice}"),
                    Some(choice @ GpuChoice::Maker(maker)) => println!(
                        "gpu {choice} ({}, the first of that maker's)",
                        MAKERS
                            .iter()
                            .find(|&&(_, number)| number == maker)
                            .map_or("a maker", |&(name, _)| name)
                    ),
                    Some(GpuChoice::PowerSaving) => {
                        println!("gpu igpu (the first in the power-saving order)");
                    }
                }
                for info in crate::gpus() {
                    println!(
                        "  {:04x}:{:04x} {}",
                        info.gpu.vendor, info.gpu.device, info.name
                    );
                }
                Ok(())
            }
            Some(choice) => {
                let gpu = gpu_named(choice).ok_or(Fail::Usage)?;
                let mut controller = Controller::open().map_err(refused)?;
                controller
                    .change_settings(|settings| settings.gpu = gpu)
                    .map_err(refused)?;
                println!("gpu {choice}; the displays move to it now");
                Ok(())
            }
        },
        "pointer" => match rest.first().map(String::as_str) {
            None => {
                let settings = crate::settings().map_err(refused)?;
                println!(
                    "pointer {}",
                    if settings.pointer == Pointer::Drawn {
                        "drawn"
                    } else {
                        "hardware"
                    }
                );
                Ok(())
            }
            Some(choice @ ("hardware" | "drawn")) => {
                let pointer = if choice == "drawn" {
                    Pointer::Drawn
                } else {
                    Pointer::Hardware
                };
                let mut controller = Controller::open().map_err(refused)?;
                controller
                    .change_settings(|settings| settings.pointer = pointer)
                    .map_err(refused)?;
                println!("pointer {choice}; displays take it at their next swapchain");
                Ok(())
            }
            Some(_) => Err(Fail::Usage),
        },
        "edid" => match rest.first().map(String::as_str) {
            None => {
                let settings = crate::settings().map_err(refused)?;
                println!(
                    "edid {}",
                    if settings.edid.is_some() {
                        "dumped (from a file)"
                    } else {
                        "the driver's own"
                    }
                );
                Ok(())
            }
            Some(choice) => {
                let edid = if choice == "none" {
                    None
                } else {
                    let bytes = std::fs::read(choice)
                        .map_err(|e| Fail::Refused(format!("{choice}: {e}")))?;
                    Some(crate::Edid::parse(&bytes).ok_or_else(|| {
                        Fail::Refused(format!(
                            "{choice} is not an EDID of one or two valid blocks"
                        ))
                    })?)
                };
                let mut controller = Controller::open().map_err(refused)?;
                controller
                    .change_settings(|settings| settings.edid = edid)
                    .map_err(refused)?;
                println!(
                    "edid {choice}; displays plugged from now carry it, each a new monitor to the system"
                );
                Ok(())
            }
        },
        _ => Err(Fail::Usage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// **A command line not understood answers 2, and a mode no display can
    /// be answers 1**, each while the line is read, before the driver is
    /// opened.
    #[test]
    fn a_line_not_understood_answers_two_and_a_mode_no_display_can_be_one() {
        for bad in [
            &[][..],
            &["frobnicate"],
            &["add"],
            &["add", "1920x1080@sixty"],
            &["remove", "16"],
            &["mode", "0"],
            &["hold", "1920x1080", "--for", "soon"],
            &["modes", "swap", "1920x1080@60"],
            &["gpu", "geforce"],
            &["gpu", "10de:"],
            &["pointer", "both"],
        ] {
            assert_eq!(run("lowlat-vdd", &line(bad)), 2, "{bad:?}");
        }
        for impossible in [&["add", "320x200@60"][..], &["mode", "3", "1920x1080@1000"]] {
            assert_eq!(run("lowlat-vdd", &line(impossible)), 1, "{impossible:?}");
        }
    }

    /// **A GPU is named as the file names it, or by its maker's name**,
    /// which the file keeps as the maker's number.
    #[test]
    fn a_gpu_is_named_by_the_files_words_or_by_its_maker() {
        use crate::Gpu;
        assert_eq!(gpu_named("auto"), Some(None));
        assert_eq!(gpu_named("igpu"), Some(Some(GpuChoice::PowerSaving)));
        assert_eq!(gpu_named("NVIDIA"), Some(Some(GpuChoice::Maker(0x10de))));
        assert_eq!(gpu_named("amd"), Some(Some(GpuChoice::Maker(0x1002))));
        assert_eq!(gpu_named("intel"), Some(Some(GpuChoice::Maker(0x8086))));
        assert_eq!(gpu_named("8086"), Some(Some(GpuChoice::Maker(0x8086))));
        assert_eq!(
            gpu_named("10de:2d05"),
            Some(Some(GpuChoice::Exact(Gpu {
                vendor: 0x10de,
                device: 0x2d05
            })))
        );
        for bad in [
            "",
            "geforce",
            "10de:",
            "10de 2d05",
            "igpus",
            "auto\ngpu igpu",
            "12345",
        ] {
            assert_eq!(gpu_named(bad), None, "{bad:?}");
        }
    }
}
