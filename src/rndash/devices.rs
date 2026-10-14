//! The Devices pane's rows, and the background poller that lists simulators,
//! emulators and connected phones.

use super::DashMsg;
use crate::devinfo;
use simon::android;
use simon::devices::{
    get_all_installed, get_all_running, InstalledDevice, Platform, RunningDevice,
};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// How to boot a device from the pane — physical devices are already connected,
/// so they have no boot target.
#[derive(Clone)]
pub(super) enum BootTarget {
    IosSim(String),
    Avd(String),
}

/// How to open a URL on a running device (for the configured app link).
#[derive(Clone)]
pub(super) enum OpenTarget {
    IosSim(String),      // simulator udid
    IosPhysical(String), // device udid
    AndroidSerial(String),
}

/// A row in the Devices pane: a simulator/emulator (bootable) or a connected
/// physical device (just listed). `open` is set once the device is running;
/// `installed`/`foreground` are populated (when a bundleId is configured) for
/// running devices — `None` means "unknown / not checked".
#[derive(Clone)]
pub(super) struct DeviceRow {
    pub(super) label: String,
    pub(super) id: String, // what `--device` takes: udid, AVD name or adb serial
    pub(super) platform: Platform,
    pub(super) running: bool,
    pub(super) boot: Option<BootTarget>,
    pub(super) open: Option<OpenTarget>,
    pub(super) installed: Option<bool>,
    pub(super) foreground: Option<bool>,
    pub(super) detail: Option<String>, // OS version, API level, connection…
    pub(super) problem: Option<String>, // why it can't be used yet (unauthorized, Developer Mode off…)
    pub(super) pinned: bool,            // the device this session (`signalbox up`) builds onto
}

impl DeviceRow {
    /// Section in the Devices pane (also the sort order).
    pub(super) fn group(&self) -> (u8, &'static str) {
        if self.pinned {
            return (0, "This session");
        }
        match (self.platform, self.boot.is_some()) {
            (Platform::Ios, true) => (1, "iOS Simulators"),
            (Platform::Ios, false) => (2, "iOS Devices"),
            (Platform::Android, true) => (3, "Android Emulators"),
            (Platform::Android, false) => (4, "Android Devices"),
        }
    }

    /// iOS device/simulator udid (known even before boot, from the boot target).
    pub(super) fn ios_udid(&self) -> Option<String> {
        if let Some(OpenTarget::IosSim(u)) | Some(OpenTarget::IosPhysical(u)) = &self.open {
            return Some(u.clone());
        }
        if let Some(BootTarget::IosSim(u)) = &self.boot {
            return Some(u.clone());
        }
        None
    }

    /// Android adb serial — only known once the device/emulator is running.
    pub(super) fn android_serial(&self) -> Option<String> {
        match &self.open {
            Some(OpenTarget::AndroidSerial(s)) => Some(s.clone()),
            _ => None,
        }
    }
}

