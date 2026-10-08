//! `metroctl` — the tiled React Native dashboard. One window: a Processes pane
//! (Metro + install/run, each in a PTY), a Devices & Actions pane (launch
//! sims/emulators, install & run, open links, app-presence), and the embedded
//! `RnView` (JS logs / network / perf). Keys are pane-scoped — each pane owns its
//! own, shown in its footer — except a few globals (focus, `R`/`D` Metro
//! reload/dev-menu, quit). The Metro PTY takes raw-key passthrough in input mode,
//! so `r`/`d`/`j` and anything else Metro supports work as in a normal terminal.

use crate::proc::PtyProcess;
use crate::rnclient::{ConnCmd, RnClient};
use crate::rnconfig::{PackageManager, ProjectConfig};
use crate::rnview::RnView;
use simon::devices::{get_all_installed, get_all_running, InstalledDevice, Platform, RunningDevice};
use simon::{android, ios};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
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
    platform: Platform,
    running: bool,
    boot: Option<BootTarget>,
    open: Option<OpenTarget>,
    installed: Option<bool>,
    foreground: Option<bool>,
}

impl DeviceRow {
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
    link_picker: bool, // deep-link quick-picker overlay
    quit: bool,
}

pub fn run(project: ProjectConfig) -> Result<()> {
    let mut app = DashApp::new(project);
    let mut terminal = ratatui::init();
    let res = app.main_loop(&mut terminal);
    ratatui::restore();
    res
}

