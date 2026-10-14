//! `signalbox` — the tiled React Native dashboard. One window: a Processes pane
//! (Metro + install/run, each in a PTY), a Devices & Actions pane (launch
//! sims/emulators, install & run, open links, app-presence), and the embedded
//! `RnView` (JS logs / network / perf). Keys are pane-scoped — each pane owns its
//! own, shown in its footer — except a few globals (focus, `R`/`D` Metro
//! reload/dev-menu, quit). The Metro PTY takes raw-key passthrough in input mode,
//! so `r`/`d`/`j` and anything else Metro supports work as in a normal terminal.

use crate::control;
use crate::devinfo;
use crate::metro_events;
use crate::proc::PtyProcess;
use crate::rnclient::{ConnCmd, RnClient};
use crate::rnconfig::{PackageManager, ProjectConfig};
use crate::rnview::RnView;
use crate::session::{self, Pinned, SessionFile, SimCleanup};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use serde_json::{json, Value};
use simon::devices::{
    get_all_installed, get_all_running, InstalledDevice, Platform, RunningDevice,
};
use simon::{android, ios};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Processes,
    Devices,
    Logs,
}

enum DashMsg {
    Devices(Vec<DeviceRow>),
    Flash(String),
}

/// How to boot a device from the pane — physical devices are already connected,
/// so they have no boot target.
#[derive(Clone)]
enum BootTarget {
    IosSim(String),
    Avd(String),
}

/// How to open a URL on a running device (for the configured app link).
#[derive(Clone)]
enum OpenTarget {
    IosSim(String),      // simulator udid
    IosPhysical(String), // device udid
    AndroidSerial(String),
}

/// A row in the Devices pane: a simulator/emulator (bootable) or a connected
/// physical device (just listed). `open` is set once the device is running;
/// `installed`/`foreground` are populated (when a bundleId is configured) for
/// running devices — `None` means "unknown / not checked".
#[derive(Clone)]
struct DeviceRow {
    label: String,
    id: String, // what `--device` takes: udid, AVD name or adb serial
    platform: Platform,
    running: bool,
    boot: Option<BootTarget>,
    open: Option<OpenTarget>,
    installed: Option<bool>,
    foreground: Option<bool>,
    detail: Option<String>,  // OS version, API level, connection…
    problem: Option<String>, // why it can't be used yet (unauthorized, Developer Mode off…)
    pinned: bool,            // the device this session (`signalbox up`) builds onto
}

impl DeviceRow {
    /// Section in the Devices pane (also the sort order).
    fn group(&self) -> (u8, &'static str) {
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
    fn ios_udid(&self) -> Option<String> {
        if let Some(OpenTarget::IosSim(u)) | Some(OpenTarget::IosPhysical(u)) = &self.open {
            return Some(u.clone());
        }
        if let Some(BootTarget::IosSim(u)) = &self.boot {
            return Some(u.clone());
        }
        None
    }

    /// Android adb serial — only known once the device/emulator is running.
    fn android_serial(&self) -> Option<String> {
        match &self.open {
            Some(OpenTarget::AndroidSerial(s)) => Some(s.clone()),
            _ => None,
        }
    }
}

pub struct DashApp {
    project: ProjectConfig,
    procs: Vec<PtyProcess>,
    proc_sel: usize,
    metro_idx: Option<usize>,
    metro_external: bool, // metro_idx is a watcher tab for a Metro started outside signalbox
    client: RnClient,
    rnview: RnView,
    focus: Pane,
    input_mode: bool,
    devices: Vec<DeviceRow>,
    dev_sel: usize,
    rx: Receiver<DashMsg>,
    tx: Sender<DashMsg>,
    flash: Option<(String, Instant)>,
    proc_area: Option<Rect>,
    v_split: u16, // % height of the top row (processes/devices) vs the logs pane
    h_split: u16, // % width of the processes pane vs devices
    confirm_quit: bool,
    link_picker: bool,                    // deep-link quick-picker overlay
    link_search: Option<(String, usize)>, // `/` in the picker: filter query and selected match
    link_form: Option<LinkForm>, // values for a picked link's placeholders, or a pasted URL
    link_values: HashMap<String, String>, // last value per placeholder name, prefilled next time
    help: Option<u16>,           // `?` key-reference popup, with its scroll offset
    quit: bool,
    started: Instant, // the "debugger off" notice only shows for a while after start
    pinned: Option<Pinned>, // device this session builds onto (`signalbox up`)
    setup: Option<SetupRun>, // `signalbox up` steps still in progress
    status: String,   // session status, mirrored to .signalbox/session.json
    term: Arc<AtomicBool>, // SIGTERM/SIGHUP received (e.g. `signalbox down`)
    delete_sim_on_quit: bool,
    sim_builds: Vec<(usize, String, bool)>, // build tabs → (device, android?), to point the app at our port
    ctl_rx: Receiver<control::Request>,
    socket: Option<PathBuf>, // control socket, when it could be bound
    #[cfg(test)]
    opened: Vec<(String, Option<String>)>, // run_on_device calls: (device, url)
}

/// What `signalbox up` asked for, applied once the dashboard opens.
pub struct Setup {
    pub pinned: Option<Pinned>,
    pub install: bool,
    pub start: bool,               // start Metro and build onto the pinned device
    pub prebuilt: Option<PathBuf>, // install this .app instead of building
}

/// Process tabs of the setup steps; `None` = step not needed.
struct SetupRun {
    install: Option<usize>,
    boot: Option<usize>,
    build: Option<usize>,
    metro_started: bool,
    prebuilt: Option<PathBuf>,
}

pub fn run(project: ProjectConfig, setup: Setup) -> Result<()> {
    let mut app = DashApp::new(project, setup.pinned.clone());
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        let _ = signal_hook::flag::register(sig, app.term.clone());
    }
    app.attach_external_metro();
    app.begin_setup(&setup);
    app.write_session();
    crate::devlog::log(
        "dashboard",
        format!(
            "started (signalbox {}) in {}",
            env!("CARGO_PKG_VERSION"),
            app.root().display()
        ),
    );
    let mut terminal = ratatui::init();
    // Pastes arrive as one event (for the link form) instead of keystrokes.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let res = app.main_loop(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    let root = app.root();
    let delete = app
        .delete_sim_on_quit
        .then(|| app.pinned.as_ref().map(|p| p.udid.clone()))
        .flatten();
    // A kept simulator: stop its app from pointing at our (soon free) port.
    // On SIGTERM `signalbox down` does this (or deletes the simulator).
    if let (None, false, Some(p)) = (&delete, app.term.load(Ordering::Relaxed), &app.pinned) {
        if p.android {
            session::android_release(&p.udid);
        } else if let (true, Some(bundle)) = (p.simulator, app.project.ios_bundle_id()) {
            session::release_app(&p.udid, bundle);
        }
    }
    let socket = app.socket.clone();
    // Stopped by a signal (window closed, killed, `signalbox down`) with a
    // simulator we created: leave the session files, so `down`, `gc` or the
    // next `up` can still find and delete that simulator.
    let orphan = app.term.load(Ordering::Relaxed) && app.pinned.as_ref().is_some_and(|p| p.created);
    drop(app); // stops Metro and the other processes
    if !orphan {
        session::remove_session(&root);
    }
    if let Some(p) = socket {
        let _ = std::fs::remove_file(p);
    }
    if let Some(udid) = delete {
        eprintln!("deleting simulator {udid}…");
        if let Err(e) = session::delete_simulator(&udid) {
            eprintln!("{e}");
        }
    }
    res
}

impl DashApp {
    fn new(project: ProjectConfig, pinned: Option<Pinned>) -> DashApp {
        let (tx, rx) = std::sync::mpsc::channel();
        let client = RnClient::start(project.metro_port());
        // Background device poller — simctl/adb are slow, so keep them off the UI thread.
        let dev_tx = tx.clone();
        let android_bundle = project.android_bundle_id().map(String::from);
        let pinned_udid = pinned.as_ref().map(|p| p.udid.clone());
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
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let path = control::socket_path();
        let socket = control::serve(&path, ctl_tx).ok().map(|_| path);
        DashApp {
            ctl_rx,
            socket,
            ..DashApp::bare(project, pinned, client, tx, rx)
        }
    }

    /// The dashboard's state alone: no device poller, no control socket.
    fn bare(
        project: ProjectConfig,
        pinned: Option<Pinned>,
        client: RnClient,
        tx: Sender<DashMsg>,
        rx: Receiver<DashMsg>,
    ) -> DashApp {
        let mut rnview = RnView::new(None);
        rnview.set_root(Some(project.root.clone()));
        rnview.set_embedded(true);
        DashApp {
            project,
            procs: Vec::new(),
            proc_sel: 0,
            metro_idx: None,
            metro_external: false,
            client,
            rnview,
            focus: Pane::Processes,
            input_mode: false,
            devices: Vec::new(),
            dev_sel: 0,
            rx,
            tx,
            flash: None,
            proc_area: None,
            v_split: 45,
            h_split: 60,
            confirm_quit: false,
            link_picker: false,
            link_search: None,
            link_form: None,
            link_values: HashMap::new(),
            help: None,
            quit: false,
            started: Instant::now(),
            pinned,
            setup: None,
            status: "ready".into(),
            term: Arc::new(AtomicBool::new(false)),
            delete_sim_on_quit: false,
            sim_builds: Vec::new(),
            ctl_rx: std::sync::mpsc::channel().1,
            socket: None,
            #[cfg(test)]
            opened: Vec::new(),
        }
    }

    /// Kick off `signalbox up`: install deps and boot the pinned simulator (in
    /// parallel), then Metro, then the build — driven by `advance_setup`.
    fn begin_setup(&mut self, setup: &Setup) {
        if !setup.start {
            return;
        }
        let install = if setup.install {
            self.spawn_proc("Install", &self.project.install_command())
        } else {
            None
        };
        let boot = match &self.pinned {
            Some(p) if p.simulator && session::sim_state(&p.udid).as_deref() != Some("Booted") => {
                let cmd = session::boot_command(&p.udid);
                self.spawn_proc("Simulator", &cmd)
            }
            _ => None,
        };
        if setup.install && install.is_none() {
            return self.set_status("install_failed");
        }
        self.setup = Some(SetupRun {
            install,
            boot,
            build: None,
            metro_started: false,
            prebuilt: setup.prebuilt.clone(),
        });
        self.advance_setup();
    }

    /// Has this build installed the app? `run-ios` can hang in its final
    /// `simctl launch` (seen on freshly created simulators) after installing,
    /// so its exit can't be the only signal.
    fn build_installed(&self, idx: usize) -> Option<bool> {
        let p = self.procs.get(idx)?;
        if let Some(code) = p.exit_code() {
            return Some(code == 0);
        }
        let tail = p.tail_text(40);
        let launched = tail.iter().any(|l| {
            l.starts_with("info Launching")
                || l.contains("Starting: Intent")
                || l.starts_with("info Starting the app")
        });
        launched.then_some(true)
    }

