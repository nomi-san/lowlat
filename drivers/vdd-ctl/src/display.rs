//! The system's display configuration, as it concerns one of the driver's
//! monitors: whether it is shown, where and at what mode, making it active,
//! and setting its mode.
//!
//! **A monitor is found by its adapter and target**, the pair the driver's
//! plug answers with, never by its `\\.\DISPLAYn` name, which the system
//! hands out afresh, nor by its EDID.

use lowlat_drivers::d3d11::{D3d11, Luid};
use lowlat_vdd_proto::modes::Mode;
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_ROTATION_IDENTITY, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_SOURCE_MODE,
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ALL_PATHS, QDC_DATABASE_CURRENT, QDC_ONLY_ACTIVE_PATHS, QUERY_DISPLAY_CONFIG_FLAGS,
    QueryDisplayConfig, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG, SetDisplayConfig,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LUID};
use windows_sys::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DMDO_90, DMDO_270, DMDO_DEFAULT, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};

/// One of the driver's monitors as the system shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Shown {
    /// The system's name for the display, `\\.\DISPLAYn`.
    pub(crate) name: String,
    /// The display's top left on the desktop.
    pub(crate) x: i32,
    pub(crate) y: i32,
    /// **The monitor's own mode, turned or not**: the configuration gives a
    /// turned display's source at the sides the monitor lists and puts the
    /// turn on its path, where the desktop's pixels have them swapped.
    pub(crate) mode: Mode,
    /// Turned on the desktop, by its display settings or a program.
    pub(crate) turned: bool,
}

/// The configuration, as asked.
struct Config {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Option<Config> {
    // The configuration can grow between the two calls; ask again when it has.
    for _ in 0..4 {
        let (mut paths_len, mut modes_len) = (0u32, 0u32);
        // SAFETY: live outputs.
        if unsafe { GetDisplayConfigBufferSizes(flags, &raw mut paths_len, &raw mut modes_len) }
            != 0
        {
            return None;
        }
        // SAFETY: plain data, for which zero is a value.
        let mut paths = vec![unsafe { core::mem::zeroed() }; usize::try_from(paths_len).ok()?];
        // SAFETY: as above.
        let mut modes = vec![unsafe { core::mem::zeroed() }; usize::try_from(modes_len).ok()?];
        // **The store's configuration comes with the topology it is kept
        // under**, which that query alone must be given a place for.
        let mut topology = 0;
        let topology_at = if flags == QDC_DATABASE_CURRENT {
            &raw mut topology
        } else {
            core::ptr::null_mut()
        };
        // SAFETY: buffers of the counts given; a topology asked for only with
        // the store's configuration.
        let status = unsafe {
            QueryDisplayConfig(
                flags,
                &raw mut paths_len,
                paths.as_mut_ptr(),
                &raw mut modes_len,
                modes.as_mut_ptr(),
                topology_at,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status != 0 {
            return None;
        }
        paths.truncate(usize::try_from(paths_len).ok()?);
        modes.truncate(usize::try_from(modes_len).ok()?);
        return Some(Config { paths, modes });
    }
    None
}

fn value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | (u64::from(u32::from_ne_bytes(luid.HighPart.to_ne_bytes())) << 32)
}

fn targets(path: &DISPLAYCONFIG_PATH_INFO, adapter: u64, target: u32) -> bool {
    value(path.targetInfo.adapterId) == adapter && path.targetInfo.id == target
}

fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> String {
    // SAFETY: plain data, for which zero is a value.
    let mut name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { core::mem::zeroed() };
    name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    name.header.size = u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).unwrap_or(0);
    name.header.adapterId = path.sourceInfo.adapterId;
    name.header.id = path.sourceInfo.id;
    // SAFETY: a request of the size it says.
    if unsafe { DisplayConfigGetDeviceInfo(&raw mut name.header) } != 0 {
        return String::new();
    }
    let end = name
        .viewGdiDeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name.viewGdiDeviceName.len());
    String::from_utf16_lossy(name.viewGdiDeviceName.get(..end).unwrap_or(&[]))
}

