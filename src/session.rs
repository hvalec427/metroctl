//! `metroctl up`: a dashboard session set up for one checkout (usually a git
//! worktree): its own Metro port, its own simulator, built and running on
//! start. The session is described in `<root>/.metroctl/session.json` so other
//! tools (orc, `metroctl down`) can find the port, simulator and process.

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
    /// Pin to an existing device by udid.
    pub device: Option<String>,
    /// Create a new simulator (optionally with this name).
    pub new_sim: Option<Option<String>>,
    pub sim_type: Option<String>,
    pub sim_runtime: Option<String>,
    pub sim_cleanup: Option<SimCleanup>,
    pub install: bool,
}

/// The device a session is pinned to.
#[derive(Clone, Debug)]
pub struct Pinned {
    pub udid: String,
    pub created: bool,         // we created it, so we may delete it
    pub simulator: bool,       // false for a physical device (nothing to boot)
    pub cleanup: SimCleanup,
}

/// `.metroctl/session.json`.
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
    /// Control socket (JSON lines), see `control.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

pub fn session_dir(root: &Path) -> PathBuf {
    root.join(".metroctl")
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

/// Keep `.metroctl/` out of git without touching the project's .gitignore.
fn exclude_from_git(root: &Path) {
    let out = Command::new("git").arg("-C").arg(root).args(["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"]).output();
    let Some(path) = out.ok().filter(|o| o.status.success()).map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())) else {
        return;
    };
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ".metroctl/" || l.trim() == ".metroctl") {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    let _ = std::fs::write(&path, format!("{current}{sep}.metroctl/\n"));
}

/// Every live session is also listed in `~/.config/metroctl/sessions/<pid>.json`,
/// so `--port auto` can skip ports another session claimed but hasn't bound yet.
fn global_dir() -> PathBuf {
    crate::rnconfig::rn_config_path().with_file_name("sessions")
}

fn register_global(s: &SessionFile) {
    let dir = global_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_vec_pretty(s) {
            let _ = std::fs::write(dir.join(format!("{}.json", s.pid)), json);
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Sessions that are still running (stale entries are cleaned up).
fn live_sessions() -> Vec<SessionFile> {
    let Ok(rd) = std::fs::read_dir(global_dir()) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let s: SessionFile = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            if s.pid != std::process::id() && !pid_alive(s.pid) {
                let _ = std::fs::remove_file(e.path());
                return None;
            }
            Some(s)
        })
        .collect()
}

/// First port from `start` that's neither listening nor claimed by a session.
pub fn free_port(start: u16) -> Result<u16> {
    let claimed: Vec<u16> = live_sessions().iter().map(|s| s.port).collect();
    (start..start.saturating_add(100))
        .find(|p| !claimed.contains(p) && !crate::metro_events::port_in_use(*p))
        .ok_or_else(|| anyhow!("no free port in {start}–{}", start.saturating_add(99)))
}

