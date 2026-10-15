//! `signalbox up`: a dashboard session set up for one checkout (usually a git
//! worktree): its own Metro port, its own simulator, built and running on
//! start. The session is described in `<root>/.signalbox/session.json` so other
//! tools (agents, `signalbox down`) can find the port, simulator and process.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What to do with a simulator `up` created when the dashboard quits.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SimCleanup {
    Ask,
    Delete,
    Keep,
}

#[derive(Clone, Debug, Default)]
pub struct UpOpts {
    /// `None` = configured port, `Some(None)` = first free port, `Some(Some(n))` = n.
    pub port: Option<Option<u16>>,
    /// Pin to an existing device (see `session::pin_device`).
    pub device: Option<String>,
    /// Create a new simulator (optionally with this name).
    pub new_sim: Option<Option<String>>,
    pub sim_type: Option<String>,
    pub sim_runtime: Option<String>,
    /// Start a throwaway emulator for this session (optionally of this AVD).
    pub new_emulator: Option<Option<String>>,
    pub sim_cleanup: Option<SimCleanup>,
    pub install: bool,
    /// Install the app the main checkout already built instead of building.
    pub prebuilt: bool,
}

/// The device a session is pinned to.
#[derive(Clone, Debug)]
pub struct Pinned {
    pub udid: String,
    pub created: bool, // we created it (a simulator, or an emulator instance), so we may discard it
    pub simulator: bool, // false for a physical device (nothing to boot)
    pub android: bool, // an adb serial (emulator or phone) rather than an iOS udid
    pub cleanup: SimCleanup,
    pub name: Option<String>, // device name, to tell its app apart from others on the same Metro
}

/// `.signalbox/session.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionFile {
    pub pid: u32,
    pub root: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default)]
    pub created_sim: bool,
    pub status: String,
    /// ios_simulator | ios_device | android.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// iOS bundle id of the app (to release it on `down`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    /// Control socket (JSON lines), see `control.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Started with `up --detach` (by an agent), so its tools may stop it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

/// Whether this process runs under `up --detach`.
pub fn detached() -> bool {
    std::env::var_os("SIGNALBOX_DETACHED").is_some()
}

pub fn session_dir(root: &Path) -> PathBuf {
    root.join(".signalbox")
}

pub fn session_path(root: &Path) -> PathBuf {
    session_dir(root).join("session.json")
}

pub fn read_session(root: &Path) -> Option<SessionFile> {
    serde_json::from_slice(&std::fs::read(session_path(root)).ok()?).ok()
}

pub fn write_session(root: &Path, s: &SessionFile) {
    let dir = session_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    exclude_from_git(root);
    let tmp = dir.join("session.json.tmp");
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, session_path(root));
        }
    }
    register_global(s);
}

pub fn remove_session(root: &Path) {
    if let Some(s) = read_session(root) {
        let _ = std::fs::remove_file(global_dir().join(format!("{}.json", s.pid)));
    }
    let _ = std::fs::remove_file(session_path(root));
}

