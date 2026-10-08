//! Self-update: release channels (stable / nightly / dev), GitHub resolution,
//! version comparison, and installing over the running binary. Mirrors simon's
//! `update.rs`, adapted for the metroctl repo/binary.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::process::Command;

const REPO: &str = "hvalec427/metroctl";
const DEFAULT_INSTALL_PATH: &str = "/usr/local/bin/metroctl";
const UA: &str = "metroctl-cli";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Nightly,
    Dev,
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
            Channel::Dev => "dev",
        }
    }
}

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn platform_supported() -> bool {
    cfg!(target_os = "macos") && (cfg!(target_arch = "aarch64") || cfg!(target_arch = "x86_64"))
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/metroctl/update.json")
}

pub fn load_channel() -> Channel {
    let raw = std::fs::read_to_string(config_path()).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    match v.get("channel").and_then(|c| c.as_str()) {
        Some("nightly") => Channel::Nightly,
        Some("dev") => Channel::Dev,
        _ => Channel::Stable,
    }
}

pub fn save_channel(channel: Channel) {
    let p = config_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, format!("{{\n  \"channel\": \"{}\"\n}}\n", channel.as_str()));
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().user_agent(UA).build().expect("http client")
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
}

pub struct Latest {
    pub version: String,
    pub tag: String,
}

fn strip_v(s: &str) -> String {
    s.strip_prefix('v').unwrap_or(s).to_string()
}

pub fn latest_for_channel(channel: Channel) -> Result<Latest> {
    let c = client();
    match channel {
        Channel::Dev => {
            let r = c.get(format!("https://api.github.com/repos/{REPO}/releases/tags/dev")).send()?;
            if !r.status().is_success() {
                bail!("No dev build has been published yet.");
            }
            let rel: Release = r.json()?;
            let version = strip_v(rel.name.as_deref().unwrap_or(&rel.tag_name));
            if version.is_empty() {
                bail!("Could not read the dev build version.");
            }
            Ok(Latest { version, tag: "dev".into() })
        }
        Channel::Stable => {
            let r = c.get(format!("https://api.github.com/repos/{REPO}/releases/latest")).send()?;
            if !r.status().is_success() {
                bail!("GitHub API returned {}", r.status());
            }
            let rel: Release = r.json()?;
            Ok(Latest { version: strip_v(&rel.tag_name), tag: rel.tag_name })
        }
        Channel::Nightly => {
            let r = c.get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30")).send()?;
            if !r.status().is_success() {
                bail!("GitHub API returned {}", r.status());
            }
            let mut releases: Vec<Release> = r.json()?;
            releases.retain(|r| r.prerelease && strip_v(&r.tag_name).contains("-nightly."));
            releases.sort_by(|a, b| compare_versions(&strip_v(&b.tag_name), &strip_v(&a.tag_name)));
            let nightly = releases.into_iter().next().ok_or_else(|| anyhow::anyhow!("No nightly (prerelease) build found yet."))?;
            Ok(Latest { version: strip_v(&nightly.tag_name), tag: nightly.tag_name })
        }
    }
}

fn strip_install(body: &str) -> String {
    let lower = body.to_lowercase();
    if let Some(idx) = lower.find("install this build") {
        let head = body[..idx].rfind('\n').unwrap_or(0);
        body[..head].trim().to_string()
    } else {
        body.trim().to_string()
    }
}

/// Aggregated changelog for releases in `channel` newer than `current`,
/// newest-first. None for dev (rolling, no notes) or on any failure.
pub fn changelog_since(channel: Channel, current: &str) -> Option<String> {
    if channel == Channel::Dev {
        return None;
    }
    let c = client();
    let r = c.get(format!("https://api.github.com/repos/{REPO}/releases?per_page=100")).send().ok()?;
    if !r.status().is_success() {
        return None;
    }
    let releases: Vec<Release> = r.json().ok()?;
    let mut in_channel: Vec<Release> = releases
        .into_iter()
        .filter(|r| if channel == Channel::Stable { !r.prerelease } else { r.prerelease })
        .collect();
    in_channel.sort_by(|a, b| compare_versions(&strip_v(&b.tag_name), &strip_v(&a.tag_name)));
    if in_channel.is_empty() {
        return None;
    }
    let known = current != "unknown" && current.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
    let newer: Vec<&Release> = if known {
        in_channel.iter().filter(|r| compare_versions(&strip_v(&r.tag_name), current) == Ordering::Greater).collect()
    } else {
        in_channel.iter().take(1).collect()
    };
    if newer.is_empty() {
        return None;
    }
    const MAX: usize = 25;
    let mut sections: Vec<String> = newer
        .iter()
        .take(MAX)
        .map(|r| {
            let notes = r.body.as_deref().map(strip_install).filter(|s| !s.is_empty()).unwrap_or_else(|| "_(no notes)_".into());
            format!("## {}\n\n{}", r.tag_name, notes)
        })
        .collect();
    if newer.len() > MAX {
        sections.push(format!("_… and {} older release(s)._", newer.len() - MAX));
    }
    Some(sections.join("\n\n"))
}

/// Compare versions incl. `-<label>.<N>` prereleases. A final X.Y.Z outranks its
/// prereleases.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn parts(v: &str) -> [u64; 4] {
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let mut nums = core.split('.').map(|n| n.parse::<u64>().unwrap_or(0));
        let x = nums.next().unwrap_or(0);
        let y = nums.next().unwrap_or(0);
        let z = nums.next().unwrap_or(0);
        let pre_num = match pre {
            None => u64::MAX,
            Some(p) => p.rsplit_once('.').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0),
        };
        [x, y, z, pre_num]
    }
    parts(a).cmp(&parts(b))
}