    /// `Some(true)` = finished OK (or not needed), `Some(false)` = failed, `None` = running.
    fn step_result(&self, idx: Option<usize>) -> Option<bool> {
        match idx {
            None => Some(true),
            Some(i) => match self.procs.get(i) {
                Some(p) => p.exit_code().map(|c| c == 0),
                None => Some(false),
            },
        }
    }

    fn advance_setup(&mut self) {
        let Some(s) = &self.setup else {
            return;
        };
        let (install, boot, build, metro_started) = (s.install, s.boot, s.build, s.metro_started);
        let (install, boot) = (self.step_result(install), self.step_result(boot));
        let fail = |app: &mut DashApp, status: &str, msg: &str| {
            app.setup = None;
            app.set_status(status);
            app.set_flash(msg.to_string());
        };
        if install == Some(false) {
            return fail(
                self,
                "install_failed",
                "install failed — see the Install tab",
            );
        }
        if boot == Some(false) {
            return fail(
                self,
                "boot_failed",
                "simulator failed to boot — see the Simulator tab",
            );
        }
        if install == Some(true) && !metro_started {
            let ours = self.metro_idx.is_some_and(|i| {
                !self.metro_external && self.procs.get(i).is_some_and(|p| p.is_alive())
            });
            if !ours {
                self.start_metro();
            }
            if let Some(s) = self.setup.as_mut() {
                s.metro_started = true;
            }
        }
        if install.is_none() {
            return self.set_status("installing");
        }
        if boot.is_none() {
            return self.set_status("booting");
        }
        let Some(build) = build else {
            // run-ios opens its own Metro in a new Terminal if none answers on the port yet.
            if !metro_events::port_in_use(self.project.metro_port()) {
                return self.set_status("starting_metro");
            }
            let Some(udid) = self.pinned.as_ref().map(|p| p.udid.clone()) else {
                self.setup = None;
                return self.set_status("ready");
            };
            let before = self.procs.len();
            let android = self.pinned.as_ref().is_some_and(|p| p.android);
            let prebuilt = self.setup.as_ref().and_then(|s| s.prebuilt.clone());
            match (prebuilt, self.project.ios_bundle_id().map(String::from)) {
                (Some(app), Some(bundle)) if !android => {
                    let cmd = session::install_prebuilt_command(
                        &udid,
                        &app,
                        &bundle,
                        self.project.metro_port(),
                    );
                    if let Some(i) = self.spawn_proc("Install app", &cmd) {
                        self.proc_sel = i;
                    }
                }
                _ => self.run_platform(android, Some(udid)),
            }
            if self.procs.len() == before {
                return fail(self, "build_failed", "couldn't start the build");
            }
            if let Some(s) = self.setup.as_mut() {
                s.build = Some(before);
            }
            return self.set_status("building");
        };
        match self.build_installed(build) {
            None => {}
            Some(true) => {
                self.setup = None;
                self.set_status("running");
            }
            Some(false) => fail(self, "build_failed", "build failed — see the iOS tab"),
        }
    }

