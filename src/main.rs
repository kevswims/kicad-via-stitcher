mod board;
mod commands;
mod config;
mod geometry;
mod ipc;
mod kicad_proto;
mod manifest;
mod place;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Stitching vias require a running KiCad with the board open: the IPC API is
/// a channel into the editor's in-memory board, not a file library.
#[derive(Parser)]
#[command(
    name = "via-stitcher",
    about = "Place and remove stitching vias on a KiCad copper plane",
    long_about = "Places a field of vias tying a copper plane (typically GND) together, \
                  driven by a TOML config. Requires KiCad to be running with the target \
                  board open and Preferences > Plugins > Enable IPC API turned on."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Place stitching vias according to the config.
    Stitch {
        #[arg(short, long, default_value = "stitch.toml")]
        config: PathBuf,
        /// Report what would be placed without changing the board.
        #[arg(long)]
        dry_run: bool,
        /// Remove the previous run's vias first instead of filling gaps around them.
        #[arg(long)]
        replace: bool,
        /// Refill zones before reading, rather than refusing on stale fill.
        #[arg(long)]
        refill_first: bool,
        /// Save the board afterwards instead of leaving it dirty for review.
        #[arg(long)]
        save: bool,
    },
    /// Remove the vias a previous stitch run placed.
    Clear {
        #[arg(short, long, default_value = "stitch.toml")]
        config: PathBuf,
        #[arg(long)]
        save: bool,
    },
    /// Check what the live KiCad instance supports. Changes nothing.
    Probe,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Stitch {
            config,
            dry_run,
            replace,
            refill_first,
            save,
        } => commands::stitch::run(&commands::stitch::Options {
            config_path: config,
            dry_run,
            replace,
            refill_first,
            save,
        }),
        Command::Clear { config, save } => commands::clear::run(&commands::clear::Options {
            config_path: config,
            save,
        }),
        Command::Probe => commands::probe::run(),
    }
}
