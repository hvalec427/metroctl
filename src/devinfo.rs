//! Extra per-device details for the Devices pane (OS version, API level,
//! connection type, problems that block a build) that simon's device listing
//! doesn't carry. All of these shell out, so they're only called from the
//! background device poller.

use simon::android;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Marketing Android version for an API level ("34" → "14").
fn android_version(api: &str) -> Option<&'static str> {
    Some(match api.parse::<u32>().ok()? {
        21 => "5.0",
        22 => "5.1",
        23 => "6",
        24 => "7.0",
        25 => "7.1",
        26 => "8.0",
        27 => "8.1",
        28 => "9",
        29 => "10",
        30 => "11",
        31 => "12",
        32 => "12L",
        33 => "13",
        34 => "14",
        35 => "15",
        36 => "16",
        37 => "17",
        _ => return None,
    })
}

fn version_api(release: Option<&str>, api: &str) -> String {
    match release.or_else(|| android_version(api)) {
        Some(v) => format!("Android {v} · API {api}"),
        None => format!("API {api}"),
    }
}

/// `Samsung · Android 14 · API 34 · Wi-Fi` for a running emulator/device.
pub fn android_running(serial: &str, physical: bool) -> Option<String> {
    let adb = android::find_bin("adb");
    let out = output(&adb, &["-s", serial, "shell", "getprop ro.build.version.release; getprop ro.build.version.sdk; getprop ro.product.manufacturer"])?;
    let mut it = out.lines().map(str::trim);
    let (release, api, maker) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
    let mut parts = Vec::new();
    if physical && !maker.is_empty() {
        let mut c = maker.chars();
        parts.push(c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default());
    }
    if !api.is_empty() {
        parts.push(version_api((!release.is_empty()).then_some(release), api));
    }
    if physical {
        // Wireless debugging serials are `ip:port` or an mDNS `adb-…._adb-tls-connect…` name.
        parts.push(if serial.contains(':') || serial.contains("_adb-tls-") { "Wi-Fi".into() } else { "USB".into() });
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `Android 16 · API 36 · Play Store` for an AVD, read from its config (works
/// while it's shut down).
pub fn avd(name: &str) -> Option<String> {
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let root = std::env::var("ANDROID_AVD_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".android/avd"));
    let ini = std::fs::read_to_string(root.join(format!("{name}.ini"))).ok()?;
    let dir = ini.lines().find_map(|l| l.strip_prefix("path=")).map(PathBuf::from).unwrap_or_else(|| root.join(format!("{name}.avd")));
    let cfg = std::fs::read_to_string(dir.join("config.ini")).ok()?;
    let get = |key: &str| cfg.lines().find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()));
    // image.sysdir.1 = system-images/android-36/google_apis_playstore/arm64-v8a/
    let sysdir = get("image.sysdir.1")?;
    let api = sysdir.split('/').find_map(|s| s.strip_prefix("android-"))?.to_string();
    let mut parts = vec![version_api(None, &api)];
    let tag = get("tag.id").unwrap_or_default();
    if tag.contains("playstore") {
        parts.push("Play Store".into());
    } else if tag.contains("google_apis") {
        parts.push("Google APIs".into());
    }
    Some(parts.join(" · "))
}

/// Android devices adb sees but can't use yet: (serial, problem).
pub fn android_unready() -> Vec<(String, String)> {
    let adb = android::find_bin("adb");
    let Some(out) = output(&adb, &["devices"]) else {
        return Vec::new();
    };
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let (serial, state) = l.split_once('\t')?;
            let problem = match state.trim() {
                "unauthorized" => "unauthorized — allow USB debugging on the phone",
                "offline" => "offline — reconnect the cable or restart adb",
                "no permissions" => "no permissions — check the USB mode / udev rules",
                _ => return None,
            };
            Some((serial.trim().to_string(), problem.to_string()))
        })
        .collect()
}

/// Details for a connected iPhone/iPad, by udid.
pub struct IosDetail {
    pub text: String,
    pub problem: Option<String>,
}

/// Model, connection and readiness for connected iOS devices, keyed by udid
/// (from `devicectl`; empty without Xcode).
pub fn ios_physical() -> HashMap<String, IosDetail> {
    let tmp = std::env::temp_dir().join("metroctl-devicectl.json");
    let ok = Command::new("xcrun").args(["devicectl", "list", "devices", "--json-output"]).arg(&tmp).output().map(|o| o.status.success()).unwrap_or(false);
    let data: serde_json::Value = match ok.then(|| std::fs::read(&tmp).ok()).flatten().and_then(|r| serde_json::from_slice(&r).ok()) {
        Some(d) => d,
        None => return HashMap::new(),
    };
    let mut map = HashMap::new();
    for d in data["result"]["devices"].as_array().into_iter().flatten() {
        let conn = &d["connectionProperties"];
        if conn["transportType"].is_null() {
            continue; // not connected
        }
        let hw = &d["hardwareProperties"];
        let udid = hw["udid"].as_str().or_else(|| d["identifier"].as_str()).unwrap_or("").to_string();
        let mut parts = Vec::new();
        if let Some(m) = hw["marketingName"].as_str() {
            parts.push(m.to_string());
        }
        parts.push(match conn["transportType"].as_str() {
            Some("wired") => "USB".into(),
            Some("localNetwork") => "Wi-Fi".into(),
            Some(other) => other.to_string(),
            None => String::new(),
        });
        let problem = if conn["pairingState"].as_str().is_some_and(|s| s != "paired") {
            Some("not paired — unlock it and tap Trust".to_string())
        } else if d["deviceProperties"]["developerModeStatus"].as_str() == Some("disabled") {
            Some("Developer Mode off — Settings › Privacy & Security".to_string())
        } else {
            None
        };
        parts.retain(|p| !p.is_empty());
        map.insert(udid, IosDetail { text: parts.join(" · "), problem });
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_api_levels() {
        assert_eq!(version_api(None, "34"), "Android 14 · API 34");
        assert_eq!(version_api(Some("15"), "35"), "Android 15 · API 35");
        assert_eq!(version_api(None, "99"), "API 99");
    }
}
