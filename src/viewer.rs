use std::io::{self, Stdout, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{
    Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, QueueableCommand, SynchronizedUpdate};
use image::RgbImage;

use crate::capture;
use crate::input::VirtualPointer;
use crate::niri::OutputGeometry;
use crate::render::{self, Viewport, ViewportRect};

const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 32.0;
const ZOOM_STEP: f32 = 1.25;
const PAN_VIEWPORT_FRACTION: f32 = 0.2;

pub fn run(
    runtime_dir: &Path,
    wayland_display: &str,
    output_name: &str,
    output_geometry: OutputGeometry,
    control: bool,
    initial_viewport: Viewport,
) -> Result<()> {
    let mut state = ViewerState::new(initial_viewport, control)?;
    let mut frame = capture::capture_with_grim(runtime_dir, wayland_display, Some(output_name))?;
    validate_output_geometry(&frame, &output_geometry)?;
    let pointer = control
        .then(|| VirtualPointer::connect(runtime_dir, wayland_display, output_name))
        .transpose()?;
    let mut terminal = Terminal::enter()?;
    let mut layout = terminal.draw(&frame, output_name, &state)?;

    loop {
        match event::read().context("cannot read a terminal event")? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match state.handle_key(key) {
                    Effect::Quit => break,
                    Effect::Refresh => {
                        match capture::capture_with_grim(
                            runtime_dir,
                            wayland_display,
                            Some(output_name),
                        ) {
                            Ok(new_frame) => {
                                frame = new_frame;
                                state.notice = Some("frame refreshed".into());
                            }
                            Err(error) => state.notice = Some(format!("refresh failed: {error:#}")),
                        }
                        layout = terminal.draw(&frame, output_name, &state)?;
                    }
                    Effect::Redraw => layout = terminal.draw(&frame, output_name, &state)?,
                    Effect::None => {}
                }
            }
            Event::Mouse(mouse) => {
                if let Some(point) = map_left_click(mouse, layout, &output_geometry) {
                    if state.control && state.armed {
                        if let Some(pointer) = &pointer {
                            match pointer.click(
                                point.local_x,
                                point.local_y,
                                output_geometry.width,
                                output_geometry.height,
                            ) {
                                Ok(()) => {
                                    state.notice = Some(format!(
                                        "clicked {}:{} (global {},{})",
                                        point.local_x,
                                        point.local_y,
                                        point.global_x,
                                        point.global_y
                                    ));
                                    thread::sleep(Duration::from_millis(75));
                                    if let Ok(new_frame) = capture::capture_with_grim(
                                        runtime_dir,
                                        wayland_display,
                                        Some(output_name),
                                    ) {
                                        frame = new_frame;
                                    }
                                }
                                Err(error) => {
                                    state.notice = Some(format!("click failed: {error:#}"));
                                }
                            }
                        }
                    } else {
                        state.notice = Some(format!(
                            "preview {}:{} (global {},{}); {}",
                            point.local_x,
                            point.local_y,
                            point.global_x,
                            point.global_y,
                            if state.control {
                                "press i to arm"
                            } else {
                                "view-only"
                            }
                        ));
                    }
                    layout = terminal.draw(&frame, output_name, &state)?;
                }
            }
            Event::Resize(_, _) => layout = terminal.draw(&frame, output_name, &state)?,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ViewerState {
    viewport: Viewport,
    notice: Option<String>,
    control: bool,
    armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    None,
    Redraw,
    Refresh,
    Quit,
}

impl ViewerState {
    fn new(viewport: Viewport, control: bool) -> Result<Self> {
        render::validate_viewport(viewport)?;
        let mut state = Self {
            viewport,
            notice: None,
            control,
            armed: false,
        };
        state.clamp_center();
        Ok(state)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Effect::Quit;
        }

        let effect = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Effect::Quit,
            KeyCode::Char('r') => Effect::Refresh,
            KeyCode::Char('i') if self.control => {
                self.armed = !self.armed;
                self.notice = Some(if self.armed {
                    "CONTROL ARMED: left click will control the desktop".into()
                } else {
                    "control disarmed".into()
                });
                Effect::Redraw
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.set_zoom((self.viewport.zoom * ZOOM_STEP).min(MAX_ZOOM))
            }
            KeyCode::Char('-') => self.set_zoom((self.viewport.zoom / ZOOM_STEP).max(MIN_ZOOM)),
            KeyCode::Char(digit @ '1'..='9') => self.set_zoom(digit.to_digit(10).unwrap() as f32),
            KeyCode::Char('0') => self.reset_overview(),
            KeyCode::Char('c') => self.center(),
            KeyCode::Left | KeyCode::Char('h') => self.pan(-1.0, 0.0),
            KeyCode::Right | KeyCode::Char('l') => self.pan(1.0, 0.0),
            KeyCode::Up | KeyCode::Char('k') => self.pan(0.0, -1.0),
            KeyCode::Down | KeyCode::Char('j') => self.pan(0.0, 1.0),
            _ => Effect::None,
        };
        if effect == Effect::Redraw && !matches!(key.code, KeyCode::Char('i')) {
            self.notice = None;
        }
        effect
    }

    fn set_zoom(&mut self, zoom: f32) -> Effect {
        if (self.viewport.zoom - zoom).abs() < f32::EPSILON {
            return Effect::None;
        }
        self.viewport.zoom = zoom;
        self.clamp_center();
        Effect::Redraw
    }

    fn reset_overview(&mut self) -> Effect {
        if (self.viewport.zoom - MIN_ZOOM).abs() < f32::EPSILON
            && (self.viewport.center_x - 0.5).abs() < f32::EPSILON
            && (self.viewport.center_y - 0.5).abs() < f32::EPSILON
        {
            return Effect::None;
        }
        self.viewport.zoom = MIN_ZOOM;
        self.viewport.center_x = 0.5;
        self.viewport.center_y = 0.5;
        Effect::Redraw
    }

    fn center(&mut self) -> Effect {
        if (self.viewport.center_x - 0.5).abs() < f32::EPSILON
            && (self.viewport.center_y - 0.5).abs() < f32::EPSILON
        {
            return Effect::None;
        }
        self.viewport.center_x = 0.5;
        self.viewport.center_y = 0.5;
        Effect::Redraw
    }

    fn pan(&mut self, x: f32, y: f32) -> Effect {
        if self.viewport.zoom <= MIN_ZOOM {
            return Effect::None;
        }
        let previous = (self.viewport.center_x, self.viewport.center_y);
        let step = PAN_VIEWPORT_FRACTION / self.viewport.zoom;
        self.viewport.center_x += x * step;
        self.viewport.center_y += y * step;
        self.clamp_center();
        if previous == (self.viewport.center_x, self.viewport.center_y) {
            Effect::None
        } else {
            Effect::Redraw
        }
    }

    fn clamp_center(&mut self) {
        let margin = 0.5 / self.viewport.zoom;
        self.viewport.center_x = self.viewport.center_x.clamp(margin, 1.0 - margin);
        self.viewport.center_y = self.viewport.center_y.clamp(margin, 1.0 - margin);
    }
}

