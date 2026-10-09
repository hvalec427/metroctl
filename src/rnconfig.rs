//! React Native project config (`~/.config/metroctl/config.json`): a registry of
//! projects keyed by repo root, each describing how to run Metro and how to
//! build/run iOS and Android (serde with camelCase names).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 8081;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Yarn,
    Pnpm,
}

impl PackageManager {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "npm" => Some(Self::Npm),
            "yarn" => Some(Self::Yarn),
            "pnpm" => Some(Self::Pnpm),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm",
        }
    }

    /// `metro` / dev-server script.
    pub fn metro_command(&self) -> String {
        format!("{} start", self.as_str())
    }

    /// Run script for a platform. npm needs `run`, yarn/pnpm take the script
    /// name directly.
    pub fn run_command(&self, script: &str) -> String {
        match self {
            Self::Npm => format!("npm run {script}"),
            Self::Yarn => format!("yarn {script}"),
            Self::Pnpm => format!("pnpm {script}"),
        }
    }

    /// Install dependencies with this package manager.
    pub fn install_command(&self) -> &'static str {
        match self {
            Self::Npm => "npm install",
            Self::Yarn => "yarn install",
            Self::Pnpm => "pnpm install",
        }
    }
}

/// Guess the package manager for a project from its lockfile, defaulting to npm.
pub fn detect_package_manager(root: &Path) -> PackageManager {
    if root.join("yarn.lock").exists() {
        PackageManager::Yarn
    } else if root.join("pnpm-lock.yaml").exists() {
        PackageManager::Pnpm
    } else {
        PackageManager::Npm
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetroConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// A deep link entry: either a bare URL string, or `{ "name": …, "url": … }`
/// for a friendlier label in the picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeepLink {
    Url(String),
    Named { name: String, url: String },
}

impl DeepLink {
    pub fn url(&self) -> &str {
        match self {
            DeepLink::Url(u) => u,
            DeepLink::Named { url, .. } => url,
        }
    }
    pub fn label(&self) -> &str {
        match self {
            DeepLink::Url(u) => u,
            DeepLink::Named { name, .. } => name,
        }
    }
}

/// A platform's build/run command. Which device it runs on is chosen live from
/// the dashboard's Devices pane, not pinned here. `bundle_id` is the app's
/// bundle id (iOS) / application id (Android), used to open links straight in
/// the app rather than a browser.
///
/// `command` may contain a `{udid}` (iOS) / `{serial}` (Android) placeholder,
/// substituted with the selected device so it lands in the right spot — e.g.
/// `"react-native run-ios --udid {udid} --scheme Foo -- --reset-cache"`. Without
/// a placeholder metroctl appends `--udid`/`--deviceId`, which a `-- …`
/// passthrough in the script would send to the wrong side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlatformConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(rename = "bundleId", default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    /// Absolute repo root; matched as a path prefix of the current directory.
    pub root: String,
    #[serde(rename = "packageManager", default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metro: Option<MetroConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios: Option<PlatformConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android: Option<PlatformConfig>,
    /// Deep links to pick from with `l` in the Devices pane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deeplinks: Vec<DeepLink>,
    /// Override for the deep-clean command (`c` in the Processes pane).
    #[serde(rename = "cleanCommand", default, skip_serializing_if = "Option::is_none")]
    pub clean_command: Option<String>,
    /// Override for the reinstall command (`i` in the Processes pane).
    #[serde(rename = "installCommand", default, skip_serializing_if = "Option::is_none")]
    pub install_command: Option<String>,
}

impl ProjectConfig {
    /// A minimal entry pointing at a root, everything else derived from defaults.
    pub fn new(name: String, root: String) -> Self {
        ProjectConfig {
            name,
            root,
            package_manager: None,
            metro: None,
            ios: None,
            android: None,
            deeplinks: Vec::new(),
            clean_command: None,
            install_command: None,
        }
    }

    pub fn ios_bundle_id(&self) -> Option<&str> {
        self.ios.as_ref().and_then(|c| c.bundle_id.as_deref())
    }

    pub fn android_bundle_id(&self) -> Option<&str> {
        self.android.as_ref().and_then(|c| c.bundle_id.as_deref())
    }

    pub fn package_manager(&self) -> PackageManager {
        self.package_manager
            .as_deref()
            .and_then(PackageManager::parse)
            .unwrap_or_else(|| detect_package_manager(Path::new(&self.root)))
    }

    pub fn metro_command(&self) -> String {
        self.metro
            .as_ref()
            .and_then(|m| m.command.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| self.package_manager().metro_command())
    }

    pub fn metro_port(&self) -> u16 {
        self.metro.as_ref().and_then(|m| m.port).unwrap_or(DEFAULT_PORT)
    }

    pub fn ios_command(&self) -> String {
        self.ios
            .as_ref()
            .and_then(|c| c.command.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| self.package_manager().run_command("ios"))
    }