fn simctl_json(args: &[&str]) -> Result<Value> {
    let out = Command::new("xcrun").arg("simctl").args(args).arg("-j").output().context("running xcrun simctl")?;
    if !out.status.success() {
        bail!("simctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Simulator state (`Booted`, `Shutdown`, …), or `None` if `udid` isn't a simulator.
pub fn sim_state(udid: &str) -> Option<String> {
    let v = simctl_json(&["list", "devices"]).ok()?;
    v["devices"].as_object()?.values().flat_map(|l| l.as_array().into_iter().flatten()).find(|d| d["udid"] == udid).map(|d| d["state"].as_str().unwrap_or("").to_string())
}

/// Version as comparable numbers ("18.2" → [18, 2]).
fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The runtime and device type for a new simulator: the newest iOS runtime
/// (or the one matching `runtime`, e.g. "18.2" / "iOS 18.2"), and the newest
/// plain "iPhone N" it supports (or the type named `device`).
fn pick_sim(runtimes: &Value, runtime: Option<&str>, device: Option<&str>) -> Result<(String, String, String)> {
    let mut rts: Vec<&Value> = runtimes["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["isAvailable"].as_bool().unwrap_or(false) && r["platform"].as_str().map_or_else(|| r["name"].as_str().unwrap_or("").starts_with("iOS"), |p| p == "iOS"))
        .collect();
    rts.sort_by_key(|r| version_key(r["version"].as_str().unwrap_or("")));
    let rt = match runtime {
        Some(want) => rts
            .iter()
            .rev()
            .find(|r| {
                let (v, n) = (r["version"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""));
                v == want || v.starts_with(&format!("{want}.")) || n.eq_ignore_ascii_case(want)
            })
            .ok_or_else(|| anyhow!("no installed iOS runtime matches {want:?}"))?,
        None => rts.last().ok_or_else(|| anyhow!("no iOS simulator runtime installed (Xcode › Settings › Components)"))?,
    };
    let types: Vec<&Value> = rt["supportedDeviceTypes"].as_array().into_iter().flatten().collect();
    let name = |t: &Value| t["name"].as_str().unwrap_or("").to_string();
    let ty = match device {
        Some(want) => types
            .iter()
            .find(|t| name(t).eq_ignore_ascii_case(want) || t["identifier"] == want)
            .ok_or_else(|| anyhow!("{} doesn't support a device type {want:?}", rt["name"].as_str().unwrap_or("runtime")))?,
        None => {
            // "iPhone 16" over "iPhone 16 Pro"/"iPhone 16e"/"iPhone SE": the plain number.
            let plain = |t: &&&Value| name(t).strip_prefix("iPhone ").and_then(|n| n.parse::<u32>().ok());
            types
                .iter()
                .filter(|t| plain(t).is_some())
                .max_by_key(|t| plain(t))
                .or_else(|| types.iter().rev().find(|t| name(t).starts_with("iPhone")))
                .ok_or_else(|| anyhow!("{} supports no iPhone", rt["name"].as_str().unwrap_or("runtime")))?
        }
    };
    let id = |v: &Value| v["identifier"].as_str().unwrap_or("").to_string();
    Ok((id(ty), id(rt), format!("{} · {}", name(ty), rt["name"].as_str().unwrap_or(""))))
}

/// Create a simulator; returns (udid, "iPhone 17 · iOS 26.0").
pub fn create_simulator(name: &str, runtime: Option<&str>, device: Option<&str>) -> Result<(String, String)> {
    let runtimes = simctl_json(&["list", "runtimes"])?;
    let (ty, rt, desc) = pick_sim(&runtimes, runtime, device)?;
    let out = Command::new("xcrun").args(["simctl", "create", name, &ty, &rt]).output()?;
    if !out.status.success() {
        bail!("simctl create: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok((String::from_utf8_lossy(&out.stdout).trim().to_string(), desc))
}

/// Shut down and delete a simulator.
pub fn delete_simulator(udid: &str) -> Result<()> {
    let _ = Command::new("xcrun").args(["simctl", "shutdown", udid]).output();
    let out = Command::new("xcrun").args(["simctl", "delete", udid]).output()?;
    if !out.status.success() {
        bail!("simctl delete: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Make the app on a simulator load its bundle from `localhost:<port>` by
/// setting React Native's `RCT_jsLocation` default, relaunching it if that
/// changed anything. Returns whether it relaunched.
pub fn point_app_at_port(udid: &str, bundle: &str, port: u16) -> Result<bool> {
    let want = format!("localhost:{port}");
    let read = Command::new("xcrun").args(["simctl", "spawn", udid, "defaults", "read", bundle, "RCT_jsLocation"]).output()?;
    if read.status.success() && String::from_utf8_lossy(&read.stdout).trim() == want {
        return Ok(false);
    }
    let out = Command::new("xcrun").args(["simctl", "spawn", udid, "defaults", "write", bundle, "RCT_jsLocation", &want]).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = Command::new("xcrun").args(["simctl", "terminate", udid, bundle]).output();
    let out = Command::new("xcrun").args(["simctl", "launch", udid, bundle]).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(true)
}

/// Shell command that boots a simulator, shows it, and waits until it's ready.
pub fn boot_command(udid: &str) -> String {
    format!(
        "xcrun simctl boot {udid} 2>/dev/null; open -a Simulator --args -CurrentDeviceUDID {udid}; echo 'waiting for the simulator to finish booting…'; xcrun simctl bootstatus {udid} -b"
    )
}

/// A simulator name from the checkout dir: `metroctl-<dir>`.
pub fn default_sim_name(root: &Path) -> String {
    let dir = root.file_name().and_then(|n| n.to_str()).unwrap_or("app");
    format!("metroctl-{dir}")
}

/// `metroctl down`: stop the session running in `root` and (unless `keep_sim`)
/// delete the simulator it created.
pub fn down(root: &Path, keep_sim: bool) -> Result<()> {
    let s = read_session(root).ok_or_else(|| anyhow!("no metroctl session in {}", root.display()))?;
    if pid_alive(s.pid) {
        let _ = Command::new("kill").args(["-TERM", &s.pid.to_string()]).output();
        let t = Instant::now();
        while pid_alive(s.pid) && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(s.pid) {
            let _ = Command::new("kill").args(["-KILL", &s.pid.to_string()]).output();
        }
        println!("stopped metroctl (pid {})", s.pid);
    }
    // Metro normally dies with its PTY; make sure the port is free.
    if crate::metro_events::port_in_use(s.port) {
        let _ = Command::new("npx").args(["--yes", "kill-port", &s.port.to_string()]).output();
    }
    if let (Some(udid), true, false) = (&s.udid, s.created_sim, keep_sim) {
        delete_simulator(udid)?;
        println!("deleted simulator {udid}");
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
    fn free_port_skips_listening_ports() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = l.local_addr().unwrap().port();
        assert_ne!(free_port(taken).unwrap(), taken);
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