    /// Answer a control-socket request (see `control.rs`).
    fn control(&mut self, cmd: &str, a: &Value) -> Value {
        // Default to this session's own device: another app can be connected to
        // the same Metro (e.g. a simulator still pointing at this port).
        let pinned_name = self.pinned.as_ref().and_then(|p| p.name.clone());
        let s = |k: &str| {
            a[k].as_str()
                .filter(|v| !v.is_empty())
                .or(if k == "device" {
                    pinned_name.as_deref()
                } else {
                    None
                })
        };
        let since = a["since"].as_u64().unwrap_or(0);
        let limit = a["limit"].as_u64().map_or(50, |n| n as usize);
        let proc_json = |p: &PtyProcess| json!({ "label": p.label, "running": p.is_alive(), "exit_code": p.exit_code() });
        match cmd {
            "status" => {
                let metro = self.metro_idx.and_then(|i| self.procs.get(i));
                json!({
                    "project": self.project.name,
                    "root": self.project.root,
                    "port": self.project.metro_port(),
                    "status": self.status,
                    "device": self.pinned.as_ref().map(|p| p.udid.clone()),
                    "device_name": self.pinned.as_ref().and_then(|p| p.name.clone()),
                    "metro": if self.metro_external { "external" } else if metro.is_some_and(|p| p.is_alive()) { "running" } else { "stopped" },
                    "apps": self.rnview.apps_json(),
                    "processes": self.procs.iter().map(proc_json).collect::<Vec<_>>(),
                })
            }
            "logs" => self
                .rnview
                .logs_json(s("device"), s("level"), s("filter"), since, limit),
            "network" => self.rnview.net_json(
                s("device"),
                a["failed_only"].as_bool().unwrap_or(false),
                s("filter"),
                since,
                limit,
            ),
            "request" => match s("id").and_then(|id| self.rnview.request_text(s("device"), id)) {
                Some(t) => json!({ "text": t }),
                None => json!({ "error": "no request with that id (see network)" }),
            },
            "errors" => {
                let failed: Vec<Value> = self
                    .procs
                    .iter()
                    .filter(|p| p.exit_code().is_some_and(|c| c != 0))
                    .map(|p| json!({ "label": p.label, "exit_code": p.exit_code(), "tail": p.tail_text(40) }))
                    .collect();
                json!({
                    "status": self.status,
                    "logs": self.rnview.logs_json(s("device"), Some("error"), None, since, limit.min(20)),
                    "requests": self.rnview.net_json(s("device"), true, None, since, limit.min(20)),
                    "failed_processes": failed,
                })
            }
            "output" => {
                let want = s("process").map(str::to_lowercase);
                let p = match &want {
                    Some(w) => self
                        .procs
                        .iter()
                        .rev()
                        .find(|p| p.label.to_lowercase().contains(w)),
                    None => self.procs.last(),
                };
                match p {
                    Some(p) => {
                        json!({ "label": p.label, "running": p.is_alive(), "exit_code": p.exit_code(), "lines": p.tail_text(a["lines"].as_u64().map_or(80, |n| n as usize)) })
                    }
                    None => {
                        json!({ "error": "no such process", "processes": self.procs.iter().map(|p| p.label.clone()).collect::<Vec<_>>() })
                    }
                }
            }
            "reload" => {
                self.metro_key(b'r', "reload");
                json!({ "ok": true, "message": self.flash.as_ref().map(|f| f.0.clone()) })
            }
            "deeplinks" => json!({
                "links": self.project.deeplinks.iter().map(|d| json!({
                    "name": d.label(),
                    "url": d.url(),
                    "placeholders": crate::rnconfig::link_placeholders(d.url()),
                })).collect::<Vec<_>>(),
            }),
            "restart_metro" => {
                self.start_metro();
                json!({ "ok": true, "port": self.project.metro_port() })
            }
            "rebuild" => {
                let Some(udid) = a["device"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .map(String::from)
                    .or_else(|| self.pinned.as_ref().map(|p| p.udid.clone()))
                else {
                    return json!({ "error": "no device: this session isn't pinned to one, pass device (udid)" });
                };
                let before = self.procs.len();
                let android = self
                    .pinned
                    .as_ref()
                    .is_some_and(|p| p.android && p.udid == udid)
                    || session::adb_serials().contains(&udid);
                self.run_platform(android, Some(udid));
                if self.procs.len() == before {
                    return json!({ "error": self.flash.as_ref().map(|f| f.0.clone()) });
                }
                // Track it like the setup build, so the session status follows it.
                self.setup = Some(SetupRun {
                    install: None,
                    boot: None,
                    build: Some(before),
                    metro_started: true,
                    prebuilt: None,
                });
                self.set_status("building");
                json!({ "ok": true, "process": self.procs[before].label })
            }
            _ => {
                json!({ "error": format!("unknown command {cmd:?}"), "commands": ["deeplinks", "status", "logs", "errors", "network", "request", "output", "reload", "rebuild", "restart_metro"] })
            }
        }
    }

    fn is_simulator(&self, udid: &str) -> bool {
        self.devices
            .iter()
            .any(|d| matches!(&d.boot, Some(BootTarget::IosSim(u)) if u == udid))
            || self
                .pinned
                .as_ref()
                .is_some_and(|p| p.simulator && p.udid == udid)
    }

    /// Once an iOS simulator build succeeds, make the app load from our Metro
    /// port. The port in the build (`RCT_METRO_PORT`) is ignored when React
    /// Native ships prebuilt, so set the app's `RCT_jsLocation` on that
    /// simulator and relaunch it.
    fn finish_sim_builds(&mut self) {
        let done: Vec<(usize, String, bool, bool)> = self
            .sim_builds
            .iter()
            .filter_map(|(i, u, a)| self.build_installed(*i).map(|ok| (*i, u.clone(), *a, ok)))
            .collect();
        if done.is_empty() {
            return;
        }
        self.sim_builds
            .retain(|(i, _, _)| !done.iter().any(|d| d.0 == *i));
        let port = self.project.metro_port();
        for (_, dev, android, _) in done.into_iter().filter(|d| d.3) {
            if android {
                let (tx, pkg) = (
                    self.tx.clone(),
                    self.project.android_bundle_id().map(String::from),
                );
                std::thread::spawn(move || {
                    let msg = match session::android_point_app_at_port(&dev, pkg.as_deref(), port) {
                        Ok(()) => format!("app on {dev} mapped to Metro :{port}"),
                        Err(e) => format!("couldn't map {dev} to :{port}: {e}"),
                    };
                    let _ = tx.send(DashMsg::Flash(msg));
                });
                continue;
            }
            let Some(bundle) = self.project.ios_bundle_id().map(String::from) else {
                continue;
            };
            let (tx, udid) = (self.tx.clone(), dev);
            std::thread::spawn(move || {
                let msg = match session::point_app_at_port(&udid, &bundle, port) {
                    Ok(true) => format!("app relaunched on Metro :{port}"),
                    Ok(false) => return,
                    Err(e) => format!("couldn't point the app at :{port}: {e}"),
                };
                let _ = tx.send(DashMsg::Flash(msg));
            });
        }
    }

    fn set_status(&mut self, status: &str) {
        if self.status != status {
            crate::devlog::log("dashboard", format!("status {} → {status}", self.status));
            self.status = status.to_string();
            self.write_session();
        }
    }

    fn write_session(&self) {
        session::write_session(
            &self.root(),
            &SessionFile {
                pid: std::process::id(),
                root: self.project.root.clone(),
                port: self.project.metro_port(),
                udid: self.pinned.as_ref().map(|p| p.udid.clone()),
                created_sim: self.pinned.as_ref().is_some_and(|p| p.created),
                status: self.status.clone(),
                bundle: self.project.ios_bundle_id().map(String::from),
                platform: self.pinned.as_ref().map(|p| {
                    if p.android {
                        "android"
                    } else if p.simulator {
                        "ios_simulator"
                    } else {
                        "ios_device"
                    }
                    .to_string()
                }),
                socket: self.socket.as_ref().map(|p| p.display().to_string()),
            },
        );
    }

    /// The created simulator to offer deleting on quit (cleanup = ask).
    fn ask_delete_sim(&self) -> bool {
        self.pinned
            .as_ref()
            .is_some_and(|p| p.created && p.cleanup == SimCleanup::Ask)
    }

    fn request_quit(&mut self, delete_sim: bool) {
        self.delete_sim_on_quit = delete_sim
            || self
                .pinned
                .as_ref()
                .is_some_and(|p| p.created && p.cleanup == SimCleanup::Delete);
        self.quit = true;
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(&self.project.root)
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn pty_size(&self) -> (u16, u16) {
        self.proc_area
            .map(|r| (r.height.max(1), r.width.max(1)))
            .unwrap_or((24, 80))
    }

    fn spawn_proc(&mut self, label: &str, command: &str) -> Option<usize> {
        self.spawn_proc_env(label, command, self.project.command_env())
    }

    fn spawn_proc_env(
        &mut self,
        label: &str,
        command: &str,
        env: BTreeMap<String, String>,
    ) -> Option<usize> {
        let (rows, cols) = self.pty_size();
        match PtyProcess::spawn(label, command, &self.root(), &env, rows, cols) {
            Ok(p) => {
                self.procs.push(p);
                Some(self.procs.len() - 1)
            }
            Err(e) => {
                self.set_flash(format!("failed to start {label}: {e}"));
                None
            }
        }
    }

    /// If a Metro started elsewhere already holds the port, open a tab that
    /// follows its bundler events (`m` in that tab takes it over).
    fn attach_external_metro(&mut self) {
        let port = self.project.metro_port();
        if self.metro_idx.is_some() || !metro_events::port_in_use(port) {
            return;
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "signalbox".into());
        let cmd = format!(
            "'{}' metro-events --port {port}",
            exe.replace('\'', "'\\''")
        );
        if let Some(i) = self.spawn_proc("Metro (external)", &cmd) {
            self.metro_idx = Some(i);
            self.metro_external = true;
            self.proc_sel = i;
            self.set_flash(format!("Metro already running on :{port} — m to take over"));
        }
    }

    /// Metro's start command, prefixed with `npx kill-port` when something
    /// (e.g. a Metro from another terminal) already holds the port.
    fn metro_cmd(&self) -> String {
        let port = self.project.metro_port();
        let cmd = self.project.metro_command();
        if metro_events::port_in_use(port) {
            format!("npx --yes kill-port {port} && {cmd}")
        } else {
            cmd
        }
    }

    fn start_metro(&mut self) {
        let cmd = self.metro_cmd();
        // Restart in place if Metro is already a tab.
        if let Some(i) = self.metro_idx {
            if let Some(old) = self.procs.get_mut(i) {
                old.kill();
            }
            let (rows, cols) = self.pty_size();
            match PtyProcess::spawn(
                "Metro",
                &cmd,
                &self.root(),
                &self.project.command_env(),
                rows,
                cols,
            ) {
                Ok(p) => {
                    self.procs[i] = p;
                    self.proc_sel = i;
                    let took_over = std::mem::take(&mut self.metro_external);
                    self.set_flash(if took_over {
                        "took over external Metro"
                    } else {
                        "restarted Metro"
                    });
                }
                Err(e) => self.set_flash(format!("failed to restart Metro: {e}")),
            }
            return;
        }
        if let Some(i) = self.spawn_proc("Metro", &cmd) {
            self.metro_idx = Some(i);
            self.proc_sel = i;
            self.set_flash(format!("started Metro (:{})", self.project.metro_port()));
        }
    }

    /// Deep clean the project (caches, node_modules, Pods, build dirs — and
    /// reinstall), via `clean_command` (react-native-clean-project by default).
    fn deep_clean(&mut self) {
        let cmd = self.project.clean_command();
        if let Some(i) = self.spawn_proc("Clean", &cmd) {
            self.proc_sel = i;
            self.set_flash("deep clean");
        }
    }

    /// Reinstall JS deps + CocoaPods, no clean.
    fn reinstall(&mut self) {
        let cmd = self.project.install_command();
        if let Some(i) = self.spawn_proc("Install", &cmd) {
            self.proc_sel = i;
            self.set_flash("reinstalling deps");
        }
    }

    /// Combined reset: deep clean (which reinstalls) → start Metro, chained in
    /// one pane. Stops the tracked Metro first so the new one can bind the port.
    fn reset_project(&mut self) {
        self.metro_external = false;
        if let Some(i) = self.metro_idx.take() {
            if let Some(old) = self.procs.get_mut(i) {
                old.kill();
            }
        }
        let cmd = format!("{} && {}", self.project.clean_command(), self.metro_cmd());
        if let Some(i) = self.spawn_proc("Reset", &cmd) {
            self.proc_sel = i;
            self.set_flash("reset: deep clean (reinstalls) → Metro");
        }
    }

    fn run_platform(&mut self, android: bool, id: Option<String>) {
        let (label, mut cmd) = if android {
            ("Android", self.project.android_command())
        } else {
            ("iOS", self.project.ios_command())
        };
        // Preferred targeting: a `{udid}` (iOS) / `{serial}` (Android) placeholder
        // in the configured command, so it lands exactly where the user wants —
        // e.g. before a `-- ...` passthrough. Otherwise best-effort append (works
        // for a plain `run-ios`, but a `--` in the script sends it to the wrong
        // side, so iOS also relies on the target already being booted; see below).
        let placeholder = if android { "{serial}" } else { "{udid}" };
        if cmd.contains(placeholder) {
            cmd = cmd.replace(placeholder, id.as_deref().unwrap_or(""));
        } else if let Some(ref id) = id {
            let flag = if android {
                format!("--deviceId {id}")
            } else {
                format!("--udid {id}")
            };
            let already = ["--udid", "--device", "--simulator", "--deviceId"]
                .iter()
                .any(|f| cmd.contains(f));
            if !already {
                let sep = if self.project.package_manager() == PackageManager::Npm {
                    " -- "
                } else {
                    " "
                };
                cmd = format!("{cmd}{sep}{flag}");
            }
        }
        // Android: also pin the device via ANDROID_SERIAL so adb (install +
        // launch, under gradle) can't pick a different connected device.
        let mut env = self.project.command_env();
        if android {
            if let Some(s) = &id {
                env.insert("ANDROID_SERIAL".to_string(), s.clone());
            }
        }
        if let Some(i) = self.spawn_proc_env(label, &cmd, env) {
            if let Some(dev) = id.filter(|u| android || self.is_simulator(u)) {
                self.sim_builds.push((i, dev, android));
            }
            self.proc_sel = i;
            self.focus = Pane::Processes;
            self.set_flash(format!("running: {cmd}"));
        }
    }

    /// Install & run the app on the highlighted device. Platform comes from the
    /// device (no need for separate iOS/Android keys); an offline sim/emulator is
    /// launched first so the run targets it.
    fn install_selected(&mut self) {
        let dev = match self.devices.get(self.dev_sel) {
            Some(d) => d.clone(),
            None => {
                self.set_flash("no device selected");
                return;
            }
        };
        let android = dev.platform == Platform::Android;
        if !dev.running {
            if let Some(target) = dev.boot.clone() {
                self.boot_target(target, dev.label.clone());
                // Don't build yet: a sim/emulator that isn't up makes run-ios /
                // run-android fall back to "first available" — the wrong device.
                // Boot now; build on the next b once it shows ● running.
                let what = if android { "emulator" } else { "simulator" };
                self.set_flash(format!(
                    "{what} starting — press b again once it shows ● running"
                ));
                return;
            }
        }
        // Target this exact device: iOS by udid, Android by adb serial.
        let id = if android {
            dev.android_serial()
        } else {
            dev.ios_udid()
        };
        self.run_platform(android, id);
    }

    /// Start (boot) the highlighted simulator/emulator.
    fn start_selected_device(&mut self) {
        let dev = match self.devices.get(self.dev_sel) {
            Some(d) => d.clone(),
            None => return,
        };
        if dev.running {
            self.set_flash(format!("{} is already running", dev.label));
            return;
        }
        match dev.boot.clone() {
            Some(target) => self.boot_target(target, dev.label.clone()),
            None => self.set_flash("that's a physical device — it's already connected"),
        }
    }

    /// Stop (shut down) the highlighted simulator/emulator. Physical devices
    /// can't be stopped from here.
    fn stop_selected_device(&mut self) {
        let dev = match self.devices.get(self.dev_sel) {
            Some(d) => d.clone(),
            None => return,
        };
        if !dev.running {
            self.set_flash(format!("{} isn't running", dev.label));
            return;
        }
        if dev.boot.is_none() {
            self.set_flash("can't stop a physical device from here");
            return;
        }
        let tx = self.tx.clone();
        let label = dev.label.clone();
        self.set_flash(format!("stopping {label}…"));
        std::thread::spawn(move || {
            let res = match dev.open {
                Some(OpenTarget::IosSim(udid)) => ios::shutdown_simulator(&udid),
                Some(OpenTarget::AndroidSerial(serial)) => android::stop_emulator(&serial),
                _ => Ok(()),
            };
            let msg = match res {
                Ok(()) => format!("stopped {label}"),
                Err(e) => format!("failed to stop {label}: {e}"),
            };
            let _ = tx.send(DashMsg::Flash(msg));
        });
    }

    /// Launch a simulator/emulator in the background, reporting via flash.
    fn boot_target(&mut self, target: BootTarget, label: String) {
        let tx = self.tx.clone();
        self.set_flash(format!("launching {label}…"));
        std::thread::spawn(move || {
            let res = match target {
                BootTarget::IosSim(udid) => ios::boot_simulator(&udid),
                BootTarget::Avd(name) => android::launch_avd(&name),
            };
            if let Err(e) = res {
                let _ = tx.send(DashMsg::Flash(format!("failed to launch {label}: {e}")));
            }
        });
    }

    /// Stop (kill) the process in the current Processes sub-tab.
    fn with_proc(&mut self, f: impl FnOnce(&mut PtyProcess)) {
        if let Some(p) = self.procs.get_mut(self.proc_sel) {
            f(p);
        }
    }

    /// Remove a finished process's tab. Running ones must be stopped (x) first.
    fn delete_selected_proc(&mut self) {
        let i = self.proc_sel;
        let Some(p) = self.procs.get(i) else {
            return;
        };
        if p.is_alive() {
            self.set_flash(format!("{} is still running — stop it first (x)", p.label));
            return;
        }
        self.advance_setup(); // settle setup steps before their tab indices shift
        self.finish_sim_builds();
        let label = self.procs.remove(i).label.clone();
        self.sim_builds.retain_mut(|(b, _, _)| {
            if *b > i {
                *b -= 1;
            }
            *b != i
        });
        if let Some(s) = self.setup.as_mut() {
            for idx in [&mut s.install, &mut s.boot, &mut s.build] {
                *idx = match *idx {
                    Some(m) if m > i => Some(m - 1),
                    other => other,
                };
            }
        }
        self.metro_idx = match self.metro_idx {
            Some(m) if m == i => {
                self.metro_external = false;
                None
            }
            Some(m) if m > i => Some(m - 1),
            other => other,
        };
        if self.proc_sel >= self.procs.len() {
            self.proc_sel = self.procs.len().saturating_sub(1);
        }
        self.set_flash(format!("removed {label}"));
    }

    fn stop_selected_proc(&mut self) {
        let msg = match self.procs.get_mut(self.proc_sel) {
            Some(p) => {
                let alive = p.is_alive();
                if alive {
                    p.kill();
                }
                let label = p.label.clone();
                if alive {
                    format!("stopped {label}")
                } else {
                    format!("{label} already stopped")
                }
            }
            None => return,
        };
        self.set_flash(msg);
    }

    /// `o`: launch the app on the selected (running) device, by its bundleId.
    fn open_selected(&mut self) {
        let dev = match self.devices.get(self.dev_sel) {
            Some(d) => d.clone(),
            None => return,
        };
        let target = match dev.open {
            Some(t) => t,
            None => {
                self.set_flash(format!("{} isn't running — start it first (⏎)", dev.label));
                return;
            }
        };
        let has_bundle = if dev.platform == Platform::Android {
            self.project.android_bundle_id().is_some()
        } else {
            self.project.ios_bundle_id().is_some()
        };
        if !has_bundle {
            let which = if dev.platform == Platform::Android {
                "android"
            } else {
                "ios"
            };
            self.set_flash(format!("set {which}.bundleId in rn.json to launch the app"));
            return;
        }
        self.run_on_device(target, None, dev.label);
    }

    /// `l` picker: open the chosen deep link on the selected (running) device.
    /// Open configured link `idx`, asking for its placeholders' values first.
    fn open_deeplink(&mut self, idx: usize) {
        let Some(link) = self.project.deeplinks.get(idx) else {
            return;
        };
        let names = crate::rnconfig::link_placeholders(link.url());
        if !names.is_empty() {
            let values = names
                .into_iter()
                .map(|n| {
                    (
                        n.clone(),
                        self.link_values.get(&n).cloned().unwrap_or_default(),
                    )
                })
                .collect();
            self.link_form = Some(LinkForm {
                title: link.label().to_string(),
                template: link.url().to_string(),
                values,
                field: 0,
            });
            return;
        }
        self.open_url(link.url().to_string());
    }

    /// Text pasted into the terminal: into the link form's current field.
    fn on_paste(&mut self, text: &str) {
        if let Some(form) = self.link_form.as_mut() {
            form.values[form.field].1.push_str(text.trim());
        } else if self.input_mode {
            if let Some(p) = self.procs.get_mut(self.proc_sel) {
                p.write_input(text.as_bytes());
            }
        }
    }

    fn open_url(&mut self, url: String) {
        let dev = match self.devices.get(self.dev_sel) {
            Some(d) => d.clone(),
            None => return,
        };
        match dev.open {
            Some(target) => self.run_on_device(target, Some(url), dev.label),
            None => self.set_flash(format!("{} isn't running — start it first (⏎)", dev.label)),
        }
    }

    /// Open a URL on a device, or launch its app when `url` is None. The link is
    /// routed straight to the app via bundleId where the platform supports it.
    fn run_on_device(&mut self, target: OpenTarget, url: Option<String>, label: String) {
        let tx = self.tx.clone();
        let ios_bundle = self.project.ios_bundle_id().map(String::from);
        let android_pkg = self.project.android_bundle_id().map(String::from);
        let verb = if url.is_some() {
            "opening"
        } else {
            "launching"
        };
        self.set_flash(format!("{verb} on {label}…"));
        #[cfg(test)]
        {
            let _ = (tx, ios_bundle, android_pkg, target);
            return self.opened.push((label, url));
        }
        #[cfg_attr(test, allow(unreachable_code))]
        std::thread::spawn(move || {
            let res = match (&url, target) {
                (Some(u), OpenTarget::IosSim(udid)) => ios::open_url_on_simulator(&udid, u),
                (Some(u), OpenTarget::IosPhysical(udid)) => {
                    ios::open_url_on_physical_ios(&udid, u, ios_bundle.as_deref(), false)
                }
                (Some(u), OpenTarget::AndroidSerial(serial)) => {
                    android::open_url_with_package(&serial, u, android_pkg.as_deref())
                }
                (None, OpenTarget::IosSim(udid)) => {
                    ios::launch_app_on_simulator(&udid, ios_bundle.as_deref().unwrap_or_default())
                }
                (None, OpenTarget::IosPhysical(udid)) => ios::launch_app_on_physical_ios(
                    &udid,
                    ios_bundle.as_deref().unwrap_or_default(),
                ),
                (None, OpenTarget::AndroidSerial(serial)) => {
                    android::launch_app(&serial, android_pkg.as_deref().unwrap_or_default())
                }
            };
            let msg = match res {
                Ok(()) => format!("done on {label}"),
                Err(e) => format!("failed on {label}: {e}"),
            };
            let _ = tx.send(DashMsg::Flash(msg));
        });
    }

    /// Send a single Metro interactive key (`r`/`d`/`j`…) to the Metro PTY, with a
    /// CDP reload fallback when Metro isn't running under us.
    fn metro_key(&mut self, byte: u8, what: &str) {
        if let Some(i) = self.metro_idx.filter(|_| !self.metro_external) {
            if let Some(p) = self.procs.get_mut(i) {
                if p.is_alive() {
                    p.write_input(&[byte]);
                    self.set_flash(format!("metro: {what}"));
                    return;
                }
            }
        }
        // A Metro we don't own: send the command through its events socket.
        let command = if byte == b'r' { "reload" } else { "devMenu" };
        if matches!(byte, b'r' | b'd')
            && metro_events::send_command(self.project.metro_port(), command)
        {
            self.set_flash(format!("metro: {what}"));
            return;
        }
        if byte == b'r' {
            if let Some(k) = self.rnview.active_target() {
                self.client.send(&k, ConnCmd::Reload);
                self.set_flash("reload (via CDP)");
                return;
            }
        }
        self.set_flash("Metro isn't running — press m to start it");
    }

    fn cycle_focus(&mut self, back: bool) {
        self.input_mode = false;
        self.focus = match (self.focus, back) {
            (Pane::Processes, false) => Pane::Devices,
            (Pane::Devices, false) => Pane::Logs,
            (Pane::Logs, false) => Pane::Processes,
            (Pane::Processes, true) => Pane::Logs,
            (Pane::Devices, true) => Pane::Processes,
            (Pane::Logs, true) => Pane::Devices,
        };
    }

    fn main_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            while let Ok(ev) = self.client.rx.try_recv() {
                self.rnview.on_event(ev);
            }
            while let Ok(m) = self.rx.try_recv() {
                match m {
                    DashMsg::Devices(v) => {
                        self.devices = v;
                        if self.dev_sel >= self.devices.len() {
                            self.dev_sel = self.devices.len().saturating_sub(1);
                        }
                    }
                    DashMsg::Flash(s) => self.set_flash(s),
                }
            }
            if let Some((_, t)) = &self.flash {
                if t.elapsed() > Duration::from_secs(4) {
                    self.flash = None;
                }
            }

            while let Ok(r) = self.ctl_rx.try_recv() {
                let reply = self.control(&r.cmd, &r.args);
                let _ = r.reply.send(reply);
            }
            self.advance_setup();
            self.finish_sim_builds();
            if self.term.load(Ordering::Relaxed) {
                // `signalbox down` / window closed: quit; `down` handles the simulator.
                return Ok(());
            }

            terminal.draw(|f| render(self, f))?;

            // Keep every PTY sized to the pane (no-op when unchanged).
            if let Some(r) = self.proc_area {
                for p in self.procs.iter_mut() {
                    p.resize(r.height, r.width);
                }
            }

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        crate::devlog::log(
            "dashboard",
            format!(
                "key {:?}{}",
                key.code,
                if key.modifiers.is_empty() {
                    String::new()
                } else {
                    format!(" {:?}", key.modifiers)
                }
            ),
        );
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);

        // Quit confirmation popup swallows all input until answered.
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('d') if self.ask_delete_sim() => self.request_quit(true),
                KeyCode::Char('y') | KeyCode::Char('k') | KeyCode::Enter => {
                    self.request_quit(false)
                }
                _ if ctrl_c => self.request_quit(false), // Ctrl-C again = force quit
                KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return;
        }

