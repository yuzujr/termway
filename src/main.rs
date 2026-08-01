mod discovery;
mod niri;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "Control a Wayland desktop through an SSH terminal")]
struct Cli {
    /// Override niri's IPC socket discovery.
    #[arg(long, global = true, value_name = "PATH")]
    niri_socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Diagnose the graphical session and print a state snapshot.
    Doctor,
    /// Print a finite number of raw niri events.
    Events {
        #[arg(short, long, default_value_t = 10)]
        count: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let discovered = discovery::discover(cli.niri_socket.as_deref())?;

    match cli.command.unwrap_or(Command::Doctor) {
        Command::Doctor => doctor(discovered),
        Command::Events { count } => events(discovered.socket_path, count),
    }
}

fn doctor(discovered: discovery::GraphicalSession) -> Result<()> {
    let mut client = niri::Client::connect(&discovered.socket_path)?;
    let snapshot = client.snapshot()?;

    println!("termway doctor");
    println!("  runtime directory : {}", discovered.runtime_dir.display());
    println!("  niri socket       : {}", discovered.socket_path.display());
    println!("  socket source     : {}", discovered.source);
    println!(
        "  Wayland display   : {}",
        discovered
            .wayland_display
            .as_deref()
            .unwrap_or("not discovered")
    );
    println!("  niri version      : {}", snapshot.version);
    println!("  outputs           : {}", snapshot.output_count);
    println!("  windows           : {}", snapshot.window_count);
    println!(
        "  focused window    : {}",
        snapshot.focused_window.as_deref().unwrap_or("none")
    );

    let event = niri::probe_event_stream(&discovered.socket_path, Duration::from_secs(2))?;
    println!("  event stream      : ok ({})", niri::event_name(&event));
    println!("result: ready for capture spike");
    Ok(())
}

fn events(socket_path: PathBuf, count: usize) -> Result<()> {
    for event in niri::read_events(&socket_path, count)? {
        println!("{}", serde_json::to_string(&event)?);
    }
    Ok(())
}
