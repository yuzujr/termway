mod capture;
mod config;
mod discovery;
mod idle;
mod input;
mod niri;
mod render;
mod screencopy;
mod viewer;

use std::io::{self, Write};
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

    /// Read actions from this file instead of the default XDG config path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

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
    /// Capture one frame and render it with truecolor half-blocks.
    Capture {
        /// Capture this output instead of niri's focused output.
        #[arg(short, long)]
        output: Option<String>,

        /// Override the terminal width in cells.
        #[arg(long)]
        cols: Option<u16>,

        /// Override the image height in terminal rows.
        #[arg(long)]
        rows: Option<u16>,

        /// Magnify the captured output around the selected center point.
        #[arg(short, long, default_value_t = 1.0)]
        zoom: f32,

        /// Horizontal viewport center from 0.0 (left) to 1.0 (right).
        #[arg(long, default_value_t = 0.5)]
        center_x: f32,

        /// Vertical viewport center from 0.0 (top) to 1.0 (bottom).
        #[arg(long, default_value_t = 0.5)]
        center_y: f32,
    },
    /// Interactively pan and zoom a captured output.
    View {
        /// View this output instead of niri's focused output.
        #[arg(short, long)]
        output: Option<String>,

        /// Initial magnification.
        #[arg(short, long, default_value_t = 1.0)]
        zoom: f32,

        /// Initial horizontal center from 0.0 (left) to 1.0 (right).
        #[arg(long, default_value_t = 0.5)]
        center_x: f32,

        /// Initial vertical center from 0.0 (top) to 1.0 (bottom).
        #[arg(long, default_value_t = 0.5)]
        center_y: f32,

        /// Allow mouse clicks after explicitly arming control with `i`.
        #[arg(long)]
        control: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load(cli.config.as_deref())?;
    let discovered = discovery::discover(cli.niri_socket.as_deref())?;

    match cli.command.unwrap_or(Command::Doctor) {
        Command::Doctor => doctor(discovered),
        Command::Events { count } => events(discovered.socket_path, count),
        Command::Capture {
            output,
            cols,
            rows,
            zoom,
            center_x,
            center_y,
        } => capture(discovered, output, cols, rows, zoom, center_x, center_y),
        Command::View {
            output,
            zoom,
            center_x,
            center_y,
            control,
        } => view(
            discovered,
            output,
            zoom,
            center_x,
            center_y,
            control,
            config.actions,
        ),
    }
}

fn view(
    discovered: discovery::GraphicalSession,
    output: Option<String>,
    zoom: f32,
    center_x: f32,
    center_y: f32,
    control: bool,
    actions: Vec<config::Action>,
) -> Result<()> {
    let display = discovered
        .wayland_display
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("could not discover WAYLAND_DISPLAY"))?;
    let output = resolve_output(&discovered, output)?;
    let geometry = niri::Client::connect(&discovered.socket_path)?.output_geometry(&output)?;
    viewer::run(
        &discovered.runtime_dir,
        display,
        &output,
        geometry,
        control,
        render::Viewport {
            zoom,
            center_x,
            center_y,
        },
        viewer::ActionOptions {
            actions,
            niri_socket: &discovered.socket_path,
            environment: &discovered.action_environment,
        },
    )
}

fn capture(
    discovered: discovery::GraphicalSession,
    output: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    zoom: f32,
    center_x: f32,
    center_y: f32,
) -> Result<()> {
    let display = discovered
        .wayland_display
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("could not discover WAYLAND_DISPLAY"))?;
    let output = resolve_output(&discovered, output)?;

    let terminal_size = crossterm::terminal::size().unwrap_or((100, 30));
    let cols = cols.unwrap_or(terminal_size.0).max(1);
    let rows = rows
        .unwrap_or_else(|| terminal_size.1.saturating_sub(1))
        .max(1);

    let started = std::time::Instant::now();
    let mut capturer = capture::Capturer::new(&discovered.runtime_dir, display, &output);
    let frame = capturer.capture()?;
    let capture_elapsed = started.elapsed();

    let started = std::time::Instant::now();
    let rendered = render::render_half_blocks_viewport(
        &frame,
        cols,
        rows,
        render::Viewport {
            zoom,
            center_x,
            center_y,
        },
    )?;
    let render_elapsed = started.elapsed();

    io::stdout().write_all(&rendered.bytes)?;
    io::stdout().flush()?;
    eprintln!(
        "termway: output={output}, backend={}, capture={}x{} in {:.1?}, viewport={}x{}+{},{} zoom={:.2}x, render={}x{} cells/{} bytes in {:.1?}",
        capturer.backend_name(),
        frame.width(),
        frame.height(),
        capture_elapsed,
        rendered.viewport.width,
        rendered.viewport.height,
        rendered.viewport.x,
        rendered.viewport.y,
        zoom,
        rendered.cols,
        rendered.rows,
        rendered.bytes.len(),
        render_elapsed,
    );
    Ok(())
}

fn resolve_output(
    discovered: &discovery::GraphicalSession,
    output: Option<String>,
) -> Result<String> {
    match output {
        Some(output) => Ok(output),
        None => {
            let mut client = niri::Client::connect(&discovered.socket_path)?;
            client
                .focused_output_name()?
                .ok_or_else(|| anyhow::anyhow!("niri has no focused output; pass --output"))
        }
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