        // Deep-link quick-picker: a digit/letter opens that link; Esc closes.
        // Key reference popup: scrolls like any list; ?/Esc/q close it.
        if let Some(top) = self.help.as_mut() {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.help = None
                }
                KeyCode::Char('j') | KeyCode::Down => *top = top.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => *top = top.saturating_sub(1),
                KeyCode::Char('g') => *top = 0,
                KeyCode::Char('G') => *top = u16::MAX,
                _ if ctrl_c => self.help = None,
                _ => {}
            }
            return;
        }

        if let Some(form) = self.link_form.as_mut() {
            match key.code {
                KeyCode::Esc => self.link_form = None,
                _ if ctrl_c => self.link_form = None,
                KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % form.values.len(),
                KeyCode::BackTab | KeyCode::Up => {
                    form.field = (form.field + form.values.len() - 1) % form.values.len()
                }
                KeyCode::Backspace => {
                    form.values[form.field].1.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.values[form.field].1.clear()
                }
                KeyCode::Enter if form.field + 1 < form.values.len() => form.field += 1,
                KeyCode::Enter => {
                    let form = self.link_form.take().unwrap();
                    if form.values.iter().any(|(_, v)| v.trim().is_empty()) {
                        self.set_flash("fill in every value");
                        self.link_form = Some(form);
                        return;
                    }
                    let url = if form.template.is_empty() {
                        form.values[0].1.trim().to_string() // a pasted URL, as is
                    } else {
                        for (k, v) in &form.values {
                            self.link_values.insert(k.clone(), v.clone());
                        }
                        crate::rnconfig::fill_link(&form.template, &form.values)
                    };
                    self.open_url(url);
                }
                KeyCode::Char(c) => form.values[form.field].1.push(c),
                _ => {}
            }
            return;
        }

        if let (true, Some((query, sel))) = (self.link_picker, self.link_search.as_mut()) {
            let matches = link_matches(&self.project.deeplinks, query);
            match key.code {
                KeyCode::Esc => self.link_search = None,
                _ if ctrl_c => self.link_search = None,
                KeyCode::Down | KeyCode::Tab => {
                    *sel = (*sel + 1).min(matches.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::BackTab => *sel = sel.saturating_sub(1),
                KeyCode::Backspace if query.is_empty() => self.link_search = None,
                KeyCode::Backspace => {
                    query.pop();
                    *sel = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.clear();
                    *sel = 0;
                }
                KeyCode::Enter => {
                    if let Some(&i) = matches.get(*sel) {
                        self.link_search = None;
                        self.link_picker = false;
                        self.open_deeplink(i);
                    }
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    *sel = 0;
                }
                _ => {}
            }
            return;
        }

        if self.link_picker {
            if key.code == KeyCode::Esc {
                self.link_picker = false;
            } else if key.code == KeyCode::Char('/') {
                self.link_search = Some((String::new(), 0));
            } else if key.code == KeyCode::Char(':') {
                // Any URL, e.g. a magic login link pasted from an email.
                self.link_picker = false;
                self.link_form = Some(LinkForm {
                    title: "Open a URL".into(),
                    template: String::new(),
                    values: vec![("url".into(), String::new())],
                    field: 0,
                });
            } else if let KeyCode::Char(c) = key.code {
                if let Some(i) = picker_index(c) {
                    if i < self.project.deeplinks.len() {
                        self.link_picker = false;
                        self.open_deeplink(i);
                    }
                }
            }
            return;
        }

        // Processes input mode: forward raw keys to the active PTY (including
        // Ctrl-C, so you can interrupt Metro); only Esc leaves input mode.
        if self.input_mode {
            if key.code == KeyCode::Esc {
                self.input_mode = false;
                return;
            }
            if let Some(bytes) = key_to_bytes(key) {
                if let Some(p) = self.procs.get_mut(self.proc_sel) {
                    p.write_input(&bytes);
                }
            }
            return;
        }

        // Ctrl-C anywhere else asks before quitting.
        if ctrl_c {
            self.confirm_quit = true;
            return;
        }

        // Global keys — work from any pane. Suppressed only while the logs viewer
        // is capturing a search/filter query (so those chars reach the query).
        let logs_capturing = self.focus == Pane::Logs && self.rnview.is_capturing_input();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !logs_capturing {
            match key.code {
                // Ctrl+arrows resize the panes.
                KeyCode::Left if ctrl => {
                    return self.h_split = self.h_split.saturating_sub(5).max(20)
                }
                KeyCode::Right if ctrl => return self.h_split = (self.h_split + 5).min(80),
                KeyCode::Up if ctrl => {
                    return self.v_split = self.v_split.saturating_sub(5).max(20)
                }
                KeyCode::Down if ctrl => return self.v_split = (self.v_split + 5).min(80),
                KeyCode::Tab => return self.cycle_focus(false),
                KeyCode::BackTab => return self.cycle_focus(true),
                KeyCode::Char('q') => {
                    self.confirm_quit = true;
                    return;
                }
                KeyCode::Char('?') => {
                    self.help = Some(0);
                    return;
                }
                KeyCode::Char('R') => return self.metro_key(b'r', "reload"),
                KeyCode::Char('D') => return self.metro_key(b'd', "dev menu"),
                _ => {}
            }
        }

        // Pane-scoped keys: each pane owns its own, no cross-pane duplicates.
        match self.focus {
            Pane::Processes => self.processes_key(key),
            Pane::Devices => self.devices_key(key),
            Pane::Logs => {
                if self.rnview.on_key(key, &self.client) {
                    self.confirm_quit = true;
                }
            }
        }
    }

    fn processes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('[') => {
                if !self.procs.is_empty() {
                    self.proc_sel = (self.proc_sel + self.procs.len() - 1) % self.procs.len();
                }
            }
            KeyCode::Char(']') => {
                if !self.procs.is_empty() {
                    self.proc_sel = (self.proc_sel + 1) % self.procs.len();
                }
            }
            KeyCode::Enter if self.metro_external && self.metro_idx == Some(self.proc_sel) => {
                self.set_flash("external Metro takes no input here — m to take over");
            }
            KeyCode::Enter => {
                if self
                    .procs
                    .get(self.proc_sel)
                    .map(|p| p.is_alive())
                    .unwrap_or(false)
                {
                    self.input_mode = true;
                } else {
                    self.set_flash("no running process in this tab");
                }
            }
            // Scroll the output (each tab keeps its own position and autoscroll).
            KeyCode::Char('k') | KeyCode::Up => self.with_proc(|p| p.scroll_by(1)),
            KeyCode::Char('j') | KeyCode::Down => self.with_proc(|p| p.scroll_by(-1)),
            KeyCode::Char('g') => self.with_proc(|p| p.scroll_to_top()),
            KeyCode::Char('G') => self.with_proc(|p| p.scroll_to_bottom()),
            KeyCode::Char(' ') => self.with_proc(|p| p.toggle_follow()),
            KeyCode::Char('x') => self.stop_selected_proc(),
            KeyCode::Char('d') => self.delete_selected_proc(),
            KeyCode::Char('m') => self.start_metro(),
            KeyCode::Char('c') => self.deep_clean(),
            KeyCode::Char('i') => self.reinstall(),
            KeyCode::Char('a') => self.reset_project(),
            _ => {}
        }
    }

    fn devices_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.dev_sel = self.dev_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.devices.is_empty() {
                    self.dev_sel = (self.dev_sel + 1).min(self.devices.len() - 1);
                }
            }
            KeyCode::Char('g') => self.dev_sel = 0,
            KeyCode::Char('G') => self.dev_sel = self.devices.len().saturating_sub(1),
            KeyCode::Enter => self.start_selected_device(),
            KeyCode::Char('b') => self.install_selected(),
            KeyCode::Char('s') => self.stop_selected_device(),
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('l') => {
                if self
                    .devices
                    .get(self.dev_sel)
                    .and_then(|d| d.open.as_ref())
                    .is_none()
                {
                    self.set_flash("start the device first (⏎)");
                } else {
                    self.link_picker = true;
                }
            }
            _ => {}
        }
    }
}