/// List the devices every 2.5s and send them as `DashMsg::Devices`, until the
/// dashboard is gone. App state is probed for `android_bundle`; `pinned_udid`
/// marks this session's device.
pub(super) fn spawn_poller(
    dev_tx: Sender<DashMsg>,
    android_bundle: Option<String>,
    pinned_udid: Option<String>,
) {
    std::thread::spawn(move || {
        // devicectl is slow; refresh the iPhone details less often than the list.
        let mut ios_details: HashMap<String, devinfo::IosDetail> = HashMap::new();
        let mut ios_checked: Option<Instant> = None;
        let mut ios_udids: Vec<String> = Vec::new();
        loop {
            let running_devices = get_all_running(None);
            let udids: Vec<String> = running_devices
                .iter()
                .filter_map(|d| match d {
                    RunningDevice::IosPhysical { udid, .. } => Some(udid.clone()),
                    _ => None,
                })
                .collect();
            if udids.is_empty() {
                ios_details.clear();
            } else if udids != ios_udids
                || ios_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(15))
            {
                ios_details = devinfo::ios_physical();
                ios_checked = Some(Instant::now());
            }
            ios_udids = udids;
            // Resolve a running emulator's adb serial from its AVD name.
            let android_serial = |avd: &str| {
                running_devices.iter().find_map(|d| match d {
                    RunningDevice::AndroidEmulator { name, serial } if name == avd => {
                        Some(serial.clone())
                    }
                    _ => None,
                })
            };
            // Is the configured app installed / in the foreground? Android only —
            // the iOS simctl probe was unreliable, so we don't check it.
            let android_state = |serial: &str| match android_bundle.as_deref() {
                Some(pkg) => (
                    Some(android::app_installed(serial, pkg)),
                    Some(android::foreground_package(serial).as_deref() == Some(pkg)),
                ),
                None => (None, None),
            };
            // Installed sims/emulators (bootable; openable once running) …
            let mut rows: Vec<DeviceRow> = Vec::new();
            for d in get_all_installed(None) {
                let label = match &d {
                    InstalledDevice::IosSim { name, runtime, .. } => {
                        format!("{name}  {runtime}")
                    }
                    InstalledDevice::AndroidAvd { name, .. } => name.clone(),
                };
                let running = d.running();
                let row = match &d {
                    InstalledDevice::IosSim { udid, .. } => DeviceRow {
                        label,
                        id: udid.clone(),
                        platform: Platform::Ios,
                        running,
                        boot: Some(BootTarget::IosSim(udid.clone())),
                        open: running.then(|| OpenTarget::IosSim(udid.clone())),
                        installed: None, // iOS install state not probed
                        foreground: None,
                        detail: None, // the runtime is already in the label
                        problem: None,
                        pinned: false,
                    },
                    InstalledDevice::AndroidAvd { name, .. } => {
                        let serial = if running { android_serial(name) } else { None };
                        let (installed, foreground) = match &serial {
                            Some(s) => android_state(s),
                            None => (None, None),
                        };
                        let detail = serial
                            .as_deref()
                            .and_then(|s| devinfo::android_running(s, false))
                            .or_else(|| devinfo::avd(name));
                        DeviceRow {
                            label,
                            id: name.clone(),
                            platform: Platform::Android,
                            running,
                            boot: Some(BootTarget::Avd(name.clone())),
                            open: serial.map(OpenTarget::AndroidSerial),
                            installed,
                            foreground,
                            detail,
                            problem: None,
                            pinned: false,
                        }
                    }
                };
                rows.push(row);
            }
            // … plus any connected physical devices (already running, not bootable).
            for d in &running_devices {
                match d {
                    RunningDevice::IosPhysical {
                        name,
                        udid,
                        os_version,
                    } => {
                        let extra = ios_details.get(udid);
                        rows.push(DeviceRow {
                            label: format!("{name}  iOS {os_version}"),
                            id: udid.clone(),
                            platform: Platform::Ios,
                            running: true,
                            boot: None,
                            open: Some(OpenTarget::IosPhysical(udid.clone())),
                            installed: None, // devicectl app queries are slow — skip
                            foreground: None,
                            detail: extra.map(|e| e.text.clone()).filter(|t| !t.is_empty()),
                            problem: extra.and_then(|e| e.problem.clone()),
                            pinned: false,
                        });
                    }
                    RunningDevice::AndroidPhysical { serial, .. } => {
                        let (installed, foreground) = android_state(serial);
                        rows.push(DeviceRow {
                            label: d.name().to_string(),
                            id: serial.clone(),
                            platform: Platform::Android,
                            running: true,
                            boot: None,
                            open: Some(OpenTarget::AndroidSerial(serial.clone())),
                            installed,
                            foreground,
                            detail: devinfo::android_running(serial, true),
                            problem: None,
                            pinned: false,
                        });
                    }
                    _ => {}
                }
            }
            // Phones adb can see but not use yet, so they don't silently vanish.
            for (serial, problem) in devinfo::android_unready() {
                rows.push(DeviceRow {
                    label: serial.clone(),
                    id: serial,
                    platform: Platform::Android,
                    running: false,
                    boot: None,
                    open: None,
                    installed: None,
                    foreground: None,
                    detail: None,
                    problem: Some(problem),
                    pinned: false,
                });
            }
            for r in &mut rows {
                r.pinned = pinned_udid.is_some()
                    && (r.ios_udid() == pinned_udid || r.android_serial() == pinned_udid);
            }
            // Group as the pane shows them: this session's device, iOS sims,
            // iPhones, Android emulators, phones.
            rows.sort_by_key(|r| r.group());
            if dev_tx.send(DashMsg::Devices(rows)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2500));
        }
    });
}
