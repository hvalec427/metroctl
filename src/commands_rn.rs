//! `simon rn` — manage and run a React Native project from a single window.
//! No subcommand launches the dashboard TUI; `init` registers the current
//! directory; `config` prints the config path.

use crate::rnconfig::{current_project, load_rn_config, rn_config_path, save_rn_config, ProjectConfig, RnConfig};
use inquire::Text;

/// `simon rn` — launch the dashboard for the current project.
pub fn launch() {
    let cfg = match load_rn_config() {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("No projects configured yet. Run `simon rn init` in your project directory.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let project = match current_project(&cfg) {
        Some(p) => p.clone(),
        None => {
            eprintln!("This directory isn't a registered React Native project.");
            eprintln!("Run `simon rn init` here to add it.");
            std::process::exit(1);
        }
    };
    if let Err(e) = crate::rndash::run(project) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// `simon rn init` — register (or update) the current directory as a project.
pub fn init() {
    let cwd = match std::env::current_dir() {
        Ok(d) => std::fs::canonicalize(&d).unwrap_or(d),
        Err(e) => {
            eprintln!("Couldn't read the current directory: {e}");
            std::process::exit(1);
        }
    };
    if !cwd.join("package.json").exists() {
        eprintln!("Warning: no package.json here — this doesn't look like a JS project.");
    }

    let default_name = cwd.file_name().and_then(|n| n.to_str()).unwrap_or("app").to_string();
    let name = match Text::new("Project name:").with_default(&default_name).prompt() {
        Ok(n) => n,
        Err(_) => return,
    };

    let root = cwd.to_string_lossy().to_string();
    let entry = ProjectConfig::new(name.clone(), root.clone());

    let mut cfg = load_rn_config().unwrap_or(None).unwrap_or_default();
    match cfg.projects.iter_mut().find(|p| p.root == root) {
        Some(existing) => {
            existing.name = name.clone();
            println!("Updated \"{name}\" in {}", rn_config_path().display());
        }
        None => {
            cfg.projects.push(entry);
            println!("Added \"{name}\" to {}", rn_config_path().display());
        }
    }

    if let Err(e) = save_rn_config(&cfg) {
        eprintln!("{e}");
        std::process::exit(1);
    }

    // Show what will run, so the user knows what to tweak in rn.json.
    let p = cfg.projects.iter().find(|p| p.root == root).unwrap();
    println!("\nResolved commands ({}):", p.package_manager().as_str());
    println!("  metro   → {}  (:{})", p.metro_command(), p.metro_port());
    println!("  ios     → {}", p.ios_command());
    println!("  android → {}", p.android_command());
    println!("\nEdit {} to customize commands, port, simulator/avd, or env.", rn_config_path().display());
}

/// `simon rn config` — print the config file path.
pub fn print_config_path() {
    println!("{}", rn_config_path().display());
    if load_rn_config().unwrap_or(None).map(|c: RnConfig| c.projects.is_empty()).unwrap_or(true) {
        eprintln!("(no projects configured yet — run `simon rn init`)");
    }
}