/// Quick-pick key for item `i`: 1-9, then a-z, then A-Z once those run out.
fn picker_char(i: usize) -> Option<char> {
    if i < 9 {
        Some((b'1' + i as u8) as char)
    } else if i < 9 + 26 {
        Some((b'a' + (i - 9) as u8) as char)
    } else if i < 9 + 26 + 26 {
        Some((b'A' + (i - 9 - 26) as u8) as char)
    } else {
        None
    }
}

/// Indexes of the deep links whose name or URL contains `query` (any case).
fn link_matches(links: &[crate::rnconfig::DeepLink], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    links
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            d.label().to_lowercase().contains(&q) || d.url().to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Inverse of `picker_char`.
fn picker_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some(c as usize - '1' as usize),
        'a'..='z' => Some(9 + (c as usize - 'a' as usize)),
        'A'..='Z' => Some(9 + 26 + (c as usize - 'A' as usize)),
        _ => None,
    }
}

/// Translate a key event into the bytes a terminal would send to the child.
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() {
                vec![(c.to_ascii_lowercase() as u8) & 0x1f]
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

// ── rendering ────────────────────────────────────────────────────────────────

fn render(app: &mut DashApp, frame: &mut Frame) {
    let area = frame.area();
    let outer = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);
    let body = outer[0];
    let status_area = outer[1];

    let halves = Layout::vertical([
        Constraint::Percentage(app.v_split),
        Constraint::Percentage(100 - app.v_split),
    ])
    .split(body);
    let top = Layout::horizontal([
        Constraint::Percentage(app.h_split),
        Constraint::Percentage(100 - app.h_split),
    ])
    .split(halves[0]);
    let proc_outer = top[0];
    let dev_outer = top[1];
    let logs_outer = halves[1];

    render_processes(app, frame, proc_outer);
    render_devices(app, frame, dev_outer);
    render_logs(app, frame, logs_outer);
    render_status(app, frame, status_area);

    if app.link_picker {
        render_link_picker(app, frame, area);
    }
    if let Some(form) = &app.link_form {
        render_link_form(form, frame, area);
    }
    if app.help.is_some() {
        render_help(app, frame, area);
    }
    if app.confirm_quit {
        let sim = app.ask_delete_sim().then(|| {
            app.devices
                .iter()
                .find(|d| d.pinned)
                .map(|d| d.label.clone())
                .unwrap_or_else(|| "the simulator".into())
        });
        render_quit_popup(frame, area, sim);
    }
}

/// Values for a link's placeholders (`template` set), or one pasted URL.
struct LinkForm {
    title: String,
    template: String,
    values: Vec<(String, String)>,
    field: usize,
}

