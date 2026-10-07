//! metroctl — a single-window React Native project dashboard (Metro, device
//! build/run, JS logs/network/perf), built on top of `simon` for device
//! management. No subcommand opens the dashboard; `logs` is the standalone
//! React Native log viewer; `init`/`config` manage the project registry.

mod commands_rn;
mod proc;
mod rn;
mod rnclient;
mod rnconfig;
mod rndash;
mod rntui;
mod rnview;

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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        None => commands_rn::launch(),
        Some(Command::Init) => commands_rn::init(),
        Some(Command::Config) => commands_rn::print_config_path(),
        Some(Command::Logs { name, port, print_ws }) => logs(name, port, print_ws),
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
