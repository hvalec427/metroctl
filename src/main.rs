//! metroctl — a single-window React Native project dashboard (Metro, device
//! build/run, JS logs/network/perf), built on top of `simon` for device
//! management. No subcommand opens the dashboard; `logs` is the standalone
//! React Native log viewer; `init`/`config` manage the project registry.

mod commands_rn;
mod devinfo;
mod metro_events;
mod proc;
mod rn;
mod rnclient;
mod rnconfig;
mod rndash;
mod rntui;
mod rnview;
mod session;
mod update;

use clap::{Parser, Subcommand};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(name = "metroctl", version, about = "React Native project dashboard")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Register (or update) the current directory as a React Native project
    Init,
    /// Print the path to the config file
    Config,
    /// Open the dashboard set up for this checkout: pick a port, create/boot a
    /// simulator, start Metro and build onto it
    Up {
        /// Metro port, or `auto` for the first free one from the configured port
        #[arg(long)]
        port: Option<String>,
        /// Pin to an existing device (udid); booted if it's a simulator
        #[arg(long, conflicts_with = "new_sim")]
        device: Option<String>,
        /// Create a new simulator for this session (default name: metroctl-<dir>)
        #[arg(long = "new-sim", value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        new_sim: Option<String>,
        /// Device type for --new-sim, e.g. "iPhone 16 Pro" (default: newest plain iPhone)
        #[arg(long = "sim-type", requires = "new_sim")]
        sim_type: Option<String>,
        /// iOS runtime for --new-sim, e.g. 18.2 (default: newest installed)
        #[arg(long = "sim-runtime", requires = "new_sim")]
        sim_runtime: Option<String>,
        /// What to do with the created simulator when you quit
        #[arg(long = "sim-cleanup", value_enum, default_value = "ask")]
        sim_cleanup: session::SimCleanup,
        /// Install JS deps and pods before starting Metro
        #[arg(long)]
        install: bool,
    },
    /// Stop the metroctl session running in this checkout and delete the simulator it created
    Down {
        /// Keep the simulator
        #[arg(long = "keep-sim")]
        keep_sim: bool,
    },
    /// Stream React Native JS console + network from Metro (CDP)
    Logs {
        name: Option<String>,
        /// Metro port (default 8081)
        #[arg(long)]
        port: Option<u16>,
        /// Print the Metro inspector WebSocket URL(s) and exit
        #[arg(long = "print-ws")]
        print_ws: bool,
    },
    /// Print the bundler events of a Metro started elsewhere (dashboard helper)
    #[command(name = "metro-events", hide = true)]
    MetroEvents {
        #[arg(long, default_value_t = 8081)]
        port: u16,
    },
    /// Check whether a newer version of metroctl is available
    #[command(name = "check-update")]
    CheckUpdate {
        #[arg(long)]
        stable: bool,
        #[arg(long)]
        nightly: bool,
        #[arg(long)]
        dev: bool,
    },
    /// Update metroctl to the latest version (stays on the installed build's channel)
    Update {
        #[arg(long)]
        stable: bool,
        #[arg(long)]
        nightly: bool,
        #[arg(long)]
        dev: bool,
        /// Install the channel's latest even if it's the same or an older version
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        None => commands_rn::launch(session::UpOpts::default()),
        Some(Command::Up { port, device, new_sim, sim_type, sim_runtime, sim_cleanup, install }) => {
            let port = match port.as_deref() {
                None => None,
                Some("auto") => Some(None),
                Some(p) => match p.parse() {
                    Ok(n) => Some(Some(n)),
                    Err(_) => {
                        eprintln!("--port must be a number or `auto`");
                        std::process::exit(2);
                    }
                },
            };
            let new_sim = new_sim.map(|n| Some(n).filter(|n| !n.is_empty()));
            commands_rn::launch(session::UpOpts { port, device, new_sim, sim_type, sim_runtime, sim_cleanup: Some(sim_cleanup), install })
        }
        Some(Command::Down { keep_sim }) => commands_rn::down(keep_sim),
        Some(Command::Init) => commands_rn::init(),
        Some(Command::Config) => commands_rn::print_config_path(),
        Some(Command::Logs { name, port, print_ws }) => logs(name, port, print_ws),
        Some(Command::MetroEvents { port }) => metro_events::watch(port),
        Some(Command::CheckUpdate { stable, nightly, dev }) => update::check_update(stable, nightly, dev),
        Some(Command::Update { stable, nightly, dev, force }) => update::update(stable, nightly, dev, force),
    }
}

fn logs(name: Option<String>, port: Option<u16>, print_ws: bool) {
    let port = port.unwrap_or(8081);
    if print_ws {
        rn::print_inspector_ws(port, name.as_deref());
        return;
    }
    // Interactive TUI in a terminal; plain line stream when piped/redirected.
    let result = if std::io::stdout().is_terminal() {
        rntui::run(port, name.as_deref())
    } else {
        rn::stream_plain(port, name.as_deref())
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