struct Terminal {
    stdout: Stdout,
}

impl Terminal {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("cannot enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = stdout.execute(EnterAlternateScreen).and_then(|stdout| {
            stdout.execute(Hide)?;
            stdout.execute(EnableMouseCapture)?;
            stdout.execute(crossterm::terminal::DisableLineWrap)?;
            stdout.execute(Clear(ClearType::All))?;
            stdout.execute(MoveTo(0, 0))
        }) {
            let _ = disable_raw_mode();
            return Err(error).context("cannot initialize the interactive terminal");
        }
        Ok(Self { stdout })
    }

    fn draw(
        &mut self,
        frame: &RgbImage,
        output_name: &str,
        state: &ViewerState,
    ) -> Result<DrawLayout> {
        let (cols, rows) = crossterm::terminal::size()?;
        let image_rows = rows.saturating_sub(1).max(1);
        let rendered =
            render::render_half_blocks_viewport(frame, cols.max(1), image_rows, state.viewport)?;
        let status_y = rendered.rows.min(rows.saturating_sub(1));
        let status = state.notice.clone().unwrap_or_else(|| {
            let mode = if !state.control {
                "VIEW"
            } else if state.armed {
                "CONTROL:ARMED"
            } else {
                "CONTROL:OFF"
            };
            format!(
                " {mode} | {output_name} | {:.2}x @ {:.0}%,{:.0}% | click inspect  i arm  arrows pan  +/- zoom  r refresh  q quit ",
                state.viewport.zoom,
                state.viewport.center_x * 100.0,
                state.viewport.center_y * 100.0,
            )
        });
        let status = fit_status(&status, cols as usize);

        self.stdout.sync_update(|stdout| -> io::Result<()> {
            stdout.queue(MoveTo(0, 0))?;
            stdout.write_all(&rendered.bytes)?;
            stdout.queue(Clear(ClearType::FromCursorDown))?;
            stdout.queue(MoveTo(0, status_y))?;
            stdout.queue(SetForegroundColor(Color::Black))?;
            stdout.queue(SetBackgroundColor(Color::Grey))?;
            stdout.queue(Print(status))?;
            stdout.queue(ResetColor)?;
            Ok(())
        })??;
        Ok(DrawLayout {
            cols: rendered.cols,
            rows: rendered.rows,
            sample_height: rendered.sample_height,
            viewport: rendered.viewport,
            source_width: frame.width(),
            source_height: frame.height(),
        })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.stdout.execute(EndSynchronizedUpdate);
        let _ = self.stdout.execute(ResetColor);
        let _ = self.stdout.execute(DisableMouseCapture);
        let _ = self.stdout.execute(crossterm::terminal::EnableLineWrap);
        let _ = self.stdout.execute(Show);
        let _ = self.stdout.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[derive(Debug, Clone, Copy)]
struct DrawLayout {
    cols: u16,
    rows: u16,
    sample_height: u16,
    viewport: ViewportRect,
    source_width: u32,
    source_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogicalPoint {
    local_x: u32,
    local_y: u32,
    global_x: i64,
    global_y: i64,
}

fn map_left_click(
    mouse: MouseEvent,
    layout: DrawLayout,
    output: &OutputGeometry,
) -> Option<LogicalPoint> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left)
        || mouse.column >= layout.cols
        || mouse.row >= layout.rows
        || output.transform != "Normal"
    {
        return None;
    }

    let sample_x = f64::from(mouse.column) + 0.5;
    let sample_y = (f64::from(mouse.row) * 2.0 + 1.0).min(f64::from(layout.sample_height) - 0.5);
    let physical_x = f64::from(layout.viewport.x)
        + sample_x / f64::from(layout.cols) * f64::from(layout.viewport.width);
    let physical_y = f64::from(layout.viewport.y)
        + sample_y / f64::from(layout.sample_height) * f64::from(layout.viewport.height);
    let local_x = (physical_x / f64::from(layout.source_width) * f64::from(output.width))
        .floor()
        .clamp(0.0, f64::from(output.width.saturating_sub(1))) as u32;
    let local_y = (physical_y / f64::from(layout.source_height) * f64::from(output.height))
        .floor()
        .clamp(0.0, f64::from(output.height.saturating_sub(1))) as u32;
    Some(LogicalPoint {
        local_x,
        local_y,
        global_x: i64::from(output.x) + i64::from(local_x),
        global_y: i64::from(output.y) + i64::from(local_y),
    })
}