fn render_link_form(form: &LinkForm, frame: &mut Frame, area: Rect) {
    let w = 76.min(area.width);
    let h = (form.values.len() as u16 * 2 + 5).min(area.height);
    let r = centered(area, w, h);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" {} ", form.title));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let mut lines = vec![Line::raw("")];
    // Names left-aligned in a column as wide as the longest, values lined up after it.
    let name_w = form
        .values
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let room = inner.width.saturating_sub(name_w as u16 + 4) as usize;
    for (i, (name, value)) in form.values.iter().enumerate() {
        let active = i == form.field;
        // Long values (tokens, magic links): show the end, where typing happens.
        let chars: Vec<char> = value.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {name:<name_w$}  "),
                if active {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(if active { format!("{shown}█") } else { shown }),
        ]));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        " type or paste · ⇥ next · ⏎ open · esc cancel",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_link_picker(app: &DashApp, frame: &mut Frame, area: Rect) {
    let links = &app.project.deeplinks;
    let search = app.link_search.as_ref();
    let shown: Vec<usize> = match search {
        Some((q, _)) => link_matches(links, q),
        None => (0..links.len())
            .filter(|&i| picker_char(i).is_some())
            .collect(),
    };
    let key_style = Style::default().fg(Color::Black).bg(Color::Cyan);

    // The list itself, with a heading above each run of links in one group.
    let mut body: Vec<Line> = Vec::new();
    let mut heading_rows: Vec<usize> = Vec::new();
    let mut group: Option<&str> = None;
    for (row, &i) in shown.iter().enumerate() {
        let d = &links[i];
        if d.group().is_some() && d.group() != group {
            if !body.is_empty() {
                body.push(Line::raw(""));
            }
            heading_rows.push(body.len());
            body.push(Line::styled(
                format!(" {}", d.group().unwrap()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        group = d.group();
        // While searching, keys are typed into the query: ⏎ opens the highlighted match.
        let (key, label) = match search {
            Some((_, sel)) if *sel == row => (
                "  ▸".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Some(_) => ("   ".to_string(), Style::default()),
            None => (
                format!(" {} ", picker_char(i).unwrap_or(' ')),
                Style::default(),
            ),
        };
        body.push(Line::from(vec![
            Span::styled(
                key,
                if search.is_some() {
                    Style::default().fg(Color::Cyan)
                } else {
                    key_style
                },
            ),
            Span::styled(format!("  {}", d.label()), label),
            Span::styled(
                crate::rnconfig::link_placeholders(d.url())
                    .iter()
                    .map(|p| format!(" ‹{p}›"))
                    .collect::<String>(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let mut head: Vec<Line> = Vec::new();
    if let Some((q, _)) = search {
        head.push(Line::from(vec![
            Span::styled(" / ", key_style),
            Span::raw(format!("  {q}█")),
        ]));
        head.push(Line::raw(""));
    }
    let foot: Vec<Line> = vec![
        Line::raw(""),
        if search.is_some() {
            Line::styled(
                " type to filter · ↑↓ select · ⏎ open · esc back",
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Line::from(vec![
                Span::styled(" / ", key_style),
                Span::raw("  search    "),
                Span::styled(" : ", key_style),
                Span::raw("  paste any URL (e.g. a magic login link)"),
            ])
        },
    ];

    // Too tall for the terminal: split into two columns, at a group heading near the middle.
    let chrome = (head.len() + foot.len() + 2) as u16;
    let split = if body.len() as u16 + chrome > area.height && area.width >= 128 {
        let mid = body.len().div_ceil(2);
        let at = heading_rows
            .iter()
            .copied()
            .filter(|&r| r > 0)
            .min_by_key(|&r| r.abs_diff(mid))
            .filter(|&r| r.abs_diff(mid) <= mid / 3)
            .unwrap_or(mid);
        Some(at)
    } else {
        None
    };
    let (rows, w) = match split {
        Some(at) => (at.max(body.len() - at), 128),
        None => (body.len(), 64),
    };
    let r = centered(area, w, (rows as u16 + chrome).clamp(6, area.height));
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Deep links · esc ");
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let [head_area, body_area, foot_area] = Layout::vertical([
        Constraint::Length(head.len() as u16),
        Constraint::Min(0),
        Constraint::Length(foot.len() as u16),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(Text::from(head)), head_area);
    frame.render_widget(Paragraph::new(Text::from(foot)), foot_area);
    match split {
        Some(at) => {
            let right: Vec<Line> = body
                .split_off(at)
                .into_iter()
                .skip_while(|l| l.width() == 0)
                .collect();
            let [left_area, right_area] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body_area);
            frame.render_widget(Paragraph::new(Text::from(body)), left_area);
            frame.render_widget(Paragraph::new(Text::from(right)), right_area);
        }
        None => frame.render_widget(Paragraph::new(Text::from(body)), body_area),
    }
}

type KeySection = (&'static str, &'static [(&'static str, &'static str)]);

// Left / right columns of the `?` popup (stacked when the terminal is narrow).
const HELP_LEFT: &[KeySection] = &[
    (
        "Global",
        &[
            ("⇥ / ⇧⇥", "next / previous pane"),
            ("^←→↑↓", "resize panes"),
            ("R", "reload the app"),
            ("D", "open the dev menu"),
            ("?", "this help"),
            ("q / ^C", "quit"),
        ],
    ),
    (
        "Every pane",
        &[
            ("j k  ↑ ↓", "move / scroll"),
            ("g  G", "top / bottom"),
            ("space", "autoscroll on/off"),
            ("[  ]", "previous / next tab"),
        ],
    ),
    (
        "Processes",
        &[
            ("⏎", "type into the process (Esc leaves)"),
            ("x", "stop it"),
            ("d", "delete a stopped tab"),
            ("m", "start / restart Metro (takes over an external one)"),
            ("c", "deep clean (reinstalls)"),
            ("i", "reinstall deps + pods"),
            ("a", "reset: deep clean, then Metro"),
        ],
    ),
    (
        "Devices",
        &[
            ("⏎", "start the simulator / emulator"),
            ("s", "stop it"),
            ("b", "build & run on it"),
            ("o", "open the app"),
            (
                "l",
                "open a deep link (fills ‹placeholders›; / search, : any URL)",
            ),
        ],
    ),
];

const HELP_RIGHT: &[KeySection] = &[
    (
        "Logs · Network · Perf",
        &[
            ("1-9", "switch device"),
            ("/  n N", "search, next / previous match"),
            ("f", "filter"),
            ("⏎", "open the entry"),
            ("z", "open maximized / toggle split"),
            ("V", "visual select, then y"),
            ("y", "copy"),
            ("c", "clear"),
            ("p", "clear on reload on/off"),
        ],
    ),
    (
        "Open entry",
        &[
            ("⏎ / Esc", "close"),
            ("J K", "scroll the entry (split view)"),
            ("{  }", "previous / next section"),
            ("o", "open file:line in nvim"),
            ("F", "show framework stack frames"),
        ],
    ),
    (
        "Network",
        &[
            ("e", "errors only"),
            ("m", "cycle method filter"),
            ("c / C", "copy as curl"),
        ],
    ),
    (
        "Debugger (paused)",
        &[
            ("F5", "continue"),
            ("F10", "step over"),
            ("F11 / ⇧F11", "step into / out"),
            ("F6", "pause"),
        ],
    ),
];

fn help_lines(sections: &[KeySection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (title, keys)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            format!(" {title}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        for (k, desc) in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("   {k:<12}"), Style::default().fg(Color::Yellow)),
                Span::raw(*desc),
            ]));
        }
    }
    lines
}

fn render_help(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let (left, right) = (help_lines(HELP_LEFT), help_lines(HELP_RIGHT));
    let two_cols = area.width >= 110;
    let content_h = if two_cols {
        left.len().max(right.len())
    } else {
        left.len() + 1 + right.len()
    } as u16;
    let w = if two_cols { 120 } else { 64 };
    let r = centered(area, w, content_h + 2);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Keys ")
        .title_bottom(Line::from(" ? / esc close ").right_aligned());
    let inner = block.inner(r);
    frame.render_widget(block, r);
    // Clamp the scroll so G lands on the last page.
    let top = app
        .help
        .unwrap_or(0)
        .min(content_h.saturating_sub(inner.height));
    app.help = Some(top);
    if two_cols {
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        frame.render_widget(Paragraph::new(Text::from(left)).scroll((top, 0)), cols[0]);
        frame.render_widget(Paragraph::new(Text::from(right)).scroll((top, 0)), cols[1]);
    } else {
        let mut all = left;
        all.push(Line::raw(""));
        all.extend(right);
        frame.render_widget(Paragraph::new(Text::from(all)).scroll((top, 0)), inner);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn render_quit_popup(frame: &mut Frame, area: Rect, created_sim: Option<String>) {
    if let Some(sim) = created_sim {
        let r = centered(area, 64, 6);
        frame.render_widget(Clear, r);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Quit signalbox? ");
        let inner = block.inner(r);
        frame.render_widget(block, r);
        let lines = vec![
            Line::raw(""),
            Line::from(Span::raw("  This stops Metro and any running builds.")),
            Line::from(Span::raw(format!("  This session created {sim}."))),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("d", Style::default().fg(Color::Red)),
                Span::raw(" quit, delete it  "),
                Span::styled("k/⏎", Style::default().fg(Color::Green)),
                Span::raw(" quit, keep it  "),
                Span::styled("esc", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel"),
            ]),
        ];
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        return;
    }
    let r = centered(area, 50, 5);
    frame.render_widget(Clear, r); // wipe whatever's underneath
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Quit signalbox? ");
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let lines = vec![
        Line::raw(""),
        Line::from(Span::raw("  This stops Metro and any running builds.")),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y/⏎", Style::default().fg(Color::Green)),
            Span::raw(" quit    "),
            Span::styled("n/esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel"),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn focus_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// The footer bar style, matching the embedded logs viewer's footer.
fn bar_style() -> Style {
    Style::default().bg(Color::Rgb(59, 66, 82)).fg(Color::White)
}

/// Split a pane's inner area into a content area and a one-line footer for that
/// pane's own key hints (when there's room).
fn split_hint(inner: Rect) -> (Rect, Option<Rect>) {
    if inner.height >= 3 {
        let v = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        (v[0], Some(v[1]))
    } else {
        (inner, None)
    }
}

/// A pane's key-hint footer: a full-width bar (matching the logs footer) when the
/// pane is focused, blank otherwise — so only the active pane shows its keys.
fn render_hint(frame: &mut Frame, area: Rect, text: &str, focused: bool) {
    let w = area.width as usize;
    let (content, style) = if focused {
        (text.to_string(), bar_style())
    } else {
        (String::new(), Style::default())
    };
    let n = content.chars().count();
    let padded = if n >= w {
        content.chars().take(w).collect::<String>()
    } else {
        format!("{content}{}", " ".repeat(w - n))
    };
    frame.render_widget(Paragraph::new(padded).style(style), area);
}

fn render_processes(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Processes;
    let tabs: String = if app.procs.is_empty() {
        "no processes".into()
    } else {
        app.procs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mark = if p.is_alive() {
                    String::new()
                } else {
                    match p.exit_code() {
                        Some(0) => " ✓".into(),
                        Some(c) => format!(" ✗{c}"),
                        None => " (exited)".into(),
                    }
                };
                if i == app.proc_sel {
                    format!("[{}{}]", p.label, mark)
                } else {
                    format!(" {}{} ", p.label, mark)
                }
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let title = if app.input_mode && focused {
        format!(" Processes  {tabs}  — INPUT (Esc to exit) ")
    } else {
        format!(" Processes  {tabs} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (content, hint) = split_hint(inner);
    // Reserve the top line of the pane for the command that produced this output.
    let (cmd_area, body) = if content.height >= 2 {
        let cmd_area = Rect {
            x: content.x,
            y: content.y,
            width: content.width,
            height: 1,
        };
        let body = Rect {
            x: content.x,
            y: content.y + 1,
            width: content.width,
            height: content.height - 1,
        };
        (Some(cmd_area), body)
    } else {
        (None, content)
    };
    app.proc_area = Some(body);

    match app.procs.get_mut(app.proc_sel) {
        // Keep showing the output whether the process is running or finished —
        // the tab label carries the ✓ / ✗exit status. This way a clean/install
        // that failed still shows WHY on screen.
        Some(p) => {
            p.sync_scroll();
            if let Some(ca) = cmd_area {
                let off = p.scroll_offset();
                let marker = match (p.follow, off) {
                    (true, _) => String::new(),
                    (false, 0) => "  [paused]".into(),
                    (false, n) => format!("  [paused ↑{n}]"),
                };
                let head = format!("$ {}{marker}", p.cmd);
                frame.render_widget(
                    Paragraph::new(head).style(Style::default().fg(Color::DarkGray)),
                    ca,
                );
            }
            let lines = pty_lines(&p.parser(), body.width, body.height);
            frame.render_widget(Paragraph::new(Text::from(lines)), body);
        }
        None => {
            let msg = Paragraph::new("Press  m  to start Metro.")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, body);
        }
    }
    if let Some(h) = hint {
        render_hint(
            frame,
            h,
            " ⏎ type · x stop · d delete · m metro · c clean · i install · a reset",
            focused,
        );
    }
}

fn render_devices(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Devices;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(" Devices & Actions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (content, hint) = split_hint(inner);

    let metro_up = app
        .metro_idx
        .and_then(|i| app.procs.get(i))
        .map(|p| p.is_alive())
        .unwrap_or(false);
    let metro_line = if metro_up {
        Line::from(vec![
            Span::styled("Metro  ● ", Style::default().fg(Color::Green)),
            Span::raw(format!(
                "running :{}{}",
                app.project.metro_port(),
                if app.metro_external {
                    " (external)"
                } else {
                    ""
                }
            )),
        ])
    } else {
        Line::from(vec![
            Span::styled("Metro  ○ ", Style::default().fg(Color::DarkGray)),
            Span::raw("stopped  (m)"),
        ])
    };

    let mut lines: Vec<Line> = vec![metro_line, Line::raw("")];
    if app.devices.is_empty() {
        lines.push(Line::styled(
            "  (no simulators/emulators found)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let mut group = None;
    let mut sel_line = 0;
    for (i, d) in app.devices.iter().enumerate() {
        if group != Some(d.group()) {
            group = Some(d.group());
            if i > 0 {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                format!(" {}", d.group().1),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let marker = if d.running {
            Span::styled("● ", Style::default().fg(Color::Green))
        } else {
            Span::styled("○ ", Style::default().fg(Color::DarkGray))
        };
        if i == app.dev_sel {
            sel_line = lines.len();
        }
        let name_style = if focused && i == app.dev_sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::raw(" "),
            marker,
            Span::styled(d.label.clone(), name_style),
        ];
        // The highlighted device's id, for `--device` in signalbox, boop and simon.
        if focused && i == app.dev_sel && d.id != d.label {
            spans.push(Span::styled(
                format!("  {}", d.id),
                Style::default().fg(Color::DarkGray),
            ));
        }
        // App-presence tags (only meaningful with a configured bundleId).
        match d.installed {
            Some(true) => spans.push(Span::styled("  app✓", Style::default().fg(Color::Green))),
            Some(false) => spans.push(Span::styled("  app✗", Style::default().fg(Color::DarkGray))),
            None => {}
        }
        if d.foreground == Some(true) {
            spans.push(Span::styled("  ▶fg", Style::default().fg(Color::Cyan)));
        }
        if let Some(t) = &d.detail {
            spans.push(Span::styled(
                format!("  {t}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(p) = &d.problem {
            spans.push(Span::styled(
                format!("  ⚠ {p}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(spans));
    }
    // Scroll just enough to keep the selected row on screen.
    let h = content.height as usize;
    let top = (sel_line + 1).saturating_sub(h) as u16;
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((top, 0)), content);
    if let Some(h) = hint {
        // Start/stop boot or shut down a simulator/emulator, so only offer the
        // one that applies to the highlighted row (neither for a real device).
        let sim = app.devices.get(app.dev_sel).filter(|d| d.boot.is_some());
        let power = match sim {
            Some(d) if d.running => "s stop · ",
            Some(_) => "⏎ start · ",
            None => "",
        };
        render_hint(
            frame,
            h,
            &format!(" {power}b build · o open · l links"),
            focused,
        );
    }
}

fn render_logs(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Logs;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(" JS Logs / Network / Perf ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.rnview.set_focused(focused); // dim its footer when another pane is active
    app.rnview.render(frame, inner);
}

fn render_status(app: &DashApp, frame: &mut Frame, area: Rect) {
    // A suspended VM takes over the status bar (red) so it's unmissable.
    let paused = app.rnview.active_paused();
    let style = if paused.is_some() {
        Style::default()
            .bg(Color::Rgb(191, 97, 106))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::Rgb(59, 66, 82)).fg(Color::White)
    };
    // Global keys only — each pane shows its own keys in its footer.
    let text: String = if let Some(p) = &paused {
        format!(" {p} — focus the Logs pane, then F5 continue · F10 over · F11 into")
    } else if let Some((f, _)) = &app.flash {
        format!(" {f}")
    } else if app.input_mode {
        " INPUT — keys go to the process · Esc to exit".into()
    } else if let Some(e) = app
        .client
        .dap_error
        .as_ref()
        .filter(|_| app.started.elapsed() < Duration::from_secs(10))
    {
        format!(" ⚠ {e}")
    } else {
        " ⇥ focus · ^←→↑↓ resize · R reload · D dev-menu · ? keys · q quit".into()
    };
    let w = area.width as usize;
    let padded = {
        let n = text.chars().count();
        if n >= w {
            text.chars().take(w).collect::<String>()
        } else {
            format!("{text}{}", " ".repeat(w - n))
        }
    };
    frame.render_widget(Paragraph::new(padded).style(style), area);
}

fn pty_lines(
    parser: &std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let guard = match parser.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let screen = guard.screen();
    let (srows, scols) = screen.size();
    let h = height.min(srows);
    let w = width.min(scols);
    let mut lines: Vec<Line> = Vec::with_capacity(height as usize);
    for row in 0..h {
        let mut spans: Vec<Span> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::default();
        for col in 0..w {
            let (glyph, style) = match screen.cell(row, col) {
                Some(cell) => {
                    let g = cell.contents();
                    (
                        if g.is_empty() { " ".to_string() } else { g },
                        cell_style(cell),
                    )
                }
                None => (" ".to_string(), Style::default()),
            };
            if style != run_style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push_str(&glyph);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    while lines.len() < height as usize {
        lines.push(Line::raw(""));
    }
    lines
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default();
    if let Some(fg) = conv_color(cell.fgcolor()) {
        s = s.fg(fg);
    }
    if let Some(bg) = conv_color(cell.bgcolor()) {
        s = s.bg(bg);
    }
    if cell.bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn conv_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    const LINKS: &str = r#"[
        {"name": "Home", "url": "app://home", "group": "Main"},
        {"name": "Settings", "url": "app://settings", "group": "Main"},
        {"name": "Order", "url": "app://order/<orderId>", "group": "Orders"},
        {"name": "Track order", "url": "app://order/{orderId}/track?driver=<driver>", "group": "Orders"},
        "app://plain"
    ]"#;

    fn project(links: &str) -> ProjectConfig {
        let json = format!(r#"{{"name": "App", "root": "/tmp/app", "deeplinks": {links}}}"#);
        serde_json::from_str(&json).unwrap()
    }

    fn app_with(project: ProjectConfig, pinned: Option<Pinned>) -> DashApp {
        let (tx, rx) = std::sync::mpsc::channel();
        DashApp::bare(project, pinned, RnClient::idle(), tx, rx)
    }

    /// The dashboard with LINKS, a booted simulator selected in the Devices pane.
    fn app() -> DashApp {
        let mut app = app_with(project(LINKS), None);
        app.devices = vec![
            sim("iPhone 16  iOS 18.0", true),
            sim("iPad Air  iOS 18.0", false),
        ];
        app.focus = Pane::Devices;
        app
    }

    fn sim(label: &str, running: bool) -> DeviceRow {
        let udid = format!("UDID-{}", label.len());
        DeviceRow {
            label: label.into(),
            id: udid.clone(),
            platform: Platform::Ios,
            running,
            boot: Some(BootTarget::IosSim(udid.clone())),
            open: running.then_some(OpenTarget::IosSim(udid)),
            installed: None,
            foreground: None,
            detail: None,
            problem: None,
            pinned: false,
        }
    }

    fn press(app: &mut DashApp, keys: &str) {
        for c in keys.chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
    }

    fn key(app: &mut DashApp, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn ctrl(app: &mut DashApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::CONTROL));
    }

    fn opened(app: &DashApp) -> Vec<Option<String>> {
        app.opened.iter().map(|(_, u)| u.clone()).collect()
    }

    fn flash(app: &DashApp) -> Option<&str> {
        app.flash.as_ref().map(|(f, _)| f.as_str())
    }

    fn screen(app: &mut DashApp, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(app, f)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                let row: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    /// Compare the rendered screen to `src/rndash/snapshots/<name>.txt`
    /// (UPDATE_SNAPSHOTS=1 rewrites it).
    fn snapshot(name: &str, app: &mut DashApp, w: u16, h: u16) {
        let got = screen(app, w, h);
        let path = format!(
            "{}/src/rndash/snapshots/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        );
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(&path, &got).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing {path}; run with UPDATE_SNAPSHOTS=1"));
        assert!(got == want, "{name} changed:\n{got}");
    }

    #[test]
    fn dashboard_layout() {
        let mut app = app();
        app.focus = Pane::Processes;
        snapshot("dashboard", &mut app, 100, 30);
        key(&mut app, KeyCode::Tab);
        snapshot("dashboard_devices_focused", &mut app, 100, 30);
    }

    #[test]
    fn ctrl_arrows_resize_the_panes() {
        let mut app = app();
        ctrl(&mut app, KeyCode::Right);
        ctrl(&mut app, KeyCode::Up);
        snapshot("dashboard_resized", &mut app, 100, 30);
        for _ in 0..20 {
            ctrl(&mut app, KeyCode::Left);
            ctrl(&mut app, KeyCode::Down);
        }
        assert_eq!((app.h_split, app.v_split), (20, 80));
    }

    #[test]
    fn tab_cycles_the_panes() {
        let mut app = app();
        app.focus = Pane::Processes;
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        assert!(app.focus == Pane::Logs);
        key(&mut app, KeyCode::Tab);
        assert!(app.focus == Pane::Processes);
        key(&mut app, KeyCode::BackTab);
        assert!(app.focus == Pane::Logs);
    }

    #[test]
    fn links_need_a_running_device() {
        let mut app = app();
        key(&mut app, KeyCode::Down); // the iPad, not booted
        press(&mut app, "l");
        assert_eq!(flash(&app), Some("start the device first (⏎)"));
        assert!(!screen(&mut app, 100, 30).contains("Deep links"));
    }

    #[test]
    fn picker_lists_links_under_their_groups() {
        let mut app = app();
        press(&mut app, "l");
        snapshot("link_picker", &mut app, 100, 30);
    }

    #[test]
    fn picker_key_opens_that_link() {
        let mut app = app();
        press(&mut app, "l2");
        assert_eq!(opened(&app), vec![Some("app://settings".to_string())]);
        assert_eq!(app.opened[0].0, "iPhone 16  iOS 18.0");
        assert_eq!(flash(&app), Some("opening on iPhone 16  iOS 18.0…"));
        assert!(!screen(&mut app, 100, 30).contains("Deep links"));
    }

    #[test]
    fn picker_ignores_keys_past_the_last_link() {
        let mut app = app();
        press(&mut app, "l9x");
        assert!(opened(&app).is_empty());
        key(&mut app, KeyCode::Esc);
        assert!(!screen(&mut app, 100, 30).contains("Deep links"));
        press(&mut app, "5"); // picker closed: nothing opens
        assert!(opened(&app).is_empty());
    }

    #[test]
    fn picker_keys_run_from_digits_to_letters_to_capitals() {
        let links: Vec<String> = (0..64).map(|i| format!("\"app://l/{i}\"")).collect();
        let mut app = app_with(project(&format!("[{}]", links.join(","))), None);
        app.devices = vec![sim("iPhone 16  iOS 18.0", true)];
        app.focus = Pane::Devices;
        for (keys, want) in [
            ("l1", 0),
            ("l9", 8),
            ("la", 9),
            ("lz", 34),
            ("lA", 35),
            ("lZ", 60),
        ] {
            press(&mut app, keys);
            assert_eq!(
                opened(&app).last().unwrap().as_deref(),
                Some(format!("app://l/{want}").as_str()),
                "{keys}"
            );
        }
        // Links past Z have no key: only search reaches them.
        press(&mut app, "l");
        let s = screen(&mut app, 100, 80);
        assert!(s.contains("app://l/60") && !s.contains("app://l/61"), "{s}");
        press(&mut app, "/l/63");
        key(&mut app, KeyCode::Enter);
        assert_eq!(opened(&app).last().unwrap().as_deref(), Some("app://l/63"));
    }

    #[test]
    fn tall_picker_splits_into_two_columns_at_a_heading() {
        let mut links = Vec::new();
        for g in ["Auth", "Shop", "Account"] {
            for i in 0..6 {
                links.push(format!(
                    r#"{{"name": "{g} {i}", "url": "app://{g}/{i}", "group": "{g}"}}"#
                ));
            }
        }
        let mut app = app_with(project(&format!("[{}]", links.join(","))), None);
        app.devices = vec![sim("iPhone 16  iOS 18.0", true)];
        app.focus = Pane::Devices;
        press(&mut app, "l");
        snapshot("link_picker_two_columns", &mut app, 140, 24);
        // Narrow terminals keep one (clipped) column.
        snapshot("link_picker_narrow", &mut app, 100, 24);
    }

    #[test]
    fn search_filters_by_name_or_url() {
        let mut app = app();
        press(&mut app, "l/ORD");
        snapshot("link_search", &mut app, 100, 30);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down); // stays on the last match
        snapshot("link_search_second", &mut app, 100, 30);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        press(&mut app, "x"); // no matches: Enter does nothing
        key(&mut app, KeyCode::Enter);
        assert!(opened(&app).is_empty());
        assert!(screen(&mut app, 100, 30).contains("/   ORDx█"));
        ctrl(&mut app, KeyCode::Char('u'));
        press(&mut app, "plain");
        key(&mut app, KeyCode::Enter);
        assert_eq!(opened(&app), vec![Some("app://plain".to_string())]);
    }

    #[test]
    fn backspace_and_esc_leave_search_then_the_picker() {
        let mut app = app();
        press(&mut app, "l/s");
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace); // empty query: back to the plain picker
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("Deep links") && s.contains(" /   search"), "{s}");
        press(&mut app, "/");
        key(&mut app, KeyCode::Esc);
        assert!(screen(&mut app, 100, 30).contains(" /   search"));
        key(&mut app, KeyCode::Esc);
        assert!(!screen(&mut app, 100, 30).contains("Deep links"));
    }

    #[test]
    fn placeholders_are_asked_for_then_filled_in() {
        let mut app = app();
        press(&mut app, "l4");
        assert!(opened(&app).is_empty());
        snapshot("link_form", &mut app, 100, 30);
        press(&mut app, "4 2");
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        press(&mut app, "2");
        key(&mut app, KeyCode::Enter); // next field
        key(&mut app, KeyCode::Enter); // empty value: refused
        assert_eq!(flash(&app), Some("fill in every value"));
        press(&mut app, "ana b");
        snapshot("link_form_filled", &mut app, 100, 30);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            opened(&app),
            vec![Some("app://order/42/track?driver=ana%20b".to_string())]
        );
        // Values are remembered per placeholder name.
        press(&mut app, "l3");
        key(&mut app, KeyCode::Enter);
        assert_eq!(opened(&app)[1].as_deref(), Some("app://order/42"));
    }

    #[test]
    fn form_fields_cycle_and_clear() {
        let mut app = app();
        press(&mut app, "l4");
        press(&mut app, "1");
        key(&mut app, KeyCode::Tab);
        press(&mut app, "x");
        key(&mut app, KeyCode::Tab); // wraps to the first field
        ctrl(&mut app, KeyCode::Char('u'));
        press(&mut app, "7");
        key(&mut app, KeyCode::BackTab); // wraps to the last
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            opened(&app),
            vec![Some("app://order/7/track?driver=x".into())]
        );
        press(&mut app, "l4");
        key(&mut app, KeyCode::Esc);
        assert!(!screen(&mut app, 100, 30).contains("Track order") && opened(&app).len() == 1);
    }

    #[test]
    fn colon_opens_a_pasted_url_as_is() {
        let mut app = app();
        press(&mut app, "l:");
        app.on_paste("  https://auth.test/magic?token=a b&x=1\n");
        snapshot("link_paste_url", &mut app, 100, 30);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            opened(&app),
            vec![Some("https://auth.test/magic?token=a b&x=1".into())]
        );
    }

    #[test]
    fn long_form_values_show_their_end() {
        let mut app = app();
        press(&mut app, "l:");
        app.on_paste(&format!("https://x.test/{}end", "a".repeat(120)));
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("aaaend█"), "{s}");
    }

    #[test]
    fn link_opens_on_a_stopped_device_flash() {
        let mut app = app();
        press(&mut app, "l");
        app.devices[0].open = None; // shut down while the picker was open
        press(&mut app, "1");
        assert!(opened(&app).is_empty());
        assert_eq!(
            flash(&app),
            Some("iPhone 16  iOS 18.0 isn't running — start it first (⏎)")
        );
    }

    #[test]
    fn help_popup_scrolls_and_clamps() {
        let mut app = app();
        press(&mut app, "?");
        snapshot("help_wide", &mut app, 130, 40);
        snapshot("help_narrow", &mut app, 80, 30);
        press(&mut app, "G");
        snapshot("help_narrow_bottom", &mut app, 80, 30);
        press(&mut app, "kkg");
        assert_eq!(app.help, Some(0));
        press(&mut app, "jj");
        assert_eq!(app.help, Some(2));
        press(&mut app, "q");
        assert!(app.help.is_none() && !app.confirm_quit);
    }

    #[test]
    fn quit_asks_first() {
        let mut app = app();
        press(&mut app, "q");
        snapshot("quit", &mut app, 100, 30);
        press(&mut app, "n");
        assert!(!app.confirm_quit && !app.quit);
        ctrl(&mut app, KeyCode::Char('c'));
        assert!(app.confirm_quit);
        key(&mut app, KeyCode::Enter);
        assert!(app.quit && !app.delete_sim_on_quit);
    }

    #[test]
    fn quit_offers_to_delete_a_created_simulator() {
        let pinned = Pinned {
            udid: "UDID-19".into(),
            created: true,
            simulator: true,
            android: false,
            cleanup: SimCleanup::Ask,
            name: None,
        };
        let mut app = app_with(project(LINKS), Some(pinned));
        let mut row = sim("iPhone 16  iOS 18.0", true);
        row.pinned = true;
        app.devices = vec![row];
        press(&mut app, "q");
        snapshot("quit_created_sim", &mut app, 100, 30);
        press(&mut app, "d");
        assert!(app.quit && app.delete_sim_on_quit);
    }

    #[test]
    fn enter_on_an_empty_process_tab_flashes() {
        let mut app = app();
        app.focus = Pane::Processes;
        key(&mut app, KeyCode::Enter);
        assert_eq!(flash(&app), Some("no running process in this tab"));
        assert!(!app.input_mode);
    }

    #[test]
    fn keys_become_terminal_bytes() {
        let k = |code, m| key_to_bytes(KeyEvent::new(code, m));
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(vec![3]));
        assert_eq!(k(KeyCode::Char('C'), KeyModifiers::CONTROL), Some(vec![3]));
        assert_eq!(k(KeyCode::Char('é'), KeyModifiers::NONE), Some("é".into()));
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), Some(vec![b'\r']));
        assert_eq!(k(KeyCode::Backspace, KeyModifiers::NONE), Some(vec![0x7f]));
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            k(KeyCode::Delete, KeyModifiers::NONE),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(k(KeyCode::F(1), KeyModifiers::NONE), None);
    }

    #[test]
    fn picker_keys_round_trip() {
        for i in 0..61 {
            assert_eq!(picker_index(picker_char(i).unwrap()), Some(i));
        }
        assert_eq!(picker_char(61), None);
        assert_eq!(picker_index('0'), None);
    }

    #[test]
    fn pty_screen_keeps_colors_and_attributes() {
        let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(3, 10, 0)));
        parser
            .lock()
            .unwrap()
            .process(b"\x1b[1;31mred\x1b[0m ok\r\n\x1b[38;2;1;2;3;4mrgb");
        let lines = pty_lines(&parser, 8, 4);
        assert_eq!(lines.len(), 4);
        let first = &lines[0].spans;
        assert_eq!(first[0].content, "red");
        assert_eq!(
            first[0].style,
            Style::default()
                .fg(Color::Indexed(1))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(first[1].content, " ok  ");
        assert_eq!(first[1].style, Style::default());
        assert_eq!(lines[1].spans[0].content, "rgb");
        assert_eq!(
            lines[1].spans[0].style,
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .add_modifier(Modifier::UNDERLINED)
        );
        assert_eq!(lines[3].width(), 0);
    }
}
