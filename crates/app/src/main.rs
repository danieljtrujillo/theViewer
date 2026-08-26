//! The viewer binary.
//!
//! `viewer` with no arguments opens the window. `viewer <path>` opens the window on that file.
//! `viewer identify <path>...` prints format identification and exits.

#![deny(unsafe_code)]
// The shipped build detaches from the console so a double-click does not flash a terminal.
// `--features console` keeps it attached, which is what the identify and info commands need.
#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]

mod cli;
mod player;
mod shell;
mod shot;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("identify") => cli::identify(&args[1..]),
        Some("info") => cli::info(&args[1..]),
        Some("shot") => cli::shot(&args[1..]),
        Some("open-with") => cli::open_with(&args[1..]),
        Some("--help") | Some("-h") => {
            cli::usage();
            ExitCode::SUCCESS
        }
        _ => {
            let initial = args.first().map(PathBuf::from).filter(|p| p.is_file());
            match ui::run(initial) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("viewer: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
