#![cfg_attr(all(windows, feature = "silent-launch"), windows_subsystem = "windows")]

#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod hotkeys;
mod media;
mod prores;
mod replay;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Capture a ProRes HQ test movie or record and save H.264 replays."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// Open the recorder settings window.
    #[cfg(windows)]
    Gui,
    /// Continuously buffer ProRes. F5 saves 30 seconds; Ctrl+Alt+F10 quits.
    #[cfg(windows)]
    ProresReplay {
        #[arg(long, default_value_t = 0)]
        display: u32,
        #[arg(long, default_value = "clips")]
        dir: PathBuf,
        /// Encoding workers. Defaults to half the available logical CPUs, up to six.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64))]
        threads: Option<u32>,
        /// Maximum output height, preserving aspect ratio. Zero uses native size.
        #[arg(long, default_value_t=0, value_parser=clap::value_parser!(u32).range(0..=2160))]
        height: u32,
        #[arg(long, default_value="60", value_parser=clap::value_parser!(u32).range(0..=1000))]
        fps: u32,
        /// Target ProRes bitrate in Mbps. Zero uses the automatic HQ target.
        #[arg(long, default_value_t=0, value_parser=clap::value_parser!(u32).range(0..=2000))]
        bitrate: u32,
    },
    /// Capture one second at 60 fps and encode a ProRes 422 HQ MOV in Rust.
    Prores {
        /// Zero-based DXGI output index on the default adapter.
        #[arg(long, default_value_t = 0, conflicts_with = "test_pattern")]
        display: u32,
        /// Encode moving color bars and a gray ramp for decoder and editor checks.
        #[arg(long)]
        test_pattern: bool,
        /// Destination MOV. Defaults to a new timestamped file under clips/.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Record a video-only MP4.
    Record {
        /// Number of seconds to record.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=3600))]
        seconds: u32,
        /// Zero-based DXGI display index on the capture adapter.
        #[arg(long, default_value_t = 0)]
        display: u32,
        /// Destination MP4. Defaults to a new timestamped file under clips/.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Buffer recent video. Press Ctrl+Alt+F9 to save or Ctrl+Alt+F10 to quit.
    Replay {
        #[arg(long, default_value_t = 0)]
        display: u32,
        #[arg(long, default_value = "clips")]
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    #[cfg(windows)]
    let default = Mode::Gui;
    #[cfg(not(windows))]
    let default = Mode::Record {
        seconds: 10,
        display: 0,
        output: None,
    };
    let command = Cli::parse().command.unwrap_or(default);
    let result = match command {
        #[cfg(windows)]
        Mode::Gui => gui::run(),
        #[cfg(windows)]
        Mode::ProresReplay {
            display,
            dir,
            threads,
            height,
            fps,
            bitrate,
        } => prores::replay::run(
            display,
            &dir,
            threads,
            prores::replay::Settings {
                height,
                fps,
                bitrate,
            },
        ),
        Mode::Prores {
            display,
            test_pattern,
            output,
        } => {
            let path = output.unwrap_or_else(|| {
                media::clip_path(std::path::Path::new("clips")).with_extension("mov")
            });
            prores::record(display, test_pattern, &path)
        }
        Mode::Record {
            seconds,
            display,
            output,
        } => {
            let path = output.unwrap_or_else(|| media::clip_path(std::path::Path::new("clips")));
            media::record(display, seconds, &path)
        }
        Mode::Replay { display, dir } => replay::run(display, &dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