fn asset_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "metroctl-darwin-arm64"
    } else {
        "metroctl-darwin-x64"
    }
}

pub fn download_binary(tag: &str) -> Result<PathBuf> {
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{}", asset_name());
    let resp = client().get(&url).send()?;
    if !resp.status().is_success() {
        bail!("Download failed: {}", resp.status());
    }
    let bytes = resp.bytes()?;
    let tmp = std::env::temp_dir().join("metroctl-update");
    std::fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = Command::new("xattr").arg("-d").arg("com.apple.quarantine").arg(&tmp).output();
    Ok(tmp)
}

/// Install over the actually-running binary (resolving symlinks), falling back
/// to `metroctl` on PATH, then the default path.
pub fn install_target() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if exe.file_name().map(|n| n == "metroctl").unwrap_or(false) {
            return std::fs::canonicalize(&exe).unwrap_or(exe);
        }
    }
    if let Ok(out) = Command::new("sh").args(["-c", "command -v metroctl"]).output() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p));
        }
    }
    PathBuf::from(DEFAULT_INSTALL_PATH)
}

pub fn needs_sudo(target: &std::path::Path) -> bool {
    let dir = target.parent().unwrap_or(std::path::Path::new("/"));
    let probe = dir.join(".metroctl-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

pub fn install_binary(tmp: &std::path::Path, target: &std::path::Path) -> Result<()> {
    let cmd = if needs_sudo(target) { ("sudo", vec!["mv"]) } else { ("mv", vec![]) };
    let mut c = Command::new(cmd.0);
    c.args(cmd.1).arg(tmp).arg(target);
    if !c.status()?.success() {
        bail!("install failed");
    }
    Ok(())
}

fn resolve_channel(stable: bool, nightly: bool, dev: bool) -> Channel {
    if dev {
        Channel::Dev
    } else if nightly {
        Channel::Nightly
    } else if stable {
        Channel::Stable
    } else {
        load_channel()
    }
}

/// `metroctl update` — install the latest build for the chosen/remembered channel.
pub fn update(stable: bool, nightly: bool, dev: bool, force: bool) {
    if !platform_supported() {
        eprintln!("metroctl self-update is macOS-only (arm64/x64). Build from source on other platforms.");
        std::process::exit(1);
    }
    let previous = load_channel();
    let channel = resolve_channel(stable, nightly, dev);
    if stable || nightly || dev {
        save_channel(channel);
    }
    let switched = channel != previous;
    let current = current_version();

    let latest = match latest_for_channel(channel) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let cmp = compare_versions(&latest.version, &current);
    if cmp == Ordering::Equal && !force {
        println!("Already on the latest {} version ({current}).", channel.as_str());
        return;
    }
    if cmp == Ordering::Less && !switched && !force {
        println!("The latest {} build ({}) is older than your installed {current} — not downgrading.", channel.as_str(), latest.version);
        println!("`metroctl update` will pick it up once a newer {} build is published, or use --force.", channel.as_str());
        return;
    }

    if cmp == Ordering::Less {
        println!("Installing {} ({}) — older than your current {current}{}.", latest.version, channel.as_str(), if switched { "" } else { ", forced" });
    } else if cmp == Ordering::Equal {
        println!("Reinstalling {} ({})...", latest.version, channel.as_str());
    } else {
        println!("Updating {current} → {} ({})...", latest.version, channel.as_str());
        if let Some(notes) = changelog_since(channel, &current) {
            println!("\nWhat's new ({current} → {}):\n{notes}\n", latest.version);
        }
    }

    let tmp = match download_binary(&latest.tag) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let target = install_target();
    let sudo_note = if needs_sudo(&target) { " (needs sudo — may prompt for your password)" } else { "" };
    println!("Installing to {}{sudo_note}...", target.display());
    if let Err(e) = install_binary(&tmp, &target) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let verb = match cmp {
        Ordering::Less => "Installed",
        Ordering::Equal => "Reinstalled",
        Ordering::Greater => "Updated to",
    };
    println!("{verb} {} ({}).", latest.version, channel.as_str());
}

/// `metroctl check-update` — report whether a newer build is available.
pub fn check_update(stable: bool, nightly: bool, dev: bool) {
    if !platform_supported() {
        eprintln!("metroctl self-update is macOS-only (arm64/x64). Build from source on other platforms.");
        std::process::exit(1);
    }
    let channel = resolve_channel(stable, nightly, dev);
    let current = current_version();

    let latest = match latest_for_channel(channel) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let cmp = compare_versions(&latest.version, &current);
    println!("Channel:   {}", channel.as_str());
    println!("Installed: {current}");
    println!("Latest:    {}", latest.version);

    match cmp {
        Ordering::Equal => println!("\nYou're up to date."),
        Ordering::Less => {
            println!("\nThe latest {} build ({}) is older than your installed version.", channel.as_str(), latest.version);
        }
        Ordering::Greater => {
            if let Some(notes) = changelog_since(channel, &current) {
                println!("\nWhat's new ({current} → {}):\n{notes}", latest.version);
            }
            let flag = if dev { " --dev" } else if nightly { " --nightly" } else if stable { " --stable" } else { "" };
            println!("\nRun `metroctl update{flag}` to install.");
        }
    }
}