impl DashApp {
    fn new(project: ProjectConfig) -> DashApp {
        let (tx, rx) = std::sync::mpsc::channel();
        let client = RnClient::start(project.metro_port());
        // Background device poller — simctl/adb are slow, so keep them off the UI thread.
        let dev_tx = tx.clone();
        let android_bundle = project.android_bundle_id().map(String::from);
        std::thread::spawn(move || loop {
            let running_devices = get_all_running(None);
            // Resolve a running emulator's adb serial from its AVD name.
            let android_serial = |avd: &str| {
                running_devices.iter().find_map(|d| match d {
                    RunningDevice::AndroidEmulator { name, serial } if name == avd => Some(serial.clone()),
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
                let label = d.label();
                let running = d.running();
                let row = match &d {
                    InstalledDevice::IosSim { udid, .. } => DeviceRow {
                        label,
                        platform: Platform::Ios,
                        running,
                        boot: Some(BootTarget::IosSim(udid.clone())),
                        open: running.then(|| OpenTarget::IosSim(udid.clone())),
                        installed: None, // iOS install state not probed
                        foreground: None,
                    },
                    InstalledDevice::AndroidAvd { name, .. } => {
                        let serial = if running { android_serial(name) } else { None };
                        let (installed, foreground) = match &serial {
                            Some(s) => android_state(s),
                            None => (None, None),
                        };
                        DeviceRow {
                            label,
                            platform: Platform::Android,
                            running,
                            boot: Some(BootTarget::Avd(name.clone())),
                            open: serial.map(OpenTarget::AndroidSerial),
                            installed,
                            foreground,
                        }
                    }
                };
                rows.push(row);
            }
            // … plus any connected physical devices (already running, not bootable).
            for d in &running_devices {
                match d {
                    RunningDevice::IosPhysical { udid, .. } => {
                        rows.push(DeviceRow {
                            label: d.label(),
                            platform: Platform::Ios,
                            running: true,
                            boot: None,
                            open: Some(OpenTarget::IosPhysical(udid.clone())),
                            installed: None, // devicectl app queries are slow — skip
                            foreground: None,
                        });
                    }
                    RunningDevice::AndroidPhysical { serial, .. } => {
                        let (installed, foreground) = android_state(serial);
                        rows.push(DeviceRow {
                            label: d.label(),
                            platform: Platform::Android,
                            running: true,
                            boot: None,
                            open: Some(OpenTarget::AndroidSerial(serial.clone())),
                            installed,
                            foreground,
                        });
                    }
                    _ => {}
                }
            }
            if dev_tx.send(DashMsg::Devices(rows)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2500));
        });
        DashApp {
            project,
            procs: Vec::new(),
            proc_sel: 0,
            metro_idx: None,
            client,
            rnview: RnView::new(None),
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
            quit: false,
        }
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(&self.project.root)
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn pty_size(&self) -> (u16, u16) {
        self.proc_area.map(|r| (r.height.max(1), r.width.max(1))).unwrap_or((24, 80))
    }

    fn spawn_proc(&mut self, label: &str, command: &str) -> Option<usize> {
        let (rows, cols) = self.pty_size();
        match PtyProcess::spawn(label, command, &self.root(), &self.project.command_env(), rows, cols) {
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

    fn start_metro(&mut self) {
        let cmd = self.project.metro_command();
        // Restart in place if Metro is already a tab.
        if let Some(i) = self.metro_idx {
            if let Some(old) = self.procs.get_mut(i) {
                old.kill();
            }
            let (rows, cols) = self.pty_size();
            match PtyProcess::spawn("Metro", &cmd, &self.root(), &self.project.command_env(), rows, cols) {
                Ok(p) => {
                    self.procs[i] = p;
                    self.proc_sel = i;
                    self.set_flash("restarted Metro");
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

    fn run_platform(&mut self, android: bool, device_flag: Option<String>) {
        let (label, mut cmd) = if android {
            ("Android", self.project.android_command())
        } else {
            ("iOS", self.project.ios_command())
        };
        // Target the highlighted device, unless the command already names one.
        if let Some(args) = device_flag {
            let already = ["--udid", "--device", "--simulator", "--deviceId"].iter().any(|f| cmd.contains(f));
            if !already {
                // npm needs `--` to forward args to the script; yarn/pnpm don't.
                let sep = if self.project.package_manager() == PackageManager::Npm { " -- " } else { " " };
                cmd = format!("{cmd}{sep}{args}");
            }
        }
        if let Some(i) = self.spawn_proc(label, &cmd) {
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
        if !dev.running {
            if let Some(target) = dev.boot.clone() {
                self.boot_target(target, dev.label.clone());
            }
        }
        let android = dev.platform == Platform::Android;
        // Target this device: iOS by udid (known even pre-boot); Android by adb
        // serial (only once running — a freshly launched emulator falls back to
        // the RN CLI default, which is the one we just started).
        let device_flag = if android {
            dev.android_serial().map(|s| format!("--deviceId {s}"))
        } else {
            dev.ios_udid().map(|u| format!("--udid {u}"))
        };
        self.run_platform(android, device_flag);
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
            let which = if dev.platform == Platform::Android { "android" } else { "ios" };
            self.set_flash(format!("set {which}.bundleId in rn.json to launch the app"));
            return;
        }
        self.run_on_device(target, None, dev.label);
    }

    /// `l` picker: open the chosen deep link on the selected (running) device.
    fn open_deeplink(&mut self, idx: usize) {
        let url = match self.project.deeplinks.get(idx) {
            Some(d) => d.url().to_string(),
            None => return,
        };
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
        let verb = if url.is_some() { "opening" } else { "launching" };
        self.set_flash(format!("{verb} on {label}…"));
        std::thread::spawn(move || {
            let res = match (&url, target) {
                (Some(u), OpenTarget::IosSim(udid)) => ios::open_url_on_simulator(&udid, u),
                (Some(u), OpenTarget::IosPhysical(udid)) => ios::open_url_on_physical_ios(&udid, u, ios_bundle.as_deref(), false),
                (Some(u), OpenTarget::AndroidSerial(serial)) => android::open_url_with_package(&serial, u, android_pkg.as_deref()),
                (None, OpenTarget::IosSim(udid)) => ios::launch_app_on_simulator(&udid, ios_bundle.as_deref().unwrap_or_default()),
                (None, OpenTarget::IosPhysical(udid)) => ios::launch_app_on_physical_ios(&udid, ios_bundle.as_deref().unwrap_or_default()),
                (None, OpenTarget::AndroidSerial(serial)) => android::launch_app(&serial, android_pkg.as_deref().unwrap_or_default()),
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
        if let Some(i) = self.metro_idx {
            if let Some(p) = self.procs.get_mut(i) {
                if p.is_alive() {
                    p.write_input(&[byte]);
                    self.set_flash(format!("metro: {what}"));
                    return;
                }
            }
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

            terminal.draw(|f| render(self, f))?;

            // Keep every PTY sized to the pane (no-op when unchanged).
            if let Some(r) = self.proc_area {
                for p in self.procs.iter_mut() {
                    p.resize(r.height, r.width);
                }
            }

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.on_key(k);
                    }
                }
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);

        // Quit confirmation popup swallows all input until answered.
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.quit = true,
                _ if ctrl_c => self.quit = true, // Ctrl-C again = force quit
                KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return;
        }

        // Deep-link quick-picker: a digit/letter opens that link; Esc closes.
        if self.link_picker {
            if key.code == KeyCode::Esc {
                self.link_picker = false;
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
                KeyCode::Left if ctrl => return self.h_split = self.h_split.saturating_sub(5).max(20),
                KeyCode::Right if ctrl => return self.h_split = (self.h_split + 5).min(80),
                KeyCode::Up if ctrl => return self.v_split = self.v_split.saturating_sub(5).max(20),
                KeyCode::Down if ctrl => return self.v_split = (self.v_split + 5).min(80),
                KeyCode::Tab => return self.cycle_focus(false),
                KeyCode::BackTab => return self.cycle_focus(true),
                KeyCode::Char('q') => {
                    self.confirm_quit = true;
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
            KeyCode::Enter => {
                if self.procs.get(self.proc_sel).map(|p| p.is_alive()).unwrap_or(false) {
                    self.input_mode = true;
                } else {
                    self.set_flash("no running process in this tab");
                }
            }
            KeyCode::Char('x') => self.stop_selected_proc(),
            KeyCode::Char('m') => self.start_metro(),
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
            KeyCode::Enter => self.install_selected(),
            KeyCode::Char('b') => self.start_selected_device(),
            KeyCode::Char('s') => self.stop_selected_device(),
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('l') => {
                if self.project.deeplinks.is_empty() {
                    self.set_flash("no deeplinks in rn.json");
                } else if self.devices.get(self.dev_sel).and_then(|d| d.open.as_ref()).is_none() {
                    self.set_flash("start the device first (⏎)");
                } else {
                    self.link_picker = true;
                }
            }
            _ => {}
        }
    }
}

/// Quick-pick key for item `i`: 1-9, then a-z once the digits run out.
fn picker_char(i: usize) -> Option<char> {
    if i < 9 {
        Some((b'1' + i as u8) as char)
    } else if i < 9 + 26 {
        Some((b'a' + (i - 9) as u8) as char)
    } else {
        None
    }
}

/// Inverse of `picker_char`.
fn picker_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some(c as usize - '1' as usize),
        'a'..='z' => Some(9 + (c as usize - 'a' as usize)),
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

    let halves = Layout::vertical([Constraint::Percentage(app.v_split), Constraint::Percentage(100 - app.v_split)]).split(body);
    let top = Layout::horizontal([Constraint::Percentage(app.h_split), Constraint::Percentage(100 - app.h_split)]).split(halves[0]);
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
    if app.confirm_quit {
        render_quit_popup(frame, area);
    }
}

fn render_link_picker(app: &DashApp, frame: &mut Frame, area: Rect) {
    let links = &app.project.deeplinks;
    let h = (links.len() as u16 + 2).clamp(3, area.height);
    let r = centered(area, 64, h);
    frame.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" Deep links · esc ");
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let lines: Vec<Line> = links
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            picker_char(i).map(|k| {
                Line::from(vec![
                    Span::styled(format!(" {k} "), Style::default().fg(Color::Black).bg(Color::Cyan)),
                    Span::raw(format!("  {}", d.label())),
                ])
            })
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn render_quit_popup(frame: &mut Frame, area: Rect) {
    let r = centered(area, 50, 5);
    frame.render_widget(Clear, r); // wipe whatever's underneath
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" Quit metroctl? ");
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
    let (content, style) = if focused { (text.to_string(), bar_style()) } else { (String::new(), Style::default()) };
    let n = content.chars().count();
    let padded = if n >= w { content.chars().take(w).collect::<String>() } else { format!("{content}{}", " ".repeat(w - n)) };
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
                let dead = if p.is_alive() { "" } else { " (exited)" };
                if i == app.proc_sel {
                    format!("[{}{}]", p.label, dead)
                } else {
                    format!(" {}{} ", p.label, dead)
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
    let block = Block::default().borders(Borders::ALL).border_style(focus_border(focused)).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (content, hint) = split_hint(inner);
    app.proc_area = Some(content);

    match app.procs.get(app.proc_sel) {
        Some(p) if p.is_alive() => {
            let lines = pty_lines(&p.parser(), content.width, content.height);
            frame.render_widget(Paragraph::new(Text::from(lines)), content);
        }
        Some(p) => {
            // Stopped — blank the stale terminal and say so.
            let msg = Paragraph::new(format!("· {} stopped ·", p.label)).style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, content);
        }
        None => {
            let msg = Paragraph::new("Press  m  to start Metro.").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, content);
        }
    }
    if let Some(h) = hint {
        render_hint(frame, h, " [ ] tab · ⏎ type · x stop · m metro", focused);
    }
}

fn render_devices(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Devices;
    let block = Block::default().borders(Borders::ALL).border_style(focus_border(focused)).title(" Devices & Actions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (content, hint) = split_hint(inner);

    let metro_up = app.metro_idx.and_then(|i| app.procs.get(i)).map(|p| p.is_alive()).unwrap_or(false);
    let metro_line = if metro_up {
        Line::from(vec![
            Span::styled("Metro  ● ", Style::default().fg(Color::Green)),
            Span::raw(format!("running :{}", app.project.metro_port())),
        ])
    } else {
        Line::from(vec![Span::styled("Metro  ○ ", Style::default().fg(Color::DarkGray)), Span::raw("stopped  (m)")])
    };

    let mut lines: Vec<Line> = vec![metro_line, Line::raw("")];
    if app.devices.is_empty() {
        lines.push(Line::styled("  (no simulators/emulators found)", Style::default().fg(Color::DarkGray)));
    }
    for (i, d) in app.devices.iter().enumerate() {
        let marker = if d.running {
            Span::styled("● ", Style::default().fg(Color::Green))
        } else {
            Span::styled("○ ", Style::default().fg(Color::DarkGray))
        };
        let name_style = if focused && i == app.dev_sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let mut spans = vec![Span::raw(" "), marker, Span::styled(d.label.clone(), name_style)];
        // App-presence tags (only meaningful with a configured bundleId).
        match d.installed {
            Some(true) => spans.push(Span::styled("  app✓", Style::default().fg(Color::Green))),
            Some(false) => spans.push(Span::styled("  app✗", Style::default().fg(Color::DarkGray))),
            None => {}
        }
        if d.foreground == Some(true) {
            spans.push(Span::styled("  ▶fg", Style::default().fg(Color::Cyan)));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), content);
    if let Some(h) = hint {
        render_hint(frame, h, " ↑↓ sel · ⏎ run · b start · s stop · o open · l links", focused);
    }
}

fn render_logs(app: &mut DashApp, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Logs;
    let block = Block::default().borders(Borders::ALL).border_style(focus_border(focused)).title(" JS Logs / Network / Perf ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.rnview.set_focused(focused); // dim its footer when another pane is active
    app.rnview.render(frame, inner);
}

fn render_status(app: &DashApp, frame: &mut Frame, area: Rect) {
    let style = Style::default().bg(Color::Rgb(59, 66, 82)).fg(Color::White);
    // Global keys only — each pane shows its own keys in its footer.
    let text: String = if let Some((f, _)) = &app.flash {
        format!(" {f}")
    } else if app.input_mode {
        " INPUT — keys go to the process · Esc to exit".into()
    } else {
        " ⇥ focus · ^←→↑↓ resize · R reload · D dev-menu · q quit".into()
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

fn pty_lines(parser: &std::sync::Arc<std::sync::Mutex<vt100::Parser>>, width: u16, height: u16) -> Vec<Line<'static>> {
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
                    (if g.is_empty() { " ".to_string() } else { g }, cell_style(cell))
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