fn validate_output_geometry(frame: &RgbImage, output: &OutputGeometry) -> Result<()> {
    if output.transform != "Normal" {
        anyhow::bail!(
            "mouse mapping currently supports Normal output transforms, not {}",
            output.transform
        );
    }
    let expected_width = f64::from(output.width) * output.scale;
    let expected_height = f64::from(output.height) * output.scale;
    if (f64::from(frame.width()) - expected_width).abs() > 1.0
        || (f64::from(frame.height()) - expected_height).abs() > 1.0
    {
        anyhow::bail!(
            "capture {}x{} does not match niri geometry {}x{} at scale {}",
            frame.width(),
            frame.height(),
            output.width,
            output.height,
            output.scale
        );
    }
    Ok(())
}

fn fit_status(status: &str, width: usize) -> String {
    let mut fitted = status.chars().take(width).collect::<String>();
    let current = fitted.chars().count();
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(current)));
    fitted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn zooms_with_keys_and_resets_overview() {
        let mut state = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('5'))), Effect::Redraw);
        assert_eq!(state.viewport.zoom, 5.0);
        state.handle_key(key(KeyCode::Char('+')));
        assert_eq!(state.viewport.zoom, 6.25);
        state.handle_key(key(KeyCode::Char('0')));
        assert_eq!(state.viewport.zoom, 1.0);
        assert_eq!(state.viewport.center_x, 0.5);
    }

    #[test]
    fn boundary_zoom_and_overview_pan_are_no_ops() {
        let mut state = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('-'))), Effect::None);
        assert_eq!(state.handle_key(key(KeyCode::Left)), Effect::None);
        assert_eq!(state.handle_key(key(KeyCode::Char('0'))), Effect::None);

        state.viewport.zoom = MAX_ZOOM;
        assert_eq!(state.handle_key(key(KeyCode::Char('+'))), Effect::None);
    }

    #[test]
    fn pans_by_a_fraction_of_the_visible_viewport() {
        let mut state = ViewerState::new(
            Viewport {
                zoom: 5.0,
                ..Viewport::default()
            },
            false,
        )
        .unwrap();
        state.handle_key(key(KeyCode::Right));
        assert!((state.viewport.center_x - 0.54).abs() < f32::EPSILON);
        state.handle_key(key(KeyCode::Char('k')));
        assert!((state.viewport.center_y - 0.46).abs() < f32::EPSILON);
    }

    #[test]
    fn pan_stops_at_visible_viewport_edge() {
        let mut state = ViewerState::new(
            Viewport {
                zoom: 5.0,
                center_x: 0.0,
                center_y: 0.5,
            },
            false,
        )
        .unwrap();
        assert_eq!(state.viewport.center_x, 0.1);
        assert_eq!(state.handle_key(key(KeyCode::Left)), Effect::None);
    }

    #[test]
    fn recognizes_refresh_and_quit() {
        let mut state = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('r'))), Effect::Refresh);
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Effect::Quit);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Effect::Quit
        );
    }

    #[test]
    fn control_must_be_available_and_explicitly_armed() {
        let mut view_only = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(view_only.handle_key(key(KeyCode::Char('i'))), Effect::None);
        assert!(!view_only.armed);

        let mut control = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(control.handle_key(key(KeyCode::Char('i'))), Effect::Redraw);
        assert!(control.armed);
        assert_eq!(control.handle_key(key(KeyCode::Char('i'))), Effect::Redraw);
        assert!(!control.armed);
    }

    #[test]
    fn maps_pane_cell_through_viewport_to_output_coordinates() {
        let layout = DrawLayout {
            cols: 100,
            rows: 50,
            sample_height: 100,
            viewport: ViewportRect {
                x: 500,
                y: 250,
                width: 1000,
                height: 500,
            },
            source_width: 2000,
            source_height: 1000,
        };
        let output = OutputGeometry {
            name: "test".into(),
            x: -1600,
            y: 100,
            width: 1600,
            height: 800,
            scale: 1.25,
            transform: "Normal".into(),
        };
        let point = map_left_click(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 49,
                row: 24,
                modifiers: KeyModifiers::NONE,
            },
            layout,
            &output,
        )
        .unwrap();
        assert_eq!(
            point,
            LogicalPoint {
                local_x: 796,
                local_y: 396,
                global_x: -804,
                global_y: 496,
            }
        );
    }

    #[test]
    fn rejects_clicks_in_letterbox_status_and_non_normal_outputs() {
        let layout = DrawLayout {
            cols: 80,
            rows: 20,
            sample_height: 40,
            viewport: ViewportRect {
                x: 0,
                y: 0,
                width: 800,
                height: 400,
            },
            source_width: 800,
            source_height: 400,
        };
        let mut output = OutputGeometry {
            name: "test".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 400,
            scale: 1.0,
            transform: "Normal".into(),
        };
        let mouse = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(map_left_click(mouse(80, 0), layout, &output).is_none());
        assert!(map_left_click(mouse(0, 20), layout, &output).is_none());
        output.transform = "90".into();
        assert!(map_left_click(mouse(0, 0), layout, &output).is_none());
    }

    #[test]
    fn status_is_exact_terminal_width() {
        assert_eq!(fit_status("abc", 5), "abc  ");
        assert_eq!(fit_status("abcdef", 3), "abc");
        assert_eq!(fit_status("你好", 1), "你");
    }
}