/// The active path to `adapter`'s `target` in `config`, and the mode it shows
/// the monitor at: its source, and the refresh to the nearest whole.
fn showing(
    config: &Config,
    adapter: u64,
    target: u32,
) -> Option<(&DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_MODE, u32)> {
    let path = config
        .paths
        .iter()
        .find(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 && targets(p, adapter, target))?;
    // SAFETY: the plain index, as the configuration was asked without the
    // virtual-mode layout.
    let source = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    let mode = config
        .modes
        .get(usize::try_from(source).ok()?)
        .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)?;
    // SAFETY: a source mode, as its type says.
    let source = unsafe { mode.Anonymous.sourceMode };
    let rate = path.targetInfo.refreshRate;
    let refresh_hz = rate
        .Numerator
        .saturating_add(rate.Denominator / 2)
        .checked_div(rate.Denominator)
        .unwrap_or(0);
    Some((path, source, refresh_hz))
}

/// The monitor at `adapter`'s `target`, when the system shows it.
pub(crate) fn shown(adapter: u64, target: u32) -> Option<Shown> {
    let config = query(QDC_ONLY_ACTIVE_PATHS)?;
    let (path, source, refresh_hz) = showing(&config, adapter, target)?;
    Some(Shown {
        name: source_name(path),
        x: source.position.x,
        y: source.position.y,
        mode: Mode::new(source.width, source.height, refresh_hz),
        turned: path.targetInfo.rotation != DISPLAYCONFIG_ROTATION_IDENTITY,
    })
}

/// The mode the system's store holds for the monitor at `adapter`'s `target`
/// among the monitors connected now -- what the system puts back on it as
/// another monitor arrives or a program leaves exclusive fullscreen; none
/// where the store shows it in no arrangement of these monitors.
pub(crate) fn saved(adapter: u64, target: u32) -> Option<Mode> {
    let config = query(QDC_DATABASE_CURRENT)?;
    let (_, source, refresh_hz) = showing(&config, adapter, target)?;
    Some(Mode::new(source.width, source.height, refresh_hz))
}

/// An active path, as the save decision reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lit {
    adapter: u64,
    technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    /// The kernel's word that the path's adapter is a virtual display's;
    /// none where it does not say.
    indirect: Option<bool>,
}

/// Whether `path` is a display of another adapter than `ours` and not a
/// virtual one. **A virtual display reports whatever technology its driver
/// chose** -- DisplayPort, HDMI -- so the kernel's word on its adapter counts
/// first, and an indirect technology as well.
fn real_beside(path: Lit, ours: u64) -> bool {
    path.adapter != ours
        && path.indirect != Some(true)
        && path.technology != DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED
        && path.technology != DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL
}

/// The active paths, each with the kernel's word on its adapter.
fn lit() -> Vec<Lit> {
    let Some(config) = query(QDC_ONLY_ACTIVE_PATHS) else {
        return Vec::new();
    };
    let kernel = D3d11::load().ok();
    config
        .paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .map(|p| Lit {
            adapter: value(p.targetInfo.adapterId),
            technology: p.targetInfo.outputTechnology,
            indirect: kernel.as_ref().and_then(|kernel| {
                kernel.indirect(Luid {
                    high: p.targetInfo.adapterId.HighPart,
                    low: p.targetInfo.adapterId.LowPart,
                })
            }),
        })
        .collect()
}

/// Whether a display of another adapter, and not a virtual one, is active:
/// what makes an arrangement safe to save (a saved one that only virtual
/// displays satisfy holds a later boot waiting for their driver).
pub(crate) fn others_active(adapter: u64) -> bool {
    lit().into_iter().any(|path| real_beside(path, adapter))
}

/// Make the monitor active beside what is: the active paths and one more,
/// from a source of its adapter no active path uses, the system choosing the
/// modes and the place. Saved to the system's store only when asked.
pub(crate) fn activate(adapter: u64, target: u32, save: bool) -> Result<(), u32> {
    let active = query(QDC_ONLY_ACTIVE_PATHS).ok_or(1u32)?;
    let all = query(QDC_ALL_PATHS).ok_or(1u32)?;
    let used = |source: u32| {
        active
            .paths
            .iter()
            .any(|p| value(p.sourceInfo.adapterId) == adapter && p.sourceInfo.id == source)
    };
    let mut path = *all
        .paths
        .iter()
        .filter(|p| targets(p, adapter, target) && value(p.sourceInfo.adapterId) == adapter)
        .filter(|p| !used(p.sourceInfo.id))
        .min_by_key(|p| p.sourceInfo.id)
        .ok_or(1u32)?;
    path.flags = DISPLAYCONFIG_PATH_ACTIVE;
    path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    let mut paths = active.paths;
    paths.push(path);
    let mut flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
    if save {
        flags |= SDC_SAVE_TO_DATABASE;
    }
    // SAFETY: the paths and modes of one query, and the path added from
    // another of the same moment.
    let status = unsafe {
        SetDisplayConfig(
            u32::try_from(paths.len()).unwrap_or(0),
            paths.as_ptr(),
            u32::try_from(active.modes.len()).unwrap_or(0),
            active.modes.as_ptr(),
            flags,
        )
    };
    if status != 0 {
        return Err(u32::try_from(status).unwrap_or(1));
    }
    Ok(())
}

