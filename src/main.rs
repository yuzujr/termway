mod capture;
mod config;
mod discovery;
mod idle;
mod input;
mod kitty;
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

    /// Read settings and actions from this file instead of the default XDG config path.
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
        /// Capture this output instead of the configured or focused output.
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
        /// View this output instead of the configured or focused output.
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

        /// Select terminal image rendering, probing known terminal environments in auto mode.
        #[arg(long, value_enum, default_value_t = kitty::GraphicsMode::Auto)]
        graphics: kitty::GraphicsMode,

        /// Pace Kitty image output inside tmux to this relay bandwidth.
        /// Overrides `graphics.advanced.tmux_bandwidth_mbps` from the config file.
        #[arg(long, value_name = "MBPS")]
        tmux_bandwidth_mbps: Option<f64>,
    },
    /// Render a deterministic Kitty transition for automated visual regression tests.
    #[command(hide = true)]
    GraphicsFixture {
        /// Deliberately pause after protocol queue segments to expose non-atomic transitions.
        #[arg(long, default_value_t = 20)]
        segment_delay_ms: u64,
    },
    /// Exercise the production Kitty atlas/refine quality path with a static test image.
    #[command(hide = true)]
    QualityFixture {
        #[arg(long, default_value_t = 40.0, value_name = "MBPS")]
        tmux_bandwidth_mbps: f64,

        /// Override preview dwell time to make stale-atlas regressions deterministic.
        #[arg(long, default_value_t = 120, value_name = "MILLISECONDS")]
        refine_delay_ms: u64,

        /// Override idle atlas refresh time to keep stale-atlas fixtures deterministic.
        #[arg(long, default_value_t = 2000, value_name = "MILLISECONDS")]
        atlas_refresh_ms: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::GraphicsFixture { segment_delay_ms }) = cli.command.as_ref() {
        return kitty::run_visual_fixture(Duration::from_millis(*segment_delay_ms));
    }
    if let Some(Command::QualityFixture {
        tmux_bandwidth_mbps,
        refine_delay_ms,
        atlas_refresh_ms,
    }) = cli.command.as_ref()
    {
        return viewer::run_quality_fixture(
            *tmux_bandwidth_mbps,
            Duration::from_millis(*refine_delay_ms),
            Duration::from_millis(*atlas_refresh_ms),
        );
    }
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
        } => capture(
            discovered,
            output.or_else(|| config.output.clone()),
            cols,
            rows,
            zoom,
            center_x,
            center_y,
        ),
        Command::View {
            output,
            zoom,
            center_x,
            center_y,
            control,
            graphics,
            tmux_bandwidth_mbps,
        } => {
            let mut graphics_config = config.graphics;
            if let Some(tmux_bandwidth_mbps) = tmux_bandwidth_mbps {
                graphics_config.advanced.tmux_bandwidth_mbps = tmux_bandwidth_mbps;
                graphics_config.validate()?;
            }
            view(
                discovered,
                ViewOptions {
                    output: output.or(config.output),
                    viewport: render::Viewport {
                        zoom,
                        center_x,
                        center_y,
                    },
                    control,
                    graphics,
                    graphics_config,
                    actions: config.actions,
                },
            )
        }
        Command::GraphicsFixture { .. } => {
            unreachable!("graphics fixture returned before discovery")
        }
        Command::QualityFixture { .. } => {
            unreachable!("quality fixture returned before discovery")
        }
    }
}

struct ViewOptions {
    output: Option<String>,
    viewport: render::Viewport,
    control: bool,
    graphics: kitty::GraphicsMode,
    graphics_config: config::GraphicsConfig,
    actions: Vec<config::Action>,
}

fn view(discovered: discovery::GraphicalSession, options: ViewOptions) -> Result<()> {
    let display = discovered
        .wayland_display
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("could not discover WAYLAND_DISPLAY"))?;
    let output = resolve_output(&discovered, options.output)?;
    let geometry = niri::Client::connect(&discovered.socket_path)?.output_geometry(&output)?;
    viewer::run(
        &discovered.runtime_dir,
        display,
        &output,
        geometry,
        viewer::RunOptions {
            control: options.control,
            graphics: options.graphics,
            graphics_config: options.graphics_config,
            initial_viewport: options.viewport,
            actions: options.actions,
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
    let focused_output = client.focused_output_name()?;
    let outputs = client.output_geometries()?;

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
    println!(
        "  outputs           : {} ({} enabled)",
        snapshot.output_count,
        outputs.len()
    );
    for output in outputs {
        let focused = if focused_output.as_deref() == Some(output.name.as_str()) {
            " focused"
        } else {
            ""
        };
        println!(
            "    {}{}: {}x{} at {:+},{:+}, scale {}, transform {}",
            output.name,
            focused,
            output.width,
            output.height,
            output.x,
            output.y,
            output.scale,
            output.transform
        );
    }
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
