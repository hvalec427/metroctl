//! `metroctl` — manage and run a React Native project from a single window.
//! No subcommand launches the dashboard TUI; `init` registers the current
//! directory; `config` prints the config path.

use crate::rndash::Setup;
use crate::session::{self, Pinned, SimCleanup, UpOpts};
use crate::rnconfig::{current_project, load_rn_config, rn_config_path, save_rn_config, ProjectConfig, RnConfig};
use inquire::Text;

/// `metroctl` — launch the dashboard for the current project.
pub fn launch(opts: UpOpts) {
    let cfg = match load_rn_config() {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("No projects configured yet. Run `metroctl init` in your project directory.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let mut project = match current_project(&cfg) {
        Some(p) => p,
        None => {
            eprintln!("This directory isn't a registered React Native project.");
            eprintln!("Run `metroctl init` here to add it.");
            std::process::exit(1);
        }
    };
    let setup = match prepare(&mut project, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    };
    if let Err(e) = crate::rndash::run(project, setup) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Apply `up` options before the dashboard opens: the port override and the
/// device to pin (creating a simulator if asked).
fn prepare(project: &mut ProjectConfig, opts: &UpOpts) -> anyhow::Result<Setup> {
    for d in session::reap_stale() {
        eprintln!("{d}");
    }
    match opts.port {
        Some(Some(p)) => project.set_metro_port(p),
        Some(None) => project.set_metro_port(session::free_port(project.metro_port())?),
        None => {}
    }
    let cleanup = opts.sim_cleanup.unwrap_or(SimCleanup::Ask);
    let pinned = if let Some(name) = &opts.new_sim {
        let name = name.clone().unwrap_or_else(|| session::default_sim_name(std::path::Path::new(&project.root)));
        eprintln!("creating simulator {name}…");
        let (udid, desc) = session::create_simulator(&name, opts.sim_runtime.as_deref(), opts.sim_type.as_deref())?;
        eprintln!("created {desc} ({udid})");
        Some(Pinned { udid, created: true, simulator: true, android: false, cleanup, name: Some(name) })
    } else {
        opts.device.as_ref().map(|udid| {
            let info = session::sim_info(udid);
            let android = info.is_none() && session::adb_serials().contains(udid);
            Pinned { udid: udid.clone(), created: false, simulator: info.is_some(), android, cleanup, name: info.map(|i| i.1) }
        })
    };
    let up = opts.port.is_some() || pinned.is_some() || opts.install;
    Ok(Setup { pinned, install: opts.install, start: up })
}

/// `metroctl down` — stop the session running in this checkout.
pub fn down(keep_sim: bool) {
    let root = load_rn_config().ok().flatten().and_then(|c| current_project(&c)).map(|p| std::path::PathBuf::from(p.root));
    let root = root.or_else(|| std::env::current_dir().ok()).unwrap();
    if let Err(e) = session::down(&root, keep_sim) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

/// `metroctl init` — register (or update) the current directory as a project.
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

/// `metroctl config` — print the config file path.
pub fn print_config_path() {
    println!("{}", rn_config_path().display());
    if load_rn_config().unwrap_or(None).map(|c: RnConfig| c.projects.is_empty()).unwrap_or(true) {
        eprintln!("(no projects configured yet — run `metroctl init`)");
    }
}