/// The system's full name for a display, `\\.\DISPLAYn`, from that or from the
/// bare `DISPLAYn` a host lists its outputs by: the system's interfaces take
/// the full one alone.
pub(crate) fn gdi_name(name: &str) -> String {
    if name.starts_with(r"\\.\") {
        name.to_string()
    } else {
        format!(r"\\.\{name}")
    }
}

/// The adapter and target of the active path whose source the system names
/// `name`, with its prefix or without: how any display of the desktop is
/// found for a turn.
pub(crate) fn named(name: &str) -> Option<(u64, u32)> {
    let name = gdi_name(name);
    let config = query(QDC_ONLY_ACTIVE_PATHS)?;
    config
        .paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .find(|p| source_name(p).eq_ignore_ascii_case(&name))
        .map(|p| (value(p.targetInfo.adapterId), p.targetInfo.id))
}

/// Turn the monitor at `adapter`'s `target` to `rotation`: its active path's
/// rotation changed and every mode supplied as it is -- the source keeps the
/// monitor's own sides -- saved to the system's store only when asked.
pub(crate) fn set_turn(adapter: u64, target: u32, rotation: i32, save: bool) -> Result<(), u32> {
    let mut config = query(QDC_ONLY_ACTIVE_PATHS).ok_or(1u32)?;
    let path = config
        .paths
        .iter_mut()
        .find(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 && targets(p, adapter, target))
        .ok_or(1u32)?;
    path.targetInfo.rotation = rotation;
    let mut flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
    if save {
        flags |= SDC_SAVE_TO_DATABASE;
    }
    // SAFETY: the paths and modes of one query, one path's rotation changed.
    let status = unsafe {
        SetDisplayConfig(
            u32::try_from(config.paths.len()).unwrap_or(0),
            config.paths.as_ptr(),
            u32::try_from(config.modes.len()).unwrap_or(0),
            config.modes.as_ptr(),
            flags,
        )
    };
    if status != 0 {
        return Err(u32::try_from(status).unwrap_or(1));
    }
    Ok(())
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(core::iter::once(0)).collect()
}

/// A mode's sides as the desktop's own interface speaks of them: the
/// monitor's own, swapped while the display is turned a quarter -- its list
/// is, and a mode put with the monitor's own sides is refused.
fn desktop_sides(mode: Mode, orientation: u32) -> (u32, u32) {
    if matches!(orientation, DMDO_90 | DMDO_270) {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    }
}

/// How the display named by `name`, terminated, is turned now, as the
/// desktop's own interface says; upright where it will not say.
fn orientation(name: &[u16]) -> u32 {
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    // SAFETY: a terminated name and a structure of the size it says.
    if unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &raw mut dm) } == 0 {
        return DMDO_DEFAULT;
    }
    // SAFETY: a display's settings, whose union holds its position and turn.
    unsafe { dm.Anonymous1.Anonymous2.dmDisplayOrientation }
}