/// Keep `.signalbox/` out of git without touching the project's .gitignore.
fn exclude_from_git(root: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .output();
    let Some(path) = out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
    else {
        return;
    };
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current
        .lines()
        .any(|l| l.trim() == ".signalbox/" || l.trim() == ".signalbox")
    {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::write(&path, format!("{current}{sep}.signalbox/\n"));
}

/// Every live session is also listed in `~/.config/signalbox/sessions/<pid>.json`,
/// so `--port auto` can skip ports another session claimed but hasn't bound yet.
fn global_dir() -> PathBuf {
    crate::rnconfig::rn_config_path().with_file_name("sessions")
}

fn register_global(s: &SessionFile) {
    register_in(&global_dir(), s);
}

fn register_in(dir: &Path, s: &SessionFile) {
    if std::fs::create_dir_all(dir).is_ok() {
        if let Ok(json) = serde_json::to_vec_pretty(s) {
            let _ = std::fs::write(dir.join(format!("{}.json", s.pid)), json);
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Sessions that are still running (stale entries are cleaned up).
fn live_sessions() -> Vec<SessionFile> {
    live_sessions_in(&global_dir())
}

fn live_sessions_in(dir: &Path) -> Vec<SessionFile> {
    reap_stale_in(dir);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect()
}

/// Clean up after sessions whose process died without doing it themselves
/// (killed, crashed, window closed): delete the simulator they created, or
/// stop the app they pointed at their port, which another session may reuse.
/// Returns what was cleaned.
pub fn reap_stale() -> Vec<String> {
    reap_stale_in(&global_dir())
}

fn reap_stale_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut done = Vec::new();
    for e in rd.flatten() {
        let Some(s) = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionFile>(&b).ok())
        else {
            let _ = std::fs::remove_file(e.path());
            continue;
        };
        if s.pid == std::process::id() || pid_alive(s.pid) {
            continue;
        }
        if let Some(udid) = &s.udid {
            if s.created_sim {
                if let Ok(what) = discard_device(udid, s.platform.as_deref() == Some("android")) {
                    done.push(format!("{what} of a dead session in {}", s.root));
                }
            } else if s.platform.as_deref() == Some("android") {
                android_release(udid);
                done.push(format!(
                    "removed the adb port mapping on {udid} (dead session in {})",
                    s.root
                ));
            } else if let Some(bundle) = &s.bundle {
                release_app(udid, bundle);
                done.push(format!(
                    "released the app on {udid} (dead session in {})",
                    s.root
                ));
            }
        }
        // Its worktree's session file, unless a newer session replaced it.
        let root = Path::new(&s.root);
        if read_session(root).is_some_and(|w| w.pid == s.pid) {
            let _ = std::fs::remove_file(session_path(root));
        }
        let _ = std::fs::remove_file(e.path());
    }
    done
}

/// `signalbox gc`: clean up dead sessions; with `sims`, also delete
/// `signalbox-*` simulators no running session owns.
pub fn gc(sims: bool) -> Result<()> {
    let done = reap_stale();
    for d in &done {
        println!("{d}");
    }
    let mut n = done.len();
    if sims {
        let owned: Vec<String> = live_sessions().into_iter().filter_map(|s| s.udid).collect();
        let v = simctl_json(&["list", "devices"])?;
        for d in v["devices"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .flat_map(|l| l.as_array().into_iter().flatten())
        {
            let (name, udid) = (
                d["name"].as_str().unwrap_or(""),
                d["udid"].as_str().unwrap_or(""),
            );
            if name.starts_with("signalbox-")
                && !name.starts_with(TEMPLATE_PREFIX)
                && !owned.iter().any(|o| o == udid)
            {
                delete_simulator(udid)?;
                println!("deleted orphaned simulator {name} ({udid})");
                n += 1;
            }
        }
    }
    if n == 0 {
        println!("nothing to clean up");
    }
    Ok(())
}

/// First port from `start` that's neither listening nor claimed by a session.
fn free_port_in(dir: &Path, start: u16) -> Result<u16> {
    let claimed: Vec<u16> = live_sessions_in(dir).iter().map(|s| s.port).collect();
    (start..start.saturating_add(100))
        .find(|p| !claimed.contains(p) && !crate::metro_events::port_in_use(*p))
        .ok_or_else(|| anyhow!("no free port in {start}–{}", start.saturating_add(99)))
}

/// A lock directory: one holder at a time, taken over when it's older than
/// `stale` (its holder died). Released when dropped.
struct DirLock(PathBuf);

impl DirLock {
    fn acquire(path: &Path, stale: Duration) -> DirLock {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = Instant::now();
        while std::fs::create_dir(path).is_err() {
            let old = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|a| a > stale);
            if old || start.elapsed() > stale {
                let _ = std::fs::remove_dir(path);
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        DirLock(path.to_path_buf())
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// What a session claims before its dashboard is up: its port, and its device
/// once it has one. Registered at once (under a lock) so sessions starting
/// together never get the same port or device.
pub struct Claim {
    dir: PathBuf,
    entry: SessionFile,
}

impl Claim {
    /// Claim Metro port `start`, or with `auto` the first free one from it.
    pub fn port(root: &Path, start: u16, auto: bool) -> Result<Claim> {
        Claim::port_in(&global_dir(), std::process::id(), root, start, auto)
    }

    fn port_in(dir: &Path, pid: u32, root: &Path, start: u16, auto: bool) -> Result<Claim> {
        let _lock = DirLock::acquire(&dir.join("claims.lock"), Duration::from_secs(60));
        let port = if auto {
            free_port_in(dir, start)?
        } else {
            start
        };
        let entry = SessionFile {
            pid,
            root: root.display().to_string(),
            port,
            udid: None,
            created_sim: false,
            status: "starting".into(),
            platform: None,
            bundle: None,
            socket: None,
            detached: detached(),
        };
        register_in(dir, &entry);
        Ok(Claim {
            dir: dir.to_path_buf(),
            entry,
        })
    }

    pub fn port_number(&self) -> u16 {
        self.entry.port
    }

    /// Claim device `udid` (one this session `created`, or an existing one),
    /// unless another live session holds it.
    pub fn device(&mut self, udid: &str, created: bool) -> Result<()> {
        let _lock = DirLock::acquire(&self.dir.join("claims.lock"), Duration::from_secs(60));
        if let Some(o) = live_sessions_in(&self.dir)
            .into_iter()
            .find(|s| s.pid != self.entry.pid && s.udid.as_deref() == Some(udid))
        {
            bail!(
                "{udid} is in use by the signalbox session in {} (pid {}); pick another device, or use --new-sim",
                o.root,
                o.pid
            );
        }
        self.entry.udid = Some(udid.to_string());
        self.entry.created_sim = created;
        register_in(&self.dir, &self.entry);
        Ok(())
    }
}

fn simctl_json(args: &[&str]) -> Result<Value> {
    let out = Command::new("xcrun")
        .arg("simctl")
        .args(args)
        .arg("-j")
        .output()
        .context("running xcrun simctl")?;
    if !out.status.success() {
        bail!(
            "simctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Simulator state (`Booted`, `Shutdown`, …), or `None` if `udid` isn't a simulator.
pub fn sim_state(udid: &str) -> Option<String> {
    sim_info(udid).map(|(state, _)| state)
}

/// (state, name) of a simulator, or `None` if `udid` isn't one.
pub fn sim_info(udid: &str) -> Option<(String, String)> {
    let v = simctl_json(&["list", "devices"]).ok()?;
    let d = v["devices"]
        .as_object()?
        .values()
        .flat_map(|l| l.as_array().into_iter().flatten())
        .find(|d| d["udid"] == udid)?;
    Some((
        d["state"].as_str().unwrap_or("").to_string(),
        d["name"].as_str().unwrap_or("").to_string(),
    ))
}

/// Undo `point_app_at_port` when a session ends: the simulator may outlive
/// it, and its app would otherwise keep loading from (and later connect to
/// whatever reuses) this session's port.
pub fn release_app(udid: &str, bundle: &str) {
    let _ = Command::new("xcrun")
        .args(["simctl", "terminate", udid, bundle])
        .output();
    let _ = Command::new("xcrun")
        .args([
            "simctl",
            "spawn",
            udid,
            "defaults",
            "delete",
            bundle,
            "RCT_jsLocation",
        ])
        .output();
}

/// Version as comparable numbers ("18.2" → [18, 2]).
fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The runtime and device type for a new simulator: the newest iOS runtime
/// (or the one matching `runtime`, e.g. "18.2" / "iOS 18.2"), and the newest
/// plain "iPhone N" it supports (or the type named `device`).
fn pick_sim(
    runtimes: &Value,
    runtime: Option<&str>,
    device: Option<&str>,
) -> Result<(String, String, String)> {
    let mut rts: Vec<&Value> = runtimes["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| {
            r["isAvailable"].as_bool().unwrap_or(false)
                && r["platform"].as_str().map_or_else(
                    || r["name"].as_str().unwrap_or("").starts_with("iOS"),
                    |p| p == "iOS",
                )
        })
        .collect();
    rts.sort_by_key(|r| version_key(r["version"].as_str().unwrap_or("")));
    let rt = match runtime {
        Some(want) => rts
            .iter()
            .rev()
            .find(|r| {
                let (v, n) = (
                    r["version"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or(""),
                );
                v == want || v.starts_with(&format!("{want}.")) || n.eq_ignore_ascii_case(want)
            })
            .ok_or_else(|| anyhow!("no installed iOS runtime matches {want:?}"))?,
        None => rts.last().ok_or_else(|| {
            anyhow!("no iOS simulator runtime installed (Xcode › Settings › Components)")
        })?,
    };
    let types: Vec<&Value> = rt["supportedDeviceTypes"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let name = |t: &Value| t["name"].as_str().unwrap_or("").to_string();
    let ty = match device {
        Some(want) => types
            .iter()
            .find(|t| name(t).eq_ignore_ascii_case(want) || t["identifier"] == want)
            .ok_or_else(|| {
                anyhow!(
                    "{} doesn't support a device type {want:?}",
                    rt["name"].as_str().unwrap_or("runtime")
                )
            })?,
        None => {
            // "iPhone 16" over "iPhone 16 Pro"/"iPhone 16e"/"iPhone SE": the plain number.
            let plain = |t: &&&Value| {
                name(t)
                    .strip_prefix("iPhone ")
                    .and_then(|n| n.parse::<u32>().ok())
            };
            types
                .iter()
                .filter(|t| plain(t).is_some())
                .max_by_key(|t| plain(t))
                .or_else(|| types.iter().rev().find(|t| name(t).starts_with("iPhone")))
                .ok_or_else(|| {
                    anyhow!(
                        "{} supports no iPhone",
                        rt["name"].as_str().unwrap_or("runtime")
                    )
                })?
        }
    };
    let id = |v: &Value| v["identifier"].as_str().unwrap_or("").to_string();
    Ok((
        id(ty),
        id(rt),
        format!("{} · {}", name(ty), rt["name"].as_str().unwrap_or("")),
    ))
}

/// Create a simulator; returns (udid, "iPhone 17 · iOS 26.0").
/// Name prefix of the settled simulators new ones are cloned from.
const TEMPLATE_PREFIX: &str = "signalbox-template";

fn simctl_out(args: &[&str]) -> Result<String> {
    let out = Command::new("xcrun").arg("simctl").args(args).output()?;
    if !out.status.success() {
        bail!(
            "simctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// udid of the simulator called `name`, if any.
fn sim_named(name: &str) -> Option<String> {
    let v = simctl_json(&["list", "devices"]).ok()?;
    v["devices"]
        .as_object()?
        .values()
        .flat_map(|l| l.as_array().into_iter().flatten())
        .find(|d| d["name"] == name)
        .and_then(|d| d["udid"].as_str().map(String::from))
}

/// Run `simctl launch` with a time limit (it hangs while a fresh simulator is
/// still setting itself up). Whether it launched in time.
fn launch_within(udid: &str, bundle: &str, secs: u64) -> bool {
    let Ok(mut child) = Command::new("xcrun")
        .args(["simctl", "launch", udid, bundle])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(secs) => {
                std::thread::sleep(Duration::from_millis(300))
            }
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

/// The settled template for a device type + runtime, creating it the first
/// time: a newly created simulator says it's booted long before it can launch
/// apps (first boot sets up its data, minutes on recent iOS). The template
/// goes through that once; clones of it start ready.
fn template(ty: &str, rt: &str, desc: &str) -> Result<String> {
    let name = format!("{TEMPLATE_PREFIX} {desc}");
    // One builder at a time (parallel `up`s).
    let _lock = DirLock::acquire(
        &crate::rnconfig::rn_config_path().with_file_name("template.lock"),
        Duration::from_secs(30 * 60),
    );
    let start = Instant::now();
    if let Some(udid) = sim_named(&name) {
        return Ok(udid);
    }
    eprintln!("creating the simulator template \"{name}\" (once per device type and iOS version; a few minutes)…");
    let udid = simctl_out(&["create", &format!("{name} (setting up)"), ty, rt])?;
    let settle = || -> Result<()> {
        let _ = simctl_out(&["boot", &udid]);
        simctl_out(&["bootstatus", &udid, "-b"])?;
        eprintln!("booted; waiting until it can launch apps…");
        let t = Instant::now();
        while !launch_within(&udid, "com.apple.Preferences", 60) {
            if t.elapsed() > Duration::from_secs(30 * 60) {
                bail!("the template simulator never became ready to launch apps");
            }
            eprintln!("  still setting up ({}s)…", t.elapsed().as_secs());
        }
        let _ = simctl_out(&["terminate", &udid, "com.apple.Preferences"]);
        simctl_out(&["shutdown", &udid])?;
        simctl_out(&["rename", &udid, &name])?;
        Ok(())
    };
    if let Err(e) = settle() {
        let _ = delete_simulator(&udid);
        return Err(e);
    }
    eprintln!("template ready after {}s", start.elapsed().as_secs());
    Ok(udid)
}

/// Create a simulator by cloning the settled template for the chosen device
/// type and runtime (see `template`). Returns (udid, "iPhone 17 · iOS 26.0").
pub fn create_simulator(
    name: &str,
    runtime: Option<&str>,
    device: Option<&str>,
) -> Result<(String, String)> {
    let runtimes = simctl_json(&["list", "runtimes"])?;
    let (ty, rt, desc) = pick_sim(&runtimes, runtime, device)?;
    let tpl = template(&ty, &rt, &desc)?;
    let _ = simctl_out(&["shutdown", &tpl]); // clone needs it shut down
    let udid = simctl_out(&["clone", &tpl, name])?;
    Ok((udid, desc))
}

/// Shut down and delete a simulator.
pub fn delete_simulator(udid: &str) -> Result<()> {
    let _ = Command::new("xcrun")
        .args(["simctl", "shutdown", udid])
        .output();
    let out = Command::new("xcrun")
        .args(["simctl", "delete", udid])
        .output()?;
    if !out.status.success() {
        bail!(
            "simctl delete: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Make the app on a simulator load its bundle from `localhost:<port>` by
/// setting React Native's `RCT_jsLocation` default, relaunching it if that
/// changed anything. Returns whether it relaunched.
pub fn point_app_at_port(udid: &str, bundle: &str, port: u16) -> Result<bool> {
    let want = format!("localhost:{port}");
    let read = Command::new("xcrun")
        .args([
            "simctl",
            "spawn",
            udid,
            "defaults",
            "read",
            bundle,
            "RCT_jsLocation",
        ])
        .output()?;
    if read.status.success() && String::from_utf8_lossy(&read.stdout).trim() == want {
        return Ok(false);
    }
    let out = Command::new("xcrun")
        .args([
            "simctl",
            "spawn",
            udid,
            "defaults",
            "write",
            bundle,
            "RCT_jsLocation",
            &want,
        ])
        .output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = Command::new("xcrun")
        .args(["simctl", "terminate", udid, bundle])
        .output();
    // `simctl launch` can hang on a freshly created simulator; don't hang with it.
    let mut child = Command::new("xcrun")
        .args(["simctl", "launch", udid, bundle])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut e, &mut err);
                }
                bail!("{}", err.trim());
            }
            return Ok(true);
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            bail!("simctl launch didn't return within 30s");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// adb serials of connected, ready Android devices and emulators.
pub fn adb_serials() -> Vec<String> {
    let out = Command::new(simon::android::find_bin("adb"))
        .arg("devices")
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, s)| s.trim() == "device")
        .map(|(d, _)| d.to_string())
        .collect()
}

/// The device `up --device` names, resolved like simon and boop do (iOS
/// udid, AVD name, adb serial, or a name only one device has). A stopped
/// emulator is started; a stopped simulator gets booted with the session.
pub fn pin_device(id: &str, cleanup: SimCleanup) -> Result<Pinned> {
    use simon::devices::{find_device, Found, InstalledDevice, RunningDevice};
    let pin = |udid: String, simulator: bool, android: bool, name: Option<String>| Pinned {
        udid,
        created: false,
        simulator,
        android,
        cleanup,
        name,
    };
    Ok(match find_device(id)? {
        Found::Running(RunningDevice::IosSim { udid, name, .. })
        | Found::Stopped(InstalledDevice::IosSim { udid, name, .. }) => {
            pin(udid, true, false, Some(name))
        }
        Found::Running(RunningDevice::IosPhysical { udid, .. }) => pin(udid, false, false, None),
        Found::Running(
            RunningDevice::AndroidEmulator { serial, .. }
            | RunningDevice::AndroidPhysical { serial, .. },
        ) => pin(serial, false, true, None),
        Found::Stopped(InstalledDevice::AndroidAvd { name, .. }) => {
            pin(boot_avd(&name)?, false, true, None)
        }
    })
}

/// Start an emulator and wait until it has booted; returns its adb serial.
fn boot_avd(avd: &str) -> Result<String> {
    eprintln!("starting emulator {avd}…");
    simon::android::launch_avd(avd)?;
    let serial = || {
        simon::android::running_emulators()
            .into_iter()
            .find(|e| e.name == avd)
            .map(|e| e.serial)
    };
    wait_booted(avd, serial, || Ok(()))
}

/// Poll until the emulator `serial()` finds has finished booting (4 minutes
/// at most), checking `alive()` in between; returns the serial.
fn wait_booted(
    avd: &str,
    serial: impl Fn() -> Option<String>,
    mut alive: impl FnMut() -> Result<()>,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(240);
    while Instant::now() < deadline {
        alive()?;
        if let Some(serial) = serial() {
            let booted = Command::new(simon::android::find_bin("adb"))
                .args(["-s", &serial, "shell", "getprop", "sys.boot_completed"])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1");
            if booted {
                return Ok(serial);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("emulator {avd} didn't finish booting in 4 minutes")
}

/// The AVD `--new-emulator` runs: the one named, or the only one there is.
pub fn pick_avd(avds: &[String], name: Option<&str>) -> Result<String> {
    match name {
        Some(n) if avds.iter().any(|a| a == n) => Ok(n.to_string()),
        Some(n) => bail!("no AVD named {n} (have: {})", list_or_none(avds)),
        None => match avds {
            [only] => Ok(only.clone()),
            [] => {
                bail!("no Android Virtual Devices; create one in Android Studio's Device Manager")
            }
            _ => bail!(
                "several AVDs ({}); name one: --new-emulator <AVD>",
                avds.join(", ")
            ),
        },
    }
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

/// First emulator console port (even, 5554–5680) whose port pair is free and
/// no adb device already uses; the instance's adb serial is `emulator-<port>`.
pub fn free_emulator_port(taken: &[String]) -> Result<u16> {
    let free = |p: u16| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok();
    (5554..=5680)
        .step_by(2)
        .find(|&p| !taken.contains(&format!("emulator-{p}")) && free(p) && free(p + 1))
        .ok_or_else(|| anyhow!("no free emulator port in 5554–5681"))
}

/// Start a throwaway instance of an AVD for this session and wait until it
/// has booted. `-read-only` lets it run next to other instances of the same
/// AVD and drops whatever it writes when it stops, so stopping it is all the
/// cleanup it needs. Returns (adb serial, AVD name).
pub fn start_emulator(avd: Option<&str>) -> Result<(String, String)> {
    use std::os::unix::process::CommandExt;
    let avd = pick_avd(&simon::android::list_avds(), avd)?;
    let port = free_emulator_port(&adb_serials())?;
    let emulator = simon::android::find_bin("emulator");
    let sdk = simon::android::sdk_root();
    eprintln!("starting a read-only {avd} emulator on port {port}…");
    let log_path = std::env::temp_dir().join(format!("signalbox-emulator-{port}.log"));
    let log = std::fs::File::create(&log_path)?;
    let mut child = Command::new(&emulator)
        .args([
            "-avd",
            &avd,
            "-read-only",
            "-no-snapshot-save",
            "-no-boot-anim",
        ])
        .args(["-port", &port.to_string()])
        .current_dir(
            Path::new(&emulator)
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        )
        .env("ANDROID_HOME", &sdk)
        .env("ANDROID_SDK_ROOT", &sdk)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        // Its own process group: closing the terminal mustn't take it down
        // when the session keeps it.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {emulator}"))?;
    let serial = format!("emulator-{port}");
    let booted = wait_booted(
        &avd,
        || adb_serials().contains(&serial).then(|| serial.clone()),
        || match child.try_wait()? {
            None => Ok(()),
            Some(status) => {
                let err = std::fs::read_to_string(&log_path).unwrap_or_default();
                let last = err.lines().rev().find(|l| !l.trim().is_empty());
                bail!(
                    "emulator {avd} exited during boot ({status}){}",
                    last.map(|l| format!(": {}", l.trim())).unwrap_or_default()
                )
            }
        },
    );
    if booted.is_err() {
        let _ = child.kill();
    }
    Ok((booted?, avd))
}

/// Stop an emulator (`adb emu kill`) and wait for it to leave adb.
pub fn stop_emulator(serial: &str) -> Result<()> {
    let out = Command::new(simon::android::find_bin("adb"))
        .args(["-s", serial, "emu", "kill"])
        .output()?;
    if !out.status.success() {
        bail!(
            "adb emu kill: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // It goes offline before it's gone; wait until adb doesn't list it at all.
    let t = Instant::now();
    while adb_listed(serial) {
        if t.elapsed() > Duration::from_secs(30) {
            bail!("emulator {serial} is still running 30s after `adb emu kill`");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Whether `adb devices` lists `serial` in any state (offline included).
fn adb_listed(serial: &str) -> bool {
    Command::new(simon::android::find_bin("adb"))
        .arg("devices")
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .skip(1)
                .any(|l| l.split('\t').next() == Some(serial))
        })
}

/// Get rid of a device a session created: delete the simulator, or stop the
/// emulator instance. Returns what was done.
pub fn discard_device(udid: &str, android: bool) -> Result<String> {
    if android {
        stop_emulator(udid)?;
        Ok(format!("stopped emulator {udid}"))
    } else {
        delete_simulator(udid)?;
        Ok(format!("deleted simulator {udid}"))
    }
}

/// Android's counterpart of `point_app_at_port`: the app loads from
/// localhost:8081 on the device, so map that to our port and relaunch it.
pub fn android_point_app_at_port(serial: &str, package: Option<&str>, port: u16) -> Result<()> {
    let out = Command::new("adb")
        .args(["-s", serial, "reverse", "tcp:8081", &format!("tcp:{port}")])
        .output()?;
    if !out.status.success() {
        bail!(
            "adb reverse: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if let Some(pkg) = package {
        let _ = Command::new("adb")
            .args(["-s", serial, "shell", "am", "force-stop", pkg])
            .output();
        let _ = Command::new("adb")
            .args([
                "-s",
                serial,
                "shell",
                "monkey",
                "-p",
                pkg,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ])
            .output();
    }
    Ok(())
}

pub fn android_release(serial: &str) {
    let _ = Command::new("adb")
        .args(["-s", serial, "reverse", "--remove", "tcp:8081"])
        .output();
}

/// The newest simulator build of `bundle` in Xcode's DerivedData, preferring
/// builds of a workspace under `prefer_root` (the main checkout).
pub fn find_prebuilt_app(bundle: &str, prefer_root: Option<&Path>) -> Option<PathBuf> {
    let dd = PathBuf::from(std::env::var("HOME").ok()?).join("Library/Developer/Xcode/DerivedData");
    let plist = |p: &Path, key: &str| -> Option<String> {
        let out = Command::new("plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(p)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let mut found: Vec<(bool, std::time::SystemTime, PathBuf)> = Vec::new();
    for d in std::fs::read_dir(&dd).ok()?.flatten() {
        let ours = prefer_root.is_some_and(|root| {
            plist(&d.path().join("info.plist"), "WorkspacePath")
                .is_some_and(|w| Path::new(&w).starts_with(root))
        });
        let Ok(products) = std::fs::read_dir(d.path().join("Build/Products")) else {
            continue;
        };
        for conf in products.flatten().filter(|c| {
            c.file_name()
                .to_string_lossy()
                .ends_with("-iphonesimulator")
        }) {
            for app in std::fs::read_dir(conf.path())
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "app"))
            {
                if plist(&app.join("Info.plist"), "CFBundleIdentifier").as_deref() == Some(bundle) {
                    let t = std::fs::metadata(&app)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    found.push((ours, t, app));
                }
            }
        }
    }
    found
        .into_iter()
        .max_by_key(|(ours, t, _)| (*ours, *t))
        .map(|(_, _, p)| p)
}

/// Shell command that installs a prebuilt app on a simulator, points it at
/// `port` and launches it (in the background: `simctl launch` can hang on a
/// freshly created simulator).
pub fn install_prebuilt_command(udid: &str, app: &Path, bundle: &str, port: u16) -> String {
    let app = format!("'{}'", app.display().to_string().replace('\'', "'\\''"));
    format!(
        "echo 'installing the main checkout build ({app}); JS comes from this worktree. Rebuild after native changes.'; \
         xcrun simctl install {udid} {app} && \
         xcrun simctl spawn {udid} defaults write {bundle} RCT_jsLocation localhost:{port} && \
         (xcrun simctl launch {udid} {bundle} &) ; sleep 5; echo 'info Launching {bundle}'"
    )
}

/// Shell command that boots a simulator, shows it, and waits until it's ready.
pub fn boot_command(udid: &str) -> String {
    format!(
        "xcrun simctl boot {udid} 2>/dev/null; open -a Simulator --args -CurrentDeviceUDID {udid}; echo 'waiting for the simulator to finish booting…'; xcrun simctl bootstatus {udid} -b"
    )
}

/// A simulator name from the checkout dir: `signalbox-<dir>`.
pub fn default_sim_name(root: &Path) -> String {
    let dir = root.file_name().and_then(|n| n.to_str()).unwrap_or("app");
    format!("signalbox-{dir}")
}

/// `signalbox down`: stop the session running in `root` and (unless `keep_sim`)
/// delete the simulator it created.
pub fn down(root: &Path, keep_sim: bool) -> Result<()> {
    let s =
        read_session(root).ok_or_else(|| anyhow!("no signalbox session in {}", root.display()))?;
    if pid_alive(s.pid) {
        let _ = Command::new("kill")
            .args(["-TERM", &s.pid.to_string()])
            .output();
        let t = Instant::now();
        while pid_alive(s.pid) && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(s.pid) {
            let _ = Command::new("kill")
                .args(["-KILL", &s.pid.to_string()])
                .output();
        }
        println!("stopped signalbox (pid {})", s.pid);
    }
    // Metro normally dies with its PTY; make sure the port is free.
    if crate::metro_events::port_in_use(s.port) {
        let _ = Command::new("npx")
            .args(["--yes", "kill-port", &s.port.to_string()])
            .output();
    }
    if let (Some(udid), true, false) = (&s.udid, s.created_sim, keep_sim) {
        println!(
            "{}",
            discard_device(udid, s.platform.as_deref() == Some("android"))?
        );
    } else if let (Some(udid), Some("android")) = (&s.udid, s.platform.as_deref()) {
        android_release(udid);
    } else if let (Some(udid), Some(bundle)) = (&s.udid, &s.bundle) {
        release_app(udid, bundle);
    }
    remove_session(root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runtimes() -> Value {
        let ty = |n: &str| json!({"name": n, "identifier": format!("id.{}", n.replace(' ', "-"))});
        json!({"runtimes": [
            {"name": "iOS 17.5", "version": "17.5", "platform": "iOS", "isAvailable": true, "identifier": "rt.17.5",
             "supportedDeviceTypes": [ty("iPhone 15"), ty("iPhone 15 Pro")]},
            {"name": "iOS 18.2", "version": "18.2", "platform": "iOS", "isAvailable": true, "identifier": "rt.18.2",
             "supportedDeviceTypes": [ty("iPhone SE (3rd generation)"), ty("iPhone 16 Pro"), ty("iPhone 16"), ty("iPhone 16e"), ty("iPad Air")]},
            {"name": "iOS 26.0", "version": "26.0", "platform": "iOS", "isAvailable": false, "identifier": "rt.26",
             "supportedDeviceTypes": [ty("iPhone 17")]},
            {"name": "watchOS 11.2", "version": "11.2", "platform": "watchOS", "isAvailable": true, "identifier": "rt.w",
             "supportedDeviceTypes": []},
        ]})
    }

    #[test]
    fn picks_the_named_or_only_avd() {
        let avds = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_avd(&avds(&["Pixel_8"]), None).unwrap(), "Pixel_8");
        assert_eq!(
            pick_avd(&avds(&["Pixel_8", "Tab"]), Some("Tab")).unwrap(),
            "Tab"
        );
        let err = |v: &[&str], n| pick_avd(&avds(v), n).unwrap_err().to_string();
        assert_eq!(
            err(&["Pixel_8", "Tab"], None),
            "several AVDs (Pixel_8, Tab); name one: --new-emulator <AVD>"
        );
        assert_eq!(
            err(&["Pixel_8"], Some("Nope")),
            "no AVD named Nope (have: Pixel_8)"
        );
        assert!(err(&[], None).starts_with("no Android Virtual Devices"));
    }

    #[test]
    fn emulator_ports_skip_taken_serials_and_busy_pairs() {
        let taken = vec!["emulator-5554".to_string()];
        let p = free_emulator_port(&taken).unwrap();
        assert!(p >= 5556 && p.is_multiple_of(2));
        // Hold the console port of the next pick, and the adb port of the one after.
        let _a = std::net::TcpListener::bind(("127.0.0.1", p)).unwrap();
        let q = free_emulator_port(&taken).unwrap();
        assert!(q > p);
        let _b = std::net::TcpListener::bind(("127.0.0.1", q + 1)).unwrap();
        assert!(free_emulator_port(&taken).unwrap() > q);
    }

    /// Needs an Android SDK and an AVD: SIGNALBOX_TEST_AVD=<name> cargo test -- --ignored
    #[test]
    #[ignore]
    fn runs_two_throwaway_emulators_of_one_avd() {
        let avd = std::env::var("SIGNALBOX_TEST_AVD").ok();
        let (a, name) = start_emulator(avd.as_deref()).unwrap();
        let (b, _) = start_emulator(Some(&name)).unwrap();
        assert_ne!(a, b);
        assert!(adb_serials().contains(&a) && adb_serials().contains(&b));
        let names: Vec<_> = simon::android::running_emulators()
            .into_iter()
            .filter(|e| e.serial == a || e.serial == b)
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec![name.clone(), name]);
        assert_eq!(
            discard_device(&a, true).unwrap(),
            format!("stopped emulator {a}")
        );
        discard_device(&b, true).unwrap();
        assert!(!adb_listed(&a) && !adb_listed(&b));
    }

    /// A registry dir of its own, and live processes to claim as.
    fn claim_setup(name: &str) -> (PathBuf, Vec<std::process::Child>) {
        let dir =
            std::env::temp_dir().join(format!("signalbox-claims-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let kids = (0..2)
            .map(|_| Command::new("sleep").arg("30").spawn().unwrap())
            .collect();
        (dir, kids)
    }

    #[test]
    fn concurrent_claims_get_different_ports() {
        let (dir, mut kids) = claim_setup("ports");
        let start = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ports: Vec<u16> = kids
            .iter()
            .map(|k| {
                let (dir, pid) = (dir.clone(), k.id());
                std::thread::spawn(move || {
                    Claim::port_in(&dir, pid, Path::new("/wt"), start, true)
                        .unwrap()
                        .port_number()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_ne!(ports[0], ports[1]);
        for k in kids.iter_mut() {
            let _ = k.kill();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_held_device_cant_be_claimed() {
        let (dir, mut kids) = claim_setup("device");
        let mut a = Claim::port_in(&dir, kids[0].id(), Path::new("/a"), 9000, false).unwrap();
        let mut b = Claim::port_in(&dir, kids[1].id(), Path::new("/b"), 9001, false).unwrap();
        a.device("SIM-1", false).unwrap();
        let err = b.device("SIM-1", false).unwrap_err().to_string();
        assert!(
            err.contains("in use by the signalbox session in /a"),
            "{err}"
        );
        b.device("SIM-2", false).unwrap();
        // Once its holder is gone, it's free again.
        kids[0].kill().unwrap();
        kids[0].wait().unwrap();
        let mut c = Claim::port_in(&dir, kids[1].id(), Path::new("/b"), 9001, false).unwrap();
        c.device("SIM-1", false).unwrap();
        let _ = kids[1].kill();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn free_port_skips_listening_ports() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = l.local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("signalbox-free-{}", std::process::id()));
        assert_ne!(free_port_in(&dir, taken).unwrap(), taken);
    }

    #[test]
    fn picks_newest_available_runtime_and_plain_iphone() {
        let (ty, rt, desc) = pick_sim(&runtimes(), None, None).unwrap();
        assert_eq!((ty.as_str(), rt.as_str()), ("id.iPhone-16", "rt.18.2"));
        assert_eq!(desc, "iPhone 16 · iOS 18.2");
    }

    #[test]
    fn honours_runtime_and_type_overrides() {
        let (ty, rt, _) = pick_sim(&runtimes(), Some("17"), Some("iphone 15 pro")).unwrap();
        assert_eq!((ty.as_str(), rt.as_str()), ("id.iPhone-15-Pro", "rt.17.5"));
        assert!(pick_sim(&runtimes(), Some("26.0"), None).is_err()); // not available
        assert!(pick_sim(&runtimes(), None, Some("iPhone 99")).is_err());
    }
}
