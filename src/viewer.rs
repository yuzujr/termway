use std::io::{self, Stdout, Write};
use std::path::Path;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{
    Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, QueueableCommand, SynchronizedUpdate};
use image::RgbImage;

use crate::capture;
use crate::render::{self, Viewport};

const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 32.0;
const ZOOM_STEP: f32 = 1.25;
const PAN_VIEWPORT_FRACTION: f32 = 0.2;

pub fn run(
    runtime_dir: &Path,
    wayland_display: &str,
    output_name: &str,
    initial_viewport: Viewport,
) -> Result<()> {
    let mut state = ViewerState::new(initial_viewport)?;
    let mut frame = capture::capture_with_grim(runtime_dir, wayland_display, Some(output_name))?;
    let mut terminal = Terminal::enter()?;
    terminal.draw(&frame, output_name, &state)?;

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
                        terminal.draw(&frame, output_name, &state)?;
                    }
                    Effect::Redraw => terminal.draw(&frame, output_name, &state)?,
                    Effect::None => {}
                }
            }
            Event::Resize(_, _) => terminal.draw(&frame, output_name, &state)?,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ViewerState {
    viewport: Viewport,
    notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    None,
    Redraw,
    Refresh,
    Quit,
}

impl ViewerState {
    fn new(viewport: Viewport) -> Result<Self> {
        render::validate_viewport(viewport)?;
        let mut state = Self {
            viewport,
            notice: None,
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
        if effect == Effect::Redraw {
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
            stdout.execute(crossterm::terminal::DisableLineWrap)?;
            stdout.execute(Clear(ClearType::All))?;
            stdout.execute(MoveTo(0, 0))
        }) {
            let _ = disable_raw_mode();
            return Err(error).context("cannot initialize the interactive terminal");
        }
        Ok(Self { stdout })
    }

    fn draw(&mut self, frame: &RgbImage, output_name: &str, state: &ViewerState) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size()?;
        let image_rows = rows.saturating_sub(1).max(1);
        let rendered =
            render::render_half_blocks_viewport(frame, cols.max(1), image_rows, state.viewport)?;
        let status_y = rendered.rows.min(rows.saturating_sub(1));
        let status = state.notice.clone().unwrap_or_else(|| {
            format!(
                " {output_name} | {:.2}x @ {:.0}%,{:.0}% | arrows/hjkl pan  +/- zoom  1-9 preset  r refresh  q quit ",
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
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.stdout.execute(EndSynchronizedUpdate);
        let _ = self.stdout.execute(ResetColor);
        let _ = self.stdout.execute(crossterm::terminal::EnableLineWrap);
        let _ = self.stdout.execute(Show);
        let _ = self.stdout.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
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
        let mut state = ViewerState::new(Viewport::default()).unwrap();
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
        let mut state = ViewerState::new(Viewport::default()).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('-'))), Effect::None);
        assert_eq!(state.handle_key(key(KeyCode::Left)), Effect::None);
        assert_eq!(state.handle_key(key(KeyCode::Char('0'))), Effect::None);

        state.viewport.zoom = MAX_ZOOM;
        assert_eq!(state.handle_key(key(KeyCode::Char('+'))), Effect::None);
    }

    #[test]
    fn pans_by_a_fraction_of_the_visible_viewport() {
        let mut state = ViewerState::new(Viewport {
            zoom: 5.0,
            ..Viewport::default()
        })
        .unwrap();
        state.handle_key(key(KeyCode::Right));
        assert!((state.viewport.center_x - 0.54).abs() < f32::EPSILON);
        state.handle_key(key(KeyCode::Char('k')));
        assert!((state.viewport.center_y - 0.46).abs() < f32::EPSILON);
    }

    #[test]
    fn pan_stops_at_visible_viewport_edge() {
        let mut state = ViewerState::new(Viewport {
            zoom: 5.0,
            center_x: 0.0,
            center_y: 0.5,
        })
        .unwrap();
        assert_eq!(state.viewport.center_x, 0.1);
        assert_eq!(state.handle_key(key(KeyCode::Left)), Effect::None);
    }

    #[test]
    fn recognizes_refresh_and_quit() {
        let mut state = ViewerState::new(Viewport::default()).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('r'))), Effect::Refresh);
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Effect::Quit);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Effect::Quit
        );
    }

    #[test]
    fn status_is_exact_terminal_width() {
        assert_eq!(fit_status("abc", 5), "abc  ");
        assert_eq!(fit_status("abcdef", 3), "abc");
        assert_eq!(fit_status("你好", 1), "你");
    }
}