    pub fn android_command(&self) -> String {
        self.android
            .as_ref()
            .and_then(|c| c.command.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| self.package_manager().run_command("android"))
    }

    /// Deep clean: purge caches, node_modules, Pods and build dirs (and
    /// reinstall), via react-native-clean-project. Override with `cleanCommand`.
    pub fn clean_command(&self) -> String {
        self.clean_command
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| {
                "npx react-native-clean-project --remove-iOS-build --remove-iOS-pods --clean-android-project --remove-android-build".to_string()
            })
    }

    /// Reinstall JS deps + CocoaPods. Override with `installCommand`.
    pub fn install_command(&self) -> String {
        self.install_command
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| format!("{} && npx pod-install", self.package_manager().install_command()))
    }

    /// Env for every command metroctl spawns: the Metro port exported as
    /// `RCT_METRO_PORT`, so the port is configured in exactly one place (`metro.port`).
    pub fn command_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("RCT_METRO_PORT".to_string(), self.metro_port().to_string());
        env
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RnConfig {
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

pub fn rn_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/metroctl/config.json")
}

pub fn load_rn_config() -> Result<Option<RnConfig>> {
    let p = rn_config_path();
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p)?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("Could not parse {} — is it valid JSON?", p.display()))
}

pub fn save_rn_config(cfg: &RnConfig) -> Result<()> {
    let p = rn_config_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&p, json)?;
    Ok(())
}

/// The registered project whose `root` is the longest path-prefix of `dir`.
pub fn project_for_dir<'a>(cfg: &'a RnConfig, dir: &Path) -> Option<&'a ProjectConfig> {
    cfg.projects
        .iter()
        .filter(|p| dir.starts_with(Path::new(&p.root)))
        .max_by_key(|p| p.root.len())
}

/// The registered project matching the current working directory. `rn init`
/// stores canonicalized roots, so a canonicalized cwd matches through symlinks.
pub fn current_project(cfg: &RnConfig) -> Option<&ProjectConfig> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    project_for_dir(cfg, &cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_manager_commands() {
        assert_eq!(PackageManager::Npm.metro_command(), "npm start");
        assert_eq!(PackageManager::Npm.run_command("ios"), "npm run ios");
        assert_eq!(PackageManager::Yarn.run_command("ios"), "yarn ios");
        assert_eq!(PackageManager::Yarn.run_command("android"), "yarn android");
        assert_eq!(PackageManager::Pnpm.run_command("android"), "pnpm android");
    }

    #[test]
    fn defaults_when_unset() {
        let p = ProjectConfig::new("App".into(), "/tmp/does-not-exist-xyz".into());
        // No lockfile → npm defaults.
        assert_eq!(p.metro_command(), "npm start");
        assert_eq!(p.ios_command(), "npm run ios");
        assert_eq!(p.android_command(), "npm run android");
        assert_eq!(p.metro_port(), DEFAULT_PORT);
    }

    #[test]
    fn explicit_overrides_win() {
        let mut p = ProjectConfig::new("App".into(), "/tmp/app".into());
        p.package_manager = Some("yarn".into());
        p.metro = Some(MetroConfig { command: Some("yarn web-start".into()), port: Some(9000) });
        p.ios = Some(PlatformConfig { command: None, bundle_id: None });
        assert_eq!(p.metro_command(), "yarn web-start");
        assert_eq!(p.metro_port(), 9000);
        // ios.command unset → derived from the declared package manager (yarn).
        assert_eq!(p.ios_command(), "yarn ios");
    }

    #[test]
    fn command_env_sets_metro_port() {
        let mut p = ProjectConfig::new("App".into(), "/tmp/app".into());
        p.metro = Some(MetroConfig { command: None, port: Some(9000) });
        assert_eq!(p.command_env().get("RCT_METRO_PORT").map(String::as_str), Some("9000"));
    }

    #[test]
    fn longest_root_prefix_wins() {
        let cfg = RnConfig {
            projects: vec![
                ProjectConfig::new("outer".into(), "/Users/me/dev".into()),
                ProjectConfig::new("inner".into(), "/Users/me/dev/app".into()),
            ],
        };
        let hit = project_for_dir(&cfg, Path::new("/Users/me/dev/app/src")).unwrap();
        assert_eq!(hit.name, "inner");
        let hit = project_for_dir(&cfg, Path::new("/Users/me/dev/other")).unwrap();
        assert_eq!(hit.name, "outer");
        // Component-wise prefix: /dev must not match /development.
        assert!(project_for_dir(&cfg, Path::new("/Users/me/development")).is_none());
    }

    #[test]
    fn roundtrips_minimal_json() {
        let json = r#"{"projects":[{"name":"App","root":"/tmp/app"}]}"#;
        let cfg: RnConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].name, "App");
        // Minimal entry serializes back without null noise.
        let out = serde_json::to_string(&cfg).unwrap();
        assert_eq!(out, r#"{"projects":[{"name":"App","root":"/tmp/app"}]}"#);
    }
}