/// Whether the display named `name` offers `mode` now.
pub(crate) fn offers(name: &str, mode: Mode) -> bool {
    let name = wide(name);
    let (width, height) = desktop_sides(mode, orientation(&name));
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    for index in 0..u32::from(u16::MAX) {
        // SAFETY: a terminated name and a structure of the size it says.
        let ok = unsafe {
            EnumDisplaySettingsW(
                name.as_ptr(),
                index as ENUM_DISPLAY_SETTINGS_MODE,
                &raw mut dm,
            )
        };
        if ok == 0 {
            return false;
        }
        if (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
            == (width, height, mode.refresh_hz)
        {
            return true;
        }
    }
    false
}

/// The refreshes the display named `name` lists at `width` x `height` -- the
/// monitor's own sides, turned or not -- and the one it runs at now.
pub(crate) fn refreshes(name: &str, width: u32, height: u32) -> (Vec<u32>, u32) {
    let name = wide(name);
    let (width, height) = desktop_sides(Mode::new(width, height, 0), orientation(&name));
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    // SAFETY: a terminated name and a structure of the size it says.
    let now = if unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &raw mut dm) }
        == 0
    {
        0
    } else {
        dm.dmDisplayFrequency
    };
    let mut listed = Vec::new();
    for index in 0..u32::from(u16::MAX) {
        // SAFETY: as above.
        let ok = unsafe {
            EnumDisplaySettingsW(
                name.as_ptr(),
                index as ENUM_DISPLAY_SETTINGS_MODE,
                &raw mut dm,
            )
        };
        if ok == 0 {
            break;
        }
        if (dm.dmPelsWidth, dm.dmPelsHeight) == (width, height)
            && !listed.contains(&dm.dmDisplayFrequency)
        {
            listed.push(dm.dmDisplayFrequency);
        }
    }
    (listed, now)
}

/// Put the display named `name` at `mode`, keeping its turn, saved to the
/// system's store only when asked.
pub(crate) fn set_mode(name: &str, mode: Mode, save: bool) -> Result<(), i32> {
    let name = wide(name);
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    (dm.dmPelsWidth, dm.dmPelsHeight) = desktop_sides(mode, orientation(&name));
    dm.dmDisplayFrequency = mode.refresh_hz;
    let flags = if save { CDS_UPDATEREGISTRY } else { 0 };
    // SAFETY: a terminated name and a filled structure; no window.
    let status = unsafe {
        ChangeDisplaySettingsExW(
            name.as_ptr(),
            &raw const dm,
            core::ptr::null_mut(),
            flags,
            core::ptr::null(),
        )
    };
    if status != DISP_CHANGE_SUCCESSFUL {
        return Err(status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Devices::Display::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;

    const OURS: u64 = 7;
    const DP: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY =
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;

    fn path(
        adapter: u64,
        technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
        indirect: Option<bool>,
    ) -> Lit {
        Lit {
            adapter,
            technology,
            indirect,
        }
    }

    /// **Another vendor's virtual display is no reason to save, whatever
    /// technology it reports**: the kernel calls its adapter indirect. A panel
    /// of another adapter is, and the technology still counts where the
    /// kernel says nothing.
    #[test]
    fn a_virtual_display_reporting_displayport_is_not_a_real_one() {
        assert!(real_beside(path(1, DP, Some(false)), OURS), "a panel");
        assert!(
            !real_beside(path(2, DP, Some(true)), OURS),
            "another vendor's virtual display, reported as DisplayPort"
        );
        assert!(!real_beside(path(OURS, DP, Some(false)), OURS), "our own");
        assert!(
            !real_beside(
                path(3, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, None),
                OURS
            ),
            "the kernel silent, an indirect technology"
        );
        assert!(
            real_beside(path(4, DP, None), OURS),
            "the kernel silent, a panel's technology"
        );
    }

    /// **The desktop's own interface speaks of a turned display's modes at
    /// swapped sides**: its list is swapped, and a mode put with the
    /// monitor's own sides is refused, so a quarter turn swaps them there; a
    /// half turn and none keep them.
    #[test]
    fn a_quarter_turn_swaps_the_sides_the_desktops_interface_speaks_of() {
        use windows_sys::Win32::Graphics::Gdi::DMDO_180;

        let mode = Mode::new(1920, 1080, 60);
        assert_eq!(desktop_sides(mode, DMDO_90), (1080, 1920));
        assert_eq!(desktop_sides(mode, DMDO_270), (1080, 1920));
        assert_eq!(desktop_sides(mode, DMDO_180), (1920, 1080));
        assert_eq!(desktop_sides(mode, DMDO_DEFAULT), (1920, 1080));
    }

    /// The active paths as the save decision reads them, printed: a virtual
    /// display's adapter must read indirect and a panel's not. Run with a
    /// virtual display plugged beside a panel.
    #[test]
    #[ignore = "reads this machine's displays"]
    fn the_active_paths_as_the_save_reads_them() {
        let paths = lit();
        assert!(!paths.is_empty(), "no active display");
        for path in &paths {
            println!(
                "adapter {:#018x} technology {} indirect {:?} real beside nothing {}",
                path.adapter,
                path.technology,
                path.indirect,
                real_beside(*path, 0)
            );
        }
    }
}
