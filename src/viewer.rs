use std::ffi::OsString;
use std::io::{self, Stdout, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use std::{collections::VecDeque, mem};

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal::{
    Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, QueueableCommand, SynchronizedUpdate};
use image::RgbImage;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;

use crate::capture;
use crate::config::{Action, ActionRunner, GraphicsConfig, QualityMode};
use crate::idle::IdleInhibitor;
use crate::input::{PointerButton, VirtualKeyboard, VirtualPointer};
use crate::kitty::{self, GraphicsMode};
use crate::niri::OutputGeometry;
use crate::render::{self, Viewport, ViewportRect};

const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 32.0;
const ZOOM_STEP: f32 = 1.25;
const PAN_VIEWPORT_FRACTION: f32 = 0.2;
const MESSAGE_DURATION: Duration = Duration::from_secs(2);
const ERROR_DURATION: Duration = Duration::from_secs(5);
const SCROLL_BATCH_WINDOW: Duration = Duration::from_millis(12);
const SCROLL_GESTURE_TIMEOUT: Duration = Duration::from_millis(80);
const SCROLL_STEP_SCALE: f32 = 0.25;
const MAX_SCROLL_STEPS_PER_FRAME: i32 = 4;
const AXIS_SWITCH_THRESHOLD: i32 = 2;
const AUTO_REFRESH_DELAY: Duration = Duration::from_millis(250);
const CLICK_FOCUS_FRACTION: f32 = 0.4;
const DAMAGE_POLL_INTERVAL: Duration = Duration::from_millis(40);
const KITTY_TILE_SIZE: u32 = 128;
const KITTY_COLOR_BITS: u8 = 7;
const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_millis(1);
// Keep tmux's eager pane reader from building a multi-megabyte client backlog. This is below a
// 50 Mbit/s relay while remaining fast enough to deliver a typical UI frame in well under a
// second. Direct SSH already gets real backpressure from its channel and is left unrestricted.
const TMUX_GRAPHICS_BURST_BYTES: f64 = 16.0 * 1024.0;

pub struct RunOptions<'a> {
    pub control: bool,
    pub graphics: GraphicsMode,
    pub graphics_config: GraphicsConfig,
    pub initial_viewport: Viewport,
    pub actions: Vec<Action>,
    pub niri_socket: &'a Path,
    pub environment: &'a [(OsString, OsString)],
}

/// Developer-only end-to-end fixture for Kitty quality and stale-atlas navigation policy.
pub fn run_quality_fixture(
    tmux_bandwidth_mbps: f64,
    refine_delay: Duration,
    atlas_refresh_delay: Duration,
) -> Result<()> {
    let mut frame = RgbImage::from_fn(2560, 1600, |x, y| {
        // Small deterministic noise blocks keep this fixture visually detailed and, crucially,
        // expensive to PNG-compress. Flat colors and regular checkerboards made multi-megabyte
        // output scheduling regressions invisible to the end-to-end test.
        let block_x = x / 3;
        let block_y = y / 3;
        let mut value = block_x.wrapping_mul(0x9e37_79b1)
            ^ block_y.wrapping_mul(0x85eb_ca77)
            ^ block_x.wrapping_add(block_y).rotate_left(13);
        value ^= value >> 16;
        value = value.wrapping_mul(0x7feb_352d);
        value ^= value >> 15;
        value = value.wrapping_mul(0x846c_a68b);
        value ^= value >> 16;
        image::Rgb([value as u8, (value >> 8) as u8, (value >> 16) as u8])
    });
    let mut state = ViewerState::new(Viewport::default(), false)?;
    let mut graphics_config = GraphicsConfig::default();
    graphics_config.advanced.tmux_bandwidth_mbps = tmux_bandwidth_mbps;
    graphics_config.advanced.preview_ms = refine_delay.as_millis().try_into().unwrap_or(u64::MAX);
    graphics_config.advanced.atlas_refresh_ms = atlas_refresh_delay
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    // The quality fixture intentionally exercises adaptive tiers on navigation.
    graphics_config.quality = QualityMode::Fast;
    let mut terminal = Terminal::enter(GraphicsMode::Kitty, graphics_config)?;
    state.graphics_backend = terminal.backend_name();
    let mut layout = terminal.draw(&frame, "quality-fixture", &state)?;

    loop {
        let output_progress = terminal.pump_output()?;
        if terminal.take_deferred_redraw() {
            layout = terminal.draw(&frame, "quality-fixture", &state)?;
        }
        let timeout = terminal.next_wakeup_timeout();
        let timeout = if terminal.has_pending_output() {
            let retry = if output_progress {
                Duration::ZERO
            } else {
                OUTPUT_RETRY_INTERVAL
            };
            Some(timeout.map_or(retry, |timeout| timeout.min(retry)))
        } else {
            timeout
        };
        if let Some(timeout) = timeout
            && !event::poll(timeout).context("cannot poll quality fixture input")?
        {
            if terminal.take_due_refine() {
                layout = terminal.draw(&frame, "quality-fixture", &state)?;
            }
            continue;
        }

        match event::read().context("cannot read quality fixture input")? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.code == KeyCode::Char('d') {
                    // Turn the cached atlas stale while putting an unmistakable current frame on
                    // screen. The visual regression then navigates before the idle keyframe is
                    // rebuilt and verifies that this old atlas is never exposed.
                    frame = RgbImage::from_pixel(2560, 1600, image::Rgb([224, 32, 224]));
                    terminal.note_visible_damage();
                    layout = terminal.draw(&frame, "quality-fixture", &state)?;
                    continue;
                }
                if key.code == KeyCode::Char('0') {
                    // Deterministically reproduce the result of tmux's bandwidth adaptation:
                    // returning to 1x must not let a 360p candidate cover a fresh 1080p atlas.
                    terminal.kitty_quality.select_lowest();
                    terminal.kitty_quality_upgrade_at = None;
                }
                match state.handle_key(key) {
                    Effect::Quit => return Ok(()),
                    Effect::Redraw => {
                        layout = terminal.draw(&frame, "quality-fixture", &state)?;
                    }
                    Effect::Chrome => terminal.draw_chrome("quality-fixture", &state, layout)?,
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                terminal.invalidate_image();
                layout = terminal.draw(&frame, "quality-fixture", &state)?;
            }
            _ => {}
        }
    }
}

pub fn run(
    runtime_dir: &Path,
    wayland_display: &str,
    output_name: &str,
    output_geometry: OutputGeometry,
    options: RunOptions<'_>,
) -> Result<()> {
    let mut state = ViewerState::new(options.initial_viewport, options.control)?;
    state.actions = options.actions;
    let mut idle_inhibitor = if options.control {
        match IdleInhibitor::acquire() {
            Ok(inhibitor) => {
                state.idle_inhibited = true;
                Some(inhibitor)
            }
            Err(error) => {
                state.error(format!("Idle inhibit unavailable: {error:#}"));
                None
            }
        }
    } else {
        None
    };
    let action_runner = ActionRunner::new(
        runtime_dir,
        wayland_display,
        options.niri_socket,
        options.environment,
    );
    let mut capturer = capture::Capturer::new(runtime_dir, wayland_display, output_name);
    let mut frame = capturer.capture()?;
    if let Some(reason) = capturer.fallback_reason() {
        state.message(format!("Using grim fallback: {reason}"));
    }
    let mut damage_watcher = if capturer.backend_name() == "wlr-screencopy" {
        match capture::DamageWatcher::spawn(runtime_dir, wayland_display, output_name) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                state.message(format!("Live updates disabled: {error:#}"));
                None
            }
        }
    } else {
        None
    };
    validate_output_geometry(&frame, &output_geometry)?;
    let pointer = options
        .control
        .then(|| VirtualPointer::connect(runtime_dir, wayland_display, output_name))
        .transpose()?;
    let mut keyboard = options
        .control
        .then(|| VirtualKeyboard::connect(runtime_dir, wayland_display))
        .transpose()?;
    let mut terminal = Terminal::enter(options.graphics, options.graphics_config)?;
    state.graphics_backend = terminal.backend_name();
    if let Some(reason) = terminal.take_fallback_reason() {
        state.message(format!("Using ANSI graphics: {reason}"));
    }
    let mut layout = terminal.draw(&frame, output_name, &state)?;
    let mut pending_events = VecDeque::new();

    loop {
        let output_progress = terminal.pump_output()?;
        if let Some(watcher) = &damage_watcher {
            match watcher.take_latest() {
                Ok(Some(update)) => {
                    validate_output_geometry(&update.image, &output_geometry)?;
                    let redraw = damage_affects_viewport(&update.damage, layout.viewport)
                        && visible_region_changed(&frame, &update.image, layout.viewport);
                    frame = update.image;
                    if redraw {
                        state.cancel_auto_refresh();
                        terminal.note_visible_damage();
                        layout = terminal.draw(&frame, output_name, &state)?;
                    } else {
                        // The navigation atlas covers the entire output, not only the current
                        // zoomed viewport. Damage outside the viewport still makes it stale.
                        terminal.note_frame_damage();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    state.error(format!("Live updates disabled: {error:#}"));
                    terminal.draw_echo(&state)?;
                    damage_watcher = None;
                }
            }
        }
        if terminal.take_deferred_redraw() {
            layout = terminal.draw(&frame, output_name, &state)?;
        }
        let terminal_event = if let Some(pending) = pending_events.pop_front() {
            pending
        } else {
            let timeout =
                earliest_timeout(state.next_wakeup_timeout(), terminal.next_wakeup_timeout());
            let timeout = timeout.map_or_else(
                || damage_watcher.as_ref().map(|_| DAMAGE_POLL_INTERVAL),
                |timeout| {
                    Some(if damage_watcher.is_some() {
                        timeout.min(DAMAGE_POLL_INTERVAL)
                    } else {
                        timeout
                    })
                },
            );
            let timeout = if terminal.has_pending_output() {
                let retry = if output_progress {
                    Duration::ZERO
                } else {
                    OUTPUT_RETRY_INTERVAL
                };
                Some(timeout.map_or(retry, |timeout| timeout.min(retry)))
            } else {
                timeout
            };
            if let Some(timeout) = timeout
                && !event::poll(timeout).context("cannot poll terminal events")?
            {
                let refine = terminal.take_due_refine();
                let auto_refresh = state.take_due_auto_refresh();
                let message_expired = state.expire_message();
                if refine {
                    layout = terminal.draw(&frame, output_name, &state)?;
                } else if auto_refresh {
                    match capturer.capture() {
                        Ok(new_frame) => {
                            frame = new_frame;
                            terminal.note_visible_damage();
                            layout = terminal.draw(&frame, output_name, &state)?;
                        }
                        Err(error) => {
                            state.error(format!("Auto-refresh failed: {error:#}"));
                            terminal.draw_echo(&state)?;
                        }
                    }
                } else if message_expired {
                    terminal.draw_echo(&state)?;
                }
                continue;
            }
            event::read().context("cannot read a terminal event")?
        };
        match terminal_event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match state.handle_key(key) {
                    Effect::Quit => break,
                    Effect::Refresh => {
                        state.cancel_auto_refresh();
                        let refreshed = match capturer.capture() {
                            Ok(new_frame) => {
                                frame = new_frame;
                                terminal.note_visible_damage();
                                state.message("Refreshed frame");
                                true
                            }
                            Err(error) => {
                                state.error(format!("Refresh failed: {error:#}"));
                                false
                            }
                        };
                        if refreshed {
                            layout = terminal.draw(&frame, output_name, &state)?;
                        } else {
                            terminal.draw_echo(&state)?;
                        }
                    }
                    Effect::RunAction(index) => {
                        let action = state.actions[index].clone();
                        match action_runner.spawn(&action) {
                            Ok(()) => state.message(format!("Started action: {}", action.name)),
                            Err(error) => state.error(format!("Action failed: {error:#}")),
                        }
                        terminal.draw_echo(&state)?;
                    }
                    Effect::ToggleIdleInhibit => {
                        if idle_inhibitor.take().is_some() {
                            state.idle_inhibited = false;
                            state.message("Idle inhibition disabled");
                        } else {
                            match IdleInhibitor::acquire() {
                                Ok(inhibitor) => {
                                    idle_inhibitor = Some(inhibitor);
                                    state.idle_inhibited = true;
                                    state.message("Idle inhibition enabled");
                                }
                                Err(error) => {
                                    state.error(format!("Idle inhibit failed: {error:#}"));
                                }
                            }
                        }
                        terminal.draw_chrome(output_name, &state, layout)?;
                    }
                    Effect::Graphics(command) => {
                        let (message, redraw) = terminal.apply_graphics_command(command);
                        state.message(message);
                        if redraw {
                            layout = terminal.draw(&frame, output_name, &state)?;
                        } else {
                            terminal.draw_chrome(output_name, &state, layout)?;
                        }
                    }
                    Effect::Redraw => layout = terminal.draw(&frame, output_name, &state)?,
                    Effect::Chrome => terminal.draw_chrome(output_name, &state, layout)?,
                    Effect::SendKey(encoded) => {
                        if let Some(keyboard) = &keyboard {
                            match keyboard.key(encoded.keycode, encoded.modifiers) {
                                Ok(()) if damage_watcher.is_none() => state.schedule_auto_refresh(),
                                Ok(()) => {}
                                Err(error) => {
                                    state.error(format!("Key injection failed: {error:#}"));
                                    terminal.draw_echo(&state)?;
                                }
                            }
                        }
                    }
                    Effect::SendUnicode(character) => {
                        if let Some(keyboard) = &mut keyboard {
                            match keyboard.unicode(character) {
                                Ok(()) if damage_watcher.is_none() => state.schedule_auto_refresh(),
                                Ok(()) => {}
                                Err(error) => {
                                    state.error(format!("Unicode injection failed: {error:#}"));
                                    terminal.draw_echo(&state)?;
                                }
                            }
                        }
                    }
                    Effect::None => {}
                }
            }
            Event::Mouse(mouse) => {
                if is_scroll(mouse.kind) {
                    let scrolls = collect_scroll_batch(mouse, &mut pending_events)?;
                    if state.scroll_target == ScrollTarget::Desktop {
                        let movement = state.scroll_movement(&scrolls, layout, Instant::now());
                        let anchor = scrolls
                            .iter()
                            .rev()
                            .find(|event| event.column < layout.cols && event.row < layout.rows)
                            .and_then(|event| {
                                map_pointer_position(*event, layout, &output_geometry)
                            });
                        if let (Some((axis, steps)), Some(point), Some(pointer)) =
                            (movement, anchor, &pointer)
                        {
                            let steps = steps
                                .clamp(-MAX_SCROLL_STEPS_PER_FRAME, MAX_SCROLL_STEPS_PER_FRAME);
                            if let Err(error) = pointer.scroll(
                                point.local_x,
                                point.local_y,
                                output_geometry.width,
                                output_geometry.height,
                                axis == ScrollAxis::Horizontal,
                                steps,
                            ) {
                                state.error(format!("Scroll injection failed: {error:#}"));
                                terminal.draw_echo(&state)?;
                            }
                        }
                    } else if state.handle_scroll_batch(&scrolls, layout, Instant::now())
                        == Effect::Redraw
                    {
                        layout = terminal.draw(&frame, output_name, &state)?;
                    }
                    continue;
                }
                if let Some((point, button)) = map_click(mouse, layout, &output_geometry) {
                    let mut frame_changed = false;
                    if state.control && state.mouse_armed {
                        if let Some(pointer) = &pointer {
                            match pointer.click(
                                point.local_x,
                                point.local_y,
                                output_geometry.width,
                                output_geometry.height,
                                button,
                            ) {
                                Ok(()) => {
                                    state.message(format!(
                                        "{} clicked {}:{} (global {},{})",
                                        match button {
                                            PointerButton::Left => "Left",
                                            PointerButton::Right => "Right",
                                        },
                                        point.local_x,
                                        point.local_y,
                                        point.global_x,
                                        point.global_y
                                    ));
                                    if damage_watcher.is_none() {
                                        std::thread::sleep(Duration::from_millis(75));
                                        if let Ok(new_frame) = capturer.capture() {
                                            frame = new_frame;
                                            frame_changed = true;
                                        }
                                    }
                                }
                                Err(error) => {
                                    state.error(format!("Click failed: {error:#}"));
                                }
                            }
                        }
                    } else if button == PointerButton::Left
                        && state.zoom_toward(point, layout) == Effect::Redraw
                    {
                        state.message(format!(
                            "Focused toward {}:{}; {:.2}x",
                            point.local_x, point.local_y, state.viewport.zoom
                        ));
                        layout = terminal.draw(&frame, output_name, &state)?;
                        continue;
                    }
                    if frame_changed {
                        terminal.note_visible_damage();
                        layout = terminal.draw(&frame, output_name, &state)?;
                    } else {
                        terminal.draw_echo(&state)?;
                    }
                }
            }
            Event::Resize(_, _) => layout = terminal.draw(&frame, output_name, &state)?,
            Event::FocusGained => {
                terminal.invalidate_image();
                layout = terminal.draw(&frame, output_name, &state)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn earliest_timeout(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
        (None, None) => None,
    }
}

#[derive(Debug)]
struct ViewerState {
    viewport: Viewport,
    message: Option<EchoMessage>,
    control: bool,
    idle_inhibited: bool,
    graphics_backend: &'static str,
    mouse_armed: bool,
    scroll_target: ScrollTarget,
    mode: InteractionMode,
    prefix_pending: bool,
    palette: Option<PaletteState>,
    display_settings: Option<DisplaySettingsState>,
    actions: Vec<Action>,
    auto_refresh_at: Option<Instant>,
    scroll_gesture: ScrollGesture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionMode {
    Nav,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    View,
    Desktop,
}

#[derive(Debug)]
struct EchoMessage {
    text: String,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct PaletteState {
    query: String,
    selected: usize,
}

#[derive(Debug, Default)]
struct DisplaySettingsState {
    selected: DisplaySetting,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum DisplaySetting {
    #[default]
    Quality,
    Resolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    None,
    Redraw,
    Chrome,
    SendKey(EncodedKey),
    SendUnicode(char),
    RunAction(usize),
    Graphics(GraphicsCommand),
    ToggleIdleInhibit,
    Refresh,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphicsCommand {
    LowerResolution,
    RaiseResolution,
    PreviousQualityMode,
    NextQualityMode,
}

impl ViewerState {
    fn new(viewport: Viewport, control: bool) -> Result<Self> {
        render::validate_viewport(viewport)?;
        let mut state = Self {
            viewport,
            message: None,
            control,
            idle_inhibited: false,
            graphics_backend: "ANSI",
            mouse_armed: false,
            scroll_target: ScrollTarget::View,
            mode: InteractionMode::Nav,
            prefix_pending: false,
            palette: None,
            display_settings: None,
            actions: Vec::new(),
            auto_refresh_at: None,
            scroll_gesture: ScrollGesture::default(),
        };
        state.clamp_center();
        Ok(state)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Effect {
        if self.display_settings.is_some() {
            return self.handle_display_settings_key(key);
        }
        if self.palette.is_some() {
            return self.handle_palette_key(key);
        }
        if self.mode == InteractionMode::Input {
            return self.handle_input_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Effect::Quit;
        }

        let effect = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Effect::Quit,
            KeyCode::Char('r') => Effect::Refresh,
            KeyCode::Char('t') if self.control => {
                self.mode = InteractionMode::Input;
                self.prefix_pending = false;
                self.message("Keyboard input enabled; use C-\\ t to return");
                Effect::Chrome
            }
            KeyCode::Char('i') if self.control => {
                self.mouse_armed = !self.mouse_armed;
                self.message(if self.mouse_armed {
                    "Mouse control armed"
                } else {
                    "Control disarmed"
                });
                Effect::Chrome
            }
            KeyCode::Char('s') if self.control => self.toggle_scroll_target(),
            KeyCode::Char('a') if self.control => Effect::ToggleIdleInhibit,
            KeyCode::Char('x') => self.open_palette(),
            KeyCode::Char('g') => self.open_display_settings(),
            KeyCode::Char('?') => self.show_help(),
            _ => self.handle_view_command(key).unwrap_or(Effect::None),
        };
        if effect == Effect::Redraw {
            self.message = None;
        }
        effect
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Effect {
        if self.prefix_pending {
            self.prefix_pending = false;
            if is_command_prefix(key) {
                return Effect::SendKey(EncodedKey {
                    keycode: 43,
                    modifiers: MOD_CONTROL,
                });
            }
            let command = match key.code {
                KeyCode::Char('t') => {
                    self.mode = InteractionMode::Nav;
                    self.message("Keyboard input disabled");
                    Effect::Chrome
                }
                KeyCode::Char('q') => Effect::Quit,
                KeyCode::Char('r') => Effect::Refresh,
                KeyCode::Char('i') => {
                    self.mouse_armed = !self.mouse_armed;
                    self.message(if self.mouse_armed {
                        "Mouse control armed"
                    } else {
                        "Mouse control disarmed"
                    });
                    Effect::Chrome
                }
                KeyCode::Char('s') => self.toggle_scroll_target(),
                KeyCode::Char('a') => Effect::ToggleIdleInhibit,
                KeyCode::Char('x') => self.open_palette(),
                KeyCode::Char('g') => self.open_display_settings(),
                KeyCode::Char('?') => self.show_help(),
                _ => Effect::None,
            };
            if !matches!(
                key.code,
                KeyCode::Char('t' | 'q' | 'r' | 'i' | 's' | 'a' | 'x' | 'g' | '?')
            ) {
                if let Some(effect) = self.handle_view_command(key) {
                    return effect;
                }
                self.error(format!("Undefined prefix key: C-\\ {:?}", key.code));
                return Effect::Chrome;
            }
            return command;
        }
        if is_command_prefix(key) {
            self.prefix_pending = true;
            self.message("C-\\-");
            return Effect::Chrome;
        }
        if let KeyCode::Char(character) = key.code
            && !character.is_ascii()
        {
            return Effect::SendUnicode(character);
        }
        match encode_key(key) {
            Some(encoded) => Effect::SendKey(encoded),
            None => {
                self.error(format!("Unsupported key: {:?}", key.code));
                Effect::Chrome
            }
        }
    }

    fn handle_view_command(&mut self, key: KeyEvent) -> Option<Effect> {
        // Traditional terminal encoding aliases C-\\ and C-4 to the same control byte. Some
        // terminals therefore report the command prefix as Char('4') + CONTROL. View commands
        // are deliberately unmodified single keys, so never interpret that event as 4x zoom.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        let effect = match key.code {
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
            _ => return None,
        };
        if effect == Effect::Redraw {
            self.message = None;
        }
        Some(effect)
    }

    fn open_palette(&mut self) -> Effect {
        if self.actions.is_empty() {
            self.error("No actions configured; see examples/config.toml");
            return Effect::Chrome;
        }
        self.message = None;
        self.palette = Some(PaletteState::default());
        Effect::Chrome
    }

    fn open_display_settings(&mut self) -> Effect {
        self.message = None;
        self.display_settings = Some(DisplaySettingsState::default());
        Effect::Chrome
    }

    fn handle_display_settings_key(&mut self, key: KeyEvent) -> Effect {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('g' | 'q')
        ) {
            self.display_settings = None;
            self.message = None;
            return Effect::Chrome;
        }

        let settings = self.display_settings.as_mut().unwrap();
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                settings.selected = match settings.selected {
                    DisplaySetting::Quality => DisplaySetting::Resolution,
                    DisplaySetting::Resolution => DisplaySetting::Quality,
                };
                Effect::Chrome
            }
            KeyCode::Left | KeyCode::Char('h' | '-') => match settings.selected {
                DisplaySetting::Quality => Effect::Graphics(GraphicsCommand::PreviousQualityMode),
                DisplaySetting::Resolution => Effect::Graphics(GraphicsCommand::LowerResolution),
            },
            KeyCode::Right | KeyCode::Char('l' | '+' | '=') => match settings.selected {
                DisplaySetting::Quality => Effect::Graphics(GraphicsCommand::NextQualityMode),
                DisplaySetting::Resolution => Effect::Graphics(GraphicsCommand::RaiseResolution),
            },
            _ => Effect::None,
        }
    }

    fn show_help(&mut self) -> Effect {
        let help = match self.mode {
            InteractionMode::Input => {
                "Keys control the desktop · C-\\ ? help · C-\\ g display · C-\\ t navigation · C-\\ q quit"
            }
            InteractionMode::Nav if self.control => {
                "Click/arrows move · +/- zoom · g display · t keyboard · i mouse · x actions · q quit"
            }
            InteractionMode::Nav => {
                "Click/arrows move · +/- zoom · 0 overview · g display · x actions · r refresh · q quit"
            }
        };
        self.message_for(help, ERROR_DURATION);
        Effect::Chrome
    }

    fn handle_palette_key(&mut self, key: KeyEvent) -> Effect {
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g'))
        {
            self.palette = None;
            self.message("Quit");
            return Effect::Chrome;
        }

        let matches = self.palette_matches();
        match key.code {
            KeyCode::Enter => {
                let Some(index) = matches
                    .get(self.palette.as_ref().unwrap().selected)
                    .copied()
                else {
                    return Effect::Chrome;
                };
                self.palette = None;
                Effect::RunAction(index)
            }
            KeyCode::Backspace => {
                let palette = self.palette.as_mut().unwrap();
                palette.query.pop();
                palette.selected = 0;
                Effect::Chrome
            }
            KeyCode::Up | KeyCode::BackTab => {
                if !matches.is_empty() {
                    let palette = self.palette.as_mut().unwrap();
                    palette.selected = if palette.selected == 0 {
                        matches.len() - 1
                    } else {
                        palette.selected - 1
                    };
                }
                Effect::Chrome
            }
            KeyCode::Down | KeyCode::Tab => {
                if !matches.is_empty() {
                    let palette = self.palette.as_mut().unwrap();
                    palette.selected = (palette.selected + 1) % matches.len();
                }
                Effect::Chrome
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                let palette = self.palette.as_mut().unwrap();
                palette.query.push(character);
                palette.selected = 0;
                Effect::Chrome
            }
            _ => Effect::None,
        }
    }

    fn palette_matches(&self) -> Vec<usize> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.to_lowercase())
            .unwrap_or_default();
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, action)| {
                query.is_empty()
                    || action.name.to_lowercase().contains(&query)
                    || action.description.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn palette_echo(&self) -> Option<String> {
        let palette = self.palette.as_ref()?;
        let matches = self.palette_matches();
        let selection = matches.get(palette.selected).map_or_else(
            || "[no match]".to_owned(),
            |index| {
                let action = &self.actions[*index];
                if action.description.is_empty() {
                    action.name.clone()
                } else {
                    format!("{} — {}", action.name, action.description)
                }
            },
        );
        Some(format!(
            "M-x {}  [{}] ({}/{})",
            palette.query,
            selection,
            if matches.is_empty() {
                0
            } else {
                palette.selected + 1
            },
            matches.len()
        ))
    }

    fn message(&mut self, text: impl Into<String>) {
        self.message_for(text, MESSAGE_DURATION);
    }

    fn toggle_scroll_target(&mut self) -> Effect {
        self.scroll_target = match self.scroll_target {
            ScrollTarget::View => ScrollTarget::Desktop,
            ScrollTarget::Desktop => ScrollTarget::View,
        };
        self.scroll_gesture = ScrollGesture::default();
        self.message(match self.scroll_target {
            ScrollTarget::View => "Scroll controls viewport",
            ScrollTarget::Desktop => "Scroll controls remote desktop",
        });
        Effect::Chrome
    }

    fn error(&mut self, text: impl Into<String>) {
        self.message_for(text, ERROR_DURATION);
    }

    fn message_for(&mut self, text: impl Into<String>, duration: Duration) {
        self.message = Some(EchoMessage {
            text: text.into(),
            expires_at: Instant::now() + duration,
        });
    }

    fn next_wakeup_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        [
            self.message.as_ref().map(|message| message.expires_at),
            self.auto_refresh_at,
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn expire_message(&mut self) -> bool {
        if self
            .message
            .as_ref()
            .is_some_and(|message| message.expires_at <= Instant::now())
        {
            self.message = None;
            true
        } else {
            false
        }
    }

    fn schedule_auto_refresh(&mut self) {
        self.auto_refresh_at = Some(Instant::now() + AUTO_REFRESH_DELAY);
    }

    fn cancel_auto_refresh(&mut self) {
        self.auto_refresh_at = None;
    }

    fn take_due_auto_refresh(&mut self) -> bool {
        if self
            .auto_refresh_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.auto_refresh_at = None;
            true
        } else {
            false
        }
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

    fn zoom_toward(&mut self, target: LogicalPoint, layout: DrawLayout) -> Effect {
        let previous = self.viewport;
        let target_x = (target.source_x as f32 + 0.5) / layout.source_width as f32;
        let target_y = (target.source_y as f32 + 0.5) / layout.source_height as f32;
        self.viewport.zoom = (self.viewport.zoom * ZOOM_STEP).min(MAX_ZOOM);
        self.viewport.center_x += (target_x - self.viewport.center_x) * CLICK_FOCUS_FRACTION;
        self.viewport.center_y += (target_y - self.viewport.center_y) * CLICK_FOCUS_FRACTION;
        self.clamp_center();
        if self.viewport.zoom == previous.zoom
            && self.viewport.center_x == previous.center_x
            && self.viewport.center_y == previous.center_y
        {
            Effect::None
        } else {
            Effect::Redraw
        }
    }

    fn handle_scroll_batch(
        &mut self,
        events: &[MouseEvent],
        layout: DrawLayout,
        now: Instant,
    ) -> Effect {
        let movement = self.scroll_movement(events, layout, now);
        let Some((axis, steps)) = movement else {
            return Effect::None;
        };
        let steps = steps.clamp(-MAX_SCROLL_STEPS_PER_FRAME, MAX_SCROLL_STEPS_PER_FRAME) as f32
            * SCROLL_STEP_SCALE;
        let effect = match axis {
            ScrollAxis::Horizontal => self.pan(steps, 0.0),
            ScrollAxis::Vertical => self.pan(0.0, steps),
        };
        if effect == Effect::Redraw {
            self.message = None;
        }
        effect
    }

    fn scroll_movement(
        &mut self,
        events: &[MouseEvent],
        layout: DrawLayout,
        now: Instant,
    ) -> Option<(ScrollAxis, i32)> {
        self.scroll_gesture.consume(
            events
                .iter()
                .copied()
                .filter(|mouse| mouse.column < layout.cols && mouse.row < layout.rows),
            now,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Default)]
struct ScrollGesture {
    axis: Option<ScrollAxis>,
    last_event_at: Option<Instant>,
    pending_x: i32,
    pending_y: i32,
    cross_axis_evidence: i32,
}

impl ScrollGesture {
    fn consume(
        &mut self,
        events: impl IntoIterator<Item = MouseEvent>,
        now: Instant,
    ) -> Option<(ScrollAxis, i32)> {
        if self
            .last_event_at
            .is_some_and(|last| now.saturating_duration_since(last) > SCROLL_GESTURE_TIMEOUT)
        {
            *self = Self::default();
        }

        let mut saw_scroll = false;
        for mouse in events {
            let (x, y) = match mouse.kind {
                MouseEventKind::ScrollUp => (0, -1),
                MouseEventKind::ScrollDown => (0, 1),
                MouseEventKind::ScrollLeft => (-1, 0),
                MouseEventKind::ScrollRight => (1, 0),
                _ => continue,
            };
            saw_scroll = true;
            self.pending_x += x;
            self.pending_y += y;
        }
        if !saw_scroll {
            return None;
        }
        self.last_event_at = Some(now);

        let axis = match self.axis {
            Some(axis) => {
                let (primary, cross) = match axis {
                    ScrollAxis::Horizontal => (self.pending_x, self.pending_y),
                    ScrollAxis::Vertical => (self.pending_y, self.pending_x),
                };
                let cross_is_dominant =
                    cross != 0 && (primary == 0 || cross.abs() > primary.abs().saturating_mul(2));
                if cross_is_dominant {
                    if self.cross_axis_evidence.signum() != cross.signum() {
                        self.cross_axis_evidence = 0;
                    }
                    self.cross_axis_evidence += cross;
                } else if primary != 0 {
                    self.cross_axis_evidence = 0;
                }

                if self.cross_axis_evidence.abs() >= AXIS_SWITCH_THRESHOLD {
                    let new_axis = match axis {
                        ScrollAxis::Horizontal => ScrollAxis::Vertical,
                        ScrollAxis::Vertical => ScrollAxis::Horizontal,
                    };
                    self.axis = Some(new_axis);
                    match new_axis {
                        ScrollAxis::Horizontal => self.pending_x = self.cross_axis_evidence,
                        ScrollAxis::Vertical => self.pending_y = self.cross_axis_evidence,
                    }
                    self.cross_axis_evidence = 0;
                    new_axis
                } else {
                    axis
                }
            }
            None => {
                let x = self.pending_x.abs();
                let y = self.pending_y.abs();
                let axis = if x > y {
                    ScrollAxis::Horizontal
                } else if y > x {
                    ScrollAxis::Vertical
                } else {
                    return None;
                };
                self.axis = Some(axis);
                self.cross_axis_evidence = 0;
                axis
            }
        };
        let steps = match axis {
            ScrollAxis::Horizontal => mem::take(&mut self.pending_x),
            ScrollAxis::Vertical => mem::take(&mut self.pending_y),
        };
        self.pending_x = 0;
        self.pending_y = 0;
        (steps != 0).then_some((axis, steps))
    }
}

fn is_scroll(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

fn collect_scroll_batch(
    first: MouseEvent,
    pending_events: &mut VecDeque<Event>,
) -> Result<Vec<MouseEvent>> {
    let deadline = Instant::now() + SCROLL_BATCH_WINDOW;
    let mut scrolls = vec![first];
    while scrolls.len() < 256 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || !event::poll(remaining).context("cannot poll batched scroll events")?
        {
            break;
        }
        match event::read().context("cannot read a batched terminal event")? {
            Event::Mouse(mouse) if is_scroll(mouse.kind) => scrolls.push(mouse),
            other => pending_events.push_back(other),
        }
    }
    Ok(scrolls)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Control,
    Graphics,
}

struct OutputSegment {
    kind: OutputKind,
    bytes: Vec<u8>,
    offset: usize,
}

struct OutputPump {
    original_flags: OFlags,
    active: Option<OutputSegment>,
    control: VecDeque<Vec<u8>>,
    graphics: VecDeque<Vec<u8>>,
    graphics_limiter: Option<RateLimiter>,
}

impl OutputPump {
    fn new(stdout: &Stdout, graphics_bytes_per_second: Option<f64>) -> Result<Self> {
        let original_flags = fcntl_getfl(stdout).context("cannot read stdout flags")?;
        fcntl_setfl(stdout, original_flags | OFlags::NONBLOCK)
            .context("cannot enable non-blocking stdout")?;
        Ok(Self {
            original_flags,
            active: None,
            control: VecDeque::new(),
            graphics: VecDeque::new(),
            graphics_limiter: graphics_bytes_per_second.map(|bytes_per_second| {
                RateLimiter::new(bytes_per_second, TMUX_GRAPHICS_BURST_BYTES, Instant::now())
            }),
        })
    }

    fn enqueue_control(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.control.push_back(bytes);
        }
    }

    fn enqueue_graphics(&mut self, segments: Vec<Vec<u8>>) {
        self.graphics
            .extend(segments.into_iter().filter(|segment| !segment.is_empty()));
    }

    fn replace_graphics(&mut self, segments: Vec<Vec<u8>>) {
        // Individual Kitty APC chunks are schedulable so controls can run between them, but an
        // image transmission whose first m=1 chunk has reached the terminal must not lose its
        // remaining chunks. Conservatively retain the current graphics queue while it is busy;
        // once idle, pending graphics are known to be complete and can be superseded safely.
        if !self.graphics_busy() {
            self.graphics.clear();
        }
        self.enqueue_graphics(segments);
    }

    fn has_pending(&self) -> bool {
        self.active.is_some() || !self.control.is_empty() || !self.graphics.is_empty()
    }

    fn graphics_busy(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|segment| segment.kind == OutputKind::Graphics)
            || !self.graphics.is_empty()
    }

    fn activate_next(&mut self) {
        if self.active.is_none() {
            self.active = self
                .control
                .pop_front()
                .map(|bytes| OutputSegment {
                    kind: OutputKind::Control,
                    bytes,
                    offset: 0,
                })
                .or_else(|| {
                    self.graphics.pop_front().map(|bytes| OutputSegment {
                        kind: OutputKind::Graphics,
                        bytes,
                        offset: 0,
                    })
                });
        }
    }

    fn pump(&mut self, stdout: &Stdout) -> Result<bool> {
        self.activate_next();
        let Some(active) = &mut self.active else {
            return Ok(false);
        };
        let remaining = &active.bytes[active.offset..];
        let allowance = if active.kind == OutputKind::Graphics {
            self.graphics_limiter
                .as_mut()
                .map_or(remaining.len(), |limiter| {
                    limiter.allowance(remaining.len(), Instant::now())
                })
        } else {
            remaining.len()
        };
        if allowance == 0 {
            return Ok(false);
        }
        match rustix::io::write(stdout, &remaining[..allowance]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(written) => {
                active.offset += written;
                if active.kind == OutputKind::Graphics
                    && let Some(limiter) = &mut self.graphics_limiter
                {
                    limiter.consume(written);
                }
            }
            Err(Errno::AGAIN) => return Ok(false),
            Err(Errno::INTR) => return Ok(false),
            Err(error) => {
                return Err(io::Error::from(error)).context("cannot write terminal output");
            }
        }
        if active.offset == active.bytes.len() {
            self.active = None;
        }
        Ok(true)
    }

    fn restore_blocking(&self, stdout: &Stdout) -> io::Result<()> {
        fcntl_setfl(stdout, self.original_flags).map_err(io::Error::from)
    }

    fn take_active_remainder(&mut self) -> Option<Vec<u8>> {
        self.control.clear();
        self.graphics.clear();
        self.active
            .take()
            .map(|active| active.bytes[active.offset..].to_vec())
            .filter(|bytes| !bytes.is_empty())
    }
}

struct RateLimiter {
    bytes_per_second: f64,
    burst: f64,
    tokens: f64,
    updated_at: Instant,
}

impl RateLimiter {
    fn new(bytes_per_second: f64, burst: f64, now: Instant) -> Self {
        Self {
            bytes_per_second,
            burst,
            tokens: burst,
            updated_at: now,
        }
    }

    fn allowance(&mut self, requested: usize, now: Instant) -> usize {
        self.tokens = (self.tokens
            + now.duration_since(self.updated_at).as_secs_f64() * self.bytes_per_second)
            .min(self.burst);
        self.updated_at = now;
        requested.min(self.tokens.floor() as usize)
    }

    fn consume(&mut self, bytes: usize) {
        self.tokens = (self.tokens - bytes as f64).max(0.0);
    }
}

struct Terminal {
    stdout: Stdout,
    kitty: Option<kitty::Renderer>,
    output_pump: Option<OutputPump>,
    fallback_reason: Option<String>,
    last_image: Option<ImageCache>,
    last_kitty_image: Option<KittyImageCache>,
    kitty_atlas: Option<KittyAtlasCache>,
    kitty_atlas_ready: bool,
    kitty_layout: Option<DrawLayout>,
    kitty_refine_at: Option<Instant>,
    graphics_config: GraphicsConfig,
    kitty_quality: KittyQuality,
    kitty_frame_budget_bytes: usize,
    kitty_quality_upgrade_at: Option<Instant>,
    kitty_force_max_quality: bool,
    kitty_damage_streak: u32,
    kitty_last_visible_damage_at: Option<Instant>,
    kitty_adaptive_damage: bool,
    kitty_atlas_refresh_at: Option<Instant>,
    kitty_atlas_stale: bool,
    kitty_previewing: bool,
    force_kitty_redraw: bool,
    deferred_kitty_redraw: bool,
    last_mode_line: Option<(u16, String, bool)>,
    last_echo: Option<(u16, String)>,
}

impl Terminal {
    fn enter(graphics: GraphicsMode, graphics_config: GraphicsConfig) -> Result<Self> {
        graphics_config.validate()?;
        let (kitty, fallback_reason) = match kitty::Selection::select(graphics)? {
            kitty::Selection::Ansi { reason } => (None, reason),
            kitty::Selection::Kitty(renderer) => (Some(renderer), None),
        };
        enable_raw_mode().context("cannot enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = stdout.execute(EnterAlternateScreen).and_then(|stdout| {
            stdout.execute(Hide)?;
            stdout.execute(EnableFocusChange)?;
            stdout.execute(EnableMouseCapture)?;
            stdout.execute(crossterm::terminal::DisableLineWrap)?;
            stdout.execute(Clear(ClearType::All))?;
            stdout.execute(MoveTo(0, 0))
        }) {
            let _ = disable_raw_mode();
            return Err(error).context("cannot initialize the interactive terminal");
        }
        let output_pump = if kitty.is_some() {
            let graphics_rate = kitty
                .as_ref()
                .filter(|renderer| renderer.is_tmux())
                .map(|_| graphics_config.advanced.tmux_bandwidth_mbps * 1_000_000.0 / 8.0);
            match OutputPump::new(&stdout, graphics_rate) {
                Ok(output) => Some(output),
                Err(error) => {
                    let _ = stdout.execute(Show);
                    let _ = stdout.execute(LeaveAlternateScreen);
                    let _ = disable_raw_mode();
                    return Err(error).context("cannot make Kitty output non-blocking");
                }
            }
        } else {
            None
        };
        let resolution = graphics_config.resolution;
        let kitty_quality = KittyQuality::new(
            resolution.width(),
            resolution.height(),
            graphics_config.advanced.adaptive_min_height,
        );
        let kitty_frame_budget_bytes = (graphics_config.advanced.tmux_bandwidth_mbps * 1_000_000.0
            / 8.0
            * Duration::from_millis(graphics_config.advanced.frame_budget_ms).as_secs_f64())
            as usize;
        Ok(Self {
            stdout,
            kitty,
            output_pump,
            fallback_reason,
            last_image: None,
            last_kitty_image: None,
            kitty_atlas: None,
            kitty_atlas_ready: false,
            kitty_layout: None,
            kitty_refine_at: None,
            graphics_config,
            kitty_quality,
            kitty_frame_budget_bytes,
            kitty_quality_upgrade_at: None,
            kitty_force_max_quality: false,
            kitty_damage_streak: 0,
            kitty_last_visible_damage_at: None,
            kitty_adaptive_damage: false,
            kitty_atlas_refresh_at: None,
            kitty_atlas_stale: false,
            kitty_previewing: false,
            force_kitty_redraw: false,
            deferred_kitty_redraw: false,
            last_mode_line: None,
            last_echo: None,
        })
    }

    fn backend_name(&self) -> &'static str {
        if self.kitty.is_some() {
            "KITTY"
        } else {
            "ANSI"
        }
    }

    fn take_fallback_reason(&mut self) -> Option<String> {
        self.fallback_reason.take()
    }

    fn apply_graphics_command(&mut self, command: GraphicsCommand) -> (String, bool) {
        if self.kitty.is_none() {
            return (
                "Runtime graphics controls require the Kitty backend".to_owned(),
                false,
            );
        }
        match command {
            GraphicsCommand::LowerResolution => {
                if !self.kitty_quality.lower_ceiling() {
                    return (
                        format!(
                            "Resolution cap is already at its minimum ({})",
                            self.kitty_quality.ceiling_label()
                        ),
                        false,
                    );
                }
                self.prepare_quality_rebuild();
                (
                    format!(
                        "Resolution cap lowered to {}",
                        self.kitty_quality.ceiling_label()
                    ),
                    true,
                )
            }
            GraphicsCommand::RaiseResolution => {
                if !self.kitty_quality.raise_ceiling() {
                    return (
                        format!(
                            "Resolution cap is already at its maximum ({})",
                            self.kitty_quality.ceiling_label()
                        ),
                        false,
                    );
                }
                self.prepare_quality_rebuild();
                (
                    format!(
                        "Resolution cap raised to {}",
                        self.kitty_quality.ceiling_label()
                    ),
                    true,
                )
            }
            GraphicsCommand::PreviousQualityMode | GraphicsCommand::NextQualityMode => {
                let current = self.graphics_config.quality;
                let quality = if command == GraphicsCommand::PreviousQualityMode {
                    current.previous()
                } else {
                    current.next()
                };
                self.graphics_config.quality = quality;
                let redraw = if quality == QualityMode::Sharp {
                    let redraw = !self.kitty_quality.is_maximum();
                    self.kitty_quality.reset();
                    self.kitty_quality_upgrade_at = None;
                    self.kitty_damage_streak = 0;
                    self.kitty_adaptive_damage = false;
                    redraw
                } else {
                    false
                };
                (
                    format!("Quality: {} — {}", quality.label(), quality.description()),
                    redraw,
                )
            }
        }
    }

    fn prepare_quality_rebuild(&mut self) {
        self.kitty_quality_upgrade_at = None;
        self.kitty_atlas_refresh_at = None;
        self.kitty_refine_at = None;
        self.kitty_force_max_quality = true;
        self.kitty_damage_streak = 0;
        self.kitty_adaptive_damage = false;
        self.force_kitty_redraw = true;
    }

    fn invalidate_image(&mut self) {
        self.last_image = None;
        self.force_kitty_redraw = true;
        self.kitty_refine_at = None;
    }

    fn pump_output(&mut self) -> Result<bool> {
        if let Some(output) = &mut self.output_pump {
            let progress = output.pump(&self.stdout)?;
            if !output.graphics_busy() && self.kitty_atlas.is_some() {
                self.kitty_atlas_ready = true;
            }
            return Ok(progress);
        }
        Ok(false)
    }

    fn has_pending_output(&self) -> bool {
        self.output_pump
            .as_ref()
            .is_some_and(OutputPump::has_pending)
    }

    fn take_deferred_redraw(&mut self) -> bool {
        if self.deferred_kitty_redraw
            && self
                .output_pump
                .as_ref()
                .is_none_or(|output| !output.graphics_busy())
        {
            self.deferred_kitty_redraw = false;
            true
        } else {
            false
        }
    }

    fn next_wakeup_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        earliest_timeout(
            earliest_timeout(
                self.kitty_refine_at
                    .map(|deadline| deadline.saturating_duration_since(now)),
                self.kitty_quality_upgrade_at
                    .map(|deadline| deadline.saturating_duration_since(now)),
            ),
            self.kitty_atlas_refresh_at
                .map(|deadline| deadline.saturating_duration_since(now)),
        )
    }

    fn take_due_refine(&mut self) -> bool {
        let now = Instant::now();
        let refine_due = self.kitty_refine_at.is_some_and(|deadline| now >= deadline);
        if refine_due {
            self.kitty_refine_at = None;
        }
        let atlas_due = self
            .kitty_atlas_refresh_at
            .is_some_and(|deadline| now >= deadline);
        if atlas_due {
            self.kitty_atlas_refresh_at = None;
            self.kitty_quality_upgrade_at = None;
            self.kitty_quality.reset();
            self.force_kitty_redraw = true;
        }
        let upgrade_due = self
            .kitty_quality_upgrade_at
            .is_some_and(|deadline| now >= deadline);
        if upgrade_due {
            self.kitty_quality_upgrade_at = None;
            // Once damage has stayed idle for the configured interval, one maximum-quality
            // keyframe is cheaper and converges faster than retransmitting every intermediate
            // tier at multi-second intervals.
            self.kitty_quality.reset();
            self.kitty_force_max_quality = true;
            self.kitty_damage_streak = 0;
            self.kitty_adaptive_damage = false;
        }
        refine_due || upgrade_due || atlas_due
    }

    fn note_frame_damage(&mut self) {
        self.kitty_atlas_stale = true;
        if self.kitty_layout.is_some() {
            self.kitty_atlas_refresh_at = Some(
                Instant::now()
                    + Duration::from_millis(self.graphics_config.advanced.atlas_refresh_ms),
            );
        }
    }

    fn note_visible_damage(&mut self) {
        let now = Instant::now();
        let damage_window =
            Duration::from_millis(self.graphics_config.advanced.adaptive_damage_window_ms);
        self.kitty_damage_streak = next_damage_streak(
            self.kitty_damage_streak,
            self.kitty_last_visible_damage_at,
            now,
            damage_window,
        );
        self.kitty_last_visible_damage_at = Some(now);
        self.kitty_adaptive_damage =
            self.kitty_damage_streak >= self.graphics_config.advanced.adaptive_damage_frames;
        self.note_frame_damage();
        if !self.kitty_quality.is_maximum() {
            self.kitty_quality_upgrade_at =
                Some(now + Duration::from_millis(self.graphics_config.advanced.recovery_ms));
        }
    }

    fn draw(
        &mut self,
        frame: &RgbImage,
        output_name: &str,
        state: &ViewerState,
    ) -> Result<DrawLayout> {
        let (cols, rows) = crossterm::terminal::size()?;
        let image_rows = rows.saturating_sub(2).max(1);
        if self.kitty.is_some() {
            return self.draw_kitty(frame, output_name, state, cols.max(1), image_rows, rows);
        }
        let rendered =
            render::render_half_blocks_viewport(frame, cols.max(1), image_rows, state.viewport)?;
        let layout = DrawLayout {
            cols: rendered.cols,
            rows: rendered.rows,
            sample_height: rendered.sample_height,
            viewport: rendered.viewport,
            source_width: frame.width(),
            source_height: frame.height(),
        };
        let mode_y = rows.saturating_sub(2);
        let incremental = self
            .last_image
            .as_ref()
            .filter(|previous| previous.layout == layout && previous.mode_y == mode_y)
            .map(|previous| {
                render::encode_cell_diff(&previous.cells, &rendered.cells, rendered.cols)
            });
        match incremental {
            Some(bytes) if bytes.is_empty() => {}
            Some(bytes) if bytes.len() < rendered.bytes.len() => {
                self.stdout
                    .sync_update(|stdout| stdout.write_all(&bytes))??;
            }
            Some(_) | None => {
                self.stdout.sync_update(|stdout| -> io::Result<()> {
                    stdout.queue(MoveTo(0, 0))?;
                    stdout.write_all(&rendered.bytes)?;
                    for y in rendered.rows..mode_y {
                        stdout.queue(MoveTo(0, y))?;
                        stdout.queue(Clear(ClearType::CurrentLine))?;
                    }
                    Ok(())
                })??;
            }
        }
        self.last_image = Some(ImageCache {
            layout,
            mode_y,
            cells: rendered.cells,
        });
        self.draw_chrome(output_name, state, layout)?;
        Ok(layout)
    }

    fn draw_kitty(
        &mut self,
        frame: &RgbImage,
        output_name: &str,
        state: &ViewerState,
        max_cols: u16,
        max_rows: u16,
        terminal_rows: u16,
    ) -> Result<DrawLayout> {
        let window = crossterm::terminal::window_size().ok();
        let cell_width = window
            .as_ref()
            .filter(|size| size.width > 0 && size.columns > 0)
            .map_or(10, |size| u32::from(size.width) / u32::from(size.columns));
        let cell_height = window
            .as_ref()
            .filter(|size| size.height > 0 && size.rows > 0)
            .map_or(20, |size| u32::from(size.height) / u32::from(size.rows));
        let cell_width = cell_width.max(1);
        let cell_height = cell_height.max(1);
        let display_pixel_width = u32::from(max_cols) * cell_width;
        let display_pixel_height = u32::from(max_rows) * cell_height;
        render::validate_viewport(state.viewport)?;
        let viewport = render::viewport_rect(frame.width(), frame.height(), state.viewport);
        let (display_width, display_height) = render::fit_dimensions(
            viewport.width,
            viewport.height,
            display_pixel_width,
            display_pixel_height,
        );
        let cols = display_width.div_ceil(cell_width).min(u32::from(max_cols)) as u16;
        let rows = display_height
            .div_ceil(cell_height)
            .min(u32::from(max_rows)) as u16;
        let layout = DrawLayout {
            cols: cols.max(1),
            rows: rows.max(1),
            sample_height: rows.max(1).saturating_mul(2),
            viewport,
            source_width: frame.width(),
            source_height: frame.height(),
        };
        let atlas_compatible = self.kitty_atlas.as_ref().is_some_and(|atlas| {
            atlas.source_width == frame.width()
                && atlas.source_height == frame.height()
                && atlas.max_cols == max_cols
                && atlas.max_rows == max_rows
                && atlas.cell_width == cell_width
                && atlas.cell_height == cell_height
        });
        let rebuild_atlas = self.force_kitty_redraw
            || !atlas_compatible
            || !self.kitty.as_ref().is_some_and(kitty::Renderer::has_atlas);
        if rebuild_atlas {
            let (ceiling_width, ceiling_height) = self.kitty_quality.ceiling_limits();
            let mut atlas = render::render_raster_viewport(
                frame,
                display_pixel_width.min(ceiling_width),
                display_pixel_height.min(ceiling_height),
                Viewport::default(),
            )?;
            atlas.image = render::align_raster_to_cell_grid(
                &atlas.image,
                layout.cols,
                layout.rows,
                cell_width,
                cell_height,
                ceiling_width,
                ceiling_height,
            );
            render::reduce_color_precision(&mut atlas.image, KITTY_COLOR_BITS);
            let crop = render::map_viewport_to_raster(
                layout.viewport,
                frame.width(),
                frame.height(),
                atlas.image.width(),
                atlas.image.height(),
            );
            let segments = self
                .kitty
                .as_mut()
                .expect("Kitty renderer was selected")
                .encode_atlas(&atlas.image, crop, layout.cols, layout.rows)?;
            self.output_pump
                .as_mut()
                .expect("Kitty output pump was initialized")
                .replace_graphics(segments);
            let full_viewport = layout.viewport
                == (ViewportRect {
                    x: 0,
                    y: 0,
                    width: frame.width(),
                    height: frame.height(),
                });
            self.last_kitty_image = full_viewport.then(|| KittyImageCache {
                layout,
                image: atlas.image.clone(),
            });
            self.kitty_atlas = Some(KittyAtlasCache {
                source_width: frame.width(),
                source_height: frame.height(),
                image_width: atlas.image.width(),
                image_height: atlas.image.height(),
                max_cols,
                max_rows,
                cell_width,
                cell_height,
            });
            self.kitty_atlas_ready = false;
            self.kitty_atlas_stale = false;
            self.kitty_atlas_refresh_at = None;
            self.kitty_layout = Some(layout);
            self.kitty_refine_at = (!full_viewport).then(|| {
                Instant::now() + Duration::from_millis(self.graphics_config.advanced.preview_ms)
            });
            self.kitty_previewing = !full_viewport;
            self.kitty_force_max_quality = false;
            self.force_kitty_redraw = false;
            self.deferred_kitty_redraw = false;
            self.last_image = None;
            self.draw_chrome(output_name, state, layout)?;
            return Ok(layout);
        }

        let viewport_changed = self.kitty_layout != Some(layout);
        // A fresh-atlas preview records the new layout before its delayed refine, so the
        // preview flag is also needed to distinguish that one-shot navigation frame from live
        // desktop damage on an unchanged viewport.
        let navigation_refine = viewport_changed || self.kitty_previewing;
        if viewport_changed {
            match atlas_navigation(self.kitty_atlas_ready, self.kitty_atlas_stale) {
                AtlasNavigation::Preview => {
                    let atlas = self
                        .kitty_atlas
                        .as_ref()
                        .expect("a ready Kitty atlas has metadata");
                    let crop = render::map_viewport_to_raster(
                        layout.viewport,
                        atlas.source_width,
                        atlas.source_height,
                        atlas.image_width,
                        atlas.image_height,
                    );
                    let segments = self
                        .kitty
                        .as_mut()
                        .expect("Kitty renderer was selected")
                        .encode_atlas_placement(crop, layout.cols, layout.rows)?;
                    self.output_pump
                        .as_mut()
                        .expect("Kitty output pump was initialized")
                        .replace_graphics(segments);
                    self.last_kitty_image = None;
                    self.kitty_layout = Some(layout);
                    self.kitty_refine_at = Some(
                        Instant::now()
                            + Duration::from_millis(self.graphics_config.advanced.preview_ms),
                    );
                    self.kitty_previewing = true;
                    self.deferred_kitty_redraw = false;
                    self.draw_chrome(output_name, state, layout)?;
                    return Ok(layout);
                }
                AtlasNavigation::WaitForUpload => {
                    // An atlas placement is only valid once its complete upload reached Kitty.
                    self.deferred_kitty_redraw = true;
                    self.draw_chrome(output_name, state, layout)?;
                    return Ok(layout);
                }
                AtlasNavigation::RefineCurrentFrame => {
                    // A cached crop is fast only while it is also truthful. Once desktop damage
                    // makes the atlas stale, keep the current viewport on screen and start
                    // replacing it directly with tiles rendered from the latest captured frame.
                    self.kitty_refine_at = None;
                    self.kitty_previewing = false;
                }
            }
        }

        if self
            .kitty_refine_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            self.draw_chrome(output_name, state, layout)?;
            return Ok(layout);
        }
        if self
            .output_pump
            .as_ref()
            .is_some_and(OutputPump::graphics_busy)
        {
            self.deferred_kitty_redraw = true;
            self.draw_chrome(output_name, state, layout)?;
            return Ok(layout);
        }

        let quality_mode = self.graphics_config.quality;
        let adaptive_quality = should_adapt_quality(
            quality_mode.adaptive_quality(),
            quality_mode.adaptive_navigation(),
            navigation_refine,
            self.kitty_adaptive_damage,
            self.kitty_force_max_quality,
        );
        if !adaptive_quality && !self.kitty_quality.is_maximum() {
            self.kitty_quality.reset();
            self.kitty_quality_upgrade_at = None;
        }

        let atlas_crop = self.kitty_atlas.as_ref().map(|atlas| {
            render::map_viewport_to_raster(
                layout.viewport,
                atlas.source_width,
                atlas.source_height,
                atlas.image_width,
                atlas.image_height,
            )
        });
        let previous_layout = self
            .last_kitty_image
            .as_ref()
            .map(|previous| previous.layout);
        let mut quality_reduced = false;
        let (raster, mut segments, candidate) = loop {
            let (quality_width, quality_height) = self.kitty_quality.limits();
            let mut raster = render::render_raster_viewport(
                frame,
                display_pixel_width.min(quality_width),
                display_pixel_height.min(quality_height),
                state.viewport,
            )?;
            debug_assert_eq!(raster.viewport, layout.viewport);
            raster.image = render::align_raster_to_cell_grid(
                &raster.image,
                layout.cols,
                layout.rows,
                cell_width,
                cell_height,
                quality_width,
                quality_height,
            );
            render::reduce_color_precision(&mut raster.image, KITTY_COLOR_BITS);
            if !refine_improves_atlas(
                self.kitty_atlas_stale,
                atlas_crop,
                raster.image.dimensions(),
            ) {
                break (raster, Vec::new(), None);
            }
            let previous = self.last_kitty_image.as_ref().filter(|previous| {
                previous.layout == layout
                    && previous.image.dimensions() == raster.image.dimensions()
            });
            let reset = self.force_kitty_redraw || previous.is_none();
            let tiles = render::changed_raster_tiles(
                (!reset)
                    .then_some(previous)
                    .flatten()
                    .map(|previous| &previous.image),
                &raster.image,
                layout.cols,
                layout.rows,
                KITTY_TILE_SIZE,
            );
            let mut candidate = self
                .kitty
                .as_ref()
                .expect("Kitty renderer was selected")
                .clone();
            let segments = candidate.encode_tiles(&tiles, reset)?;
            let encoded_bytes = segments.iter().map(Vec::len).sum();
            if adaptive_quality
                && candidate.is_tmux()
                && encoded_bytes > self.kitty_frame_budget_bytes
                && self
                    .kitty_quality
                    .reduce_for(encoded_bytes, self.kitty_frame_budget_bytes)
            {
                quality_reduced = true;
                continue;
            }
            break (raster, segments, Some(candidate));
        };
        let Some(candidate) = candidate else {
            // A fresh terminal-side atlas is already at least as detailed as this bandwidth-
            // limited raster. Do not encode or let a nominal "refine" replace it with blurrier
            // tiles.
            self.kitty_quality.reset();
            self.kitty_quality_upgrade_at = None;
            self.last_image = None;
            self.last_kitty_image = None;
            self.kitty_layout = Some(layout);
            self.kitty_refine_at = None;
            self.kitty_previewing = false;
            self.force_kitty_redraw = false;
            self.deferred_kitty_redraw = false;
            self.kitty_force_max_quality = false;
            self.draw_chrome(output_name, state, layout)?;
            return Ok(layout);
        };
        *self.kitty.as_mut().expect("Kitty renderer was selected") = candidate;
        if quality_reduced {
            self.kitty_quality_upgrade_at = Some(
                Instant::now() + Duration::from_millis(self.graphics_config.advanced.recovery_ms),
            );
        }
        if let Some(previous_layout) = previous_layout
            && previous_layout != layout
        {
            let mut cleanup = Vec::new();
            clear_stale_kitty_cells(&mut cleanup, previous_layout, layout, terminal_rows)?;
            if !cleanup.is_empty() {
                segments.push(cleanup);
            }
        }
        if !segments.is_empty() {
            self.output_pump
                .as_mut()
                .expect("Kitty output pump was initialized")
                .enqueue_graphics(segments);
        }
        self.last_image = None;
        self.last_kitty_image = Some(KittyImageCache {
            layout,
            image: raster.image,
        });
        self.kitty_layout = Some(layout);
        self.kitty_refine_at = None;
        self.kitty_previewing = false;
        if self.kitty_atlas_stale {
            self.kitty_atlas_refresh_at = Some(
                Instant::now()
                    + Duration::from_millis(self.graphics_config.advanced.atlas_refresh_ms),
            );
        }
        self.force_kitty_redraw = false;
        self.deferred_kitty_redraw = false;
        self.kitty_force_max_quality = false;
        self.draw_chrome(output_name, state, layout)?;
        Ok(layout)
    }

    fn draw_chrome(
        &mut self,
        output_name: &str,
        state: &ViewerState,
        layout: DrawLayout,
    ) -> Result<()> {
        self.draw_mode_line(output_name, state, layout)?;
        self.draw_echo(state)
    }

    fn draw_mode_line(
        &mut self,
        output_name: &str,
        state: &ViewerState,
        _layout: DrawLayout,
    ) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size()?;
        let mode_y = rows.saturating_sub(2);
        let mode = match state.mode {
            InteractionMode::Nav => "Navigation",
            InteractionMode::Input => "Keyboard",
        };
        let mouse = if !state.control {
            "View only"
        } else if state.mouse_armed {
            "Mouse on"
        } else {
            "Mouse off"
        };
        let graphics = if self.kitty.is_some() {
            if self.kitty_previewing {
                format!("{} loading", self.kitty_quality.ceiling_label())
            } else {
                format!(
                    "{} {}",
                    self.kitty_quality.label(),
                    self.graphics_config.quality.label()
                )
            }
        } else {
            state.graphics_backend.to_owned()
        };
        let scroll = (state.scroll_target == ScrollTarget::Desktop).then_some(" · Desktop scroll");
        let idle = state.idle_inhibited.then_some(" · Keep awake");
        let help = if state.mode == InteractionMode::Input {
            "C-\\ ? Help"
        } else {
            "? Help"
        };
        let mode_line = fit_status(
            &format!(
                " Termway · {output_name} · {mode} · {:.2}× · {graphics} · {mouse}{}{} · {help} ",
                state.viewport.zoom,
                scroll.unwrap_or(""),
                idle.unwrap_or(""),
            ),
            cols as usize,
        );
        let remote_pointer_active =
            state.mouse_armed || matches!(state.scroll_target, ScrollTarget::Desktop);
        let signature = (mode_y, mode_line.clone(), remote_pointer_active);
        if self.last_mode_line.as_ref() == Some(&signature) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        bytes.queue(MoveTo(0, mode_y))?;
        bytes.queue(ResetColor)?;
        bytes.queue(SetAttribute(Attribute::Reverse))?;
        if remote_pointer_active {
            bytes.queue(SetAttribute(Attribute::Bold))?;
        }
        bytes.queue(Print(mode_line))?;
        bytes.queue(SetAttribute(Attribute::Reset))?;
        bytes.queue(ResetColor)?;
        self.write_control(bytes)?;
        self.last_mode_line = Some(signature);
        Ok(())
    }

    fn draw_echo(&mut self, state: &ViewerState) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size()?;
        if rows <= 1 {
            return Ok(());
        }
        let echo_y = rows - 1;
        let modal_echo = self
            .display_settings_echo(state)
            .or_else(|| state.palette_echo());
        let echo = fit_status(
            modal_echo.as_deref().unwrap_or_else(|| {
                state
                    .message
                    .as_ref()
                    .map(|message| message.text.as_str())
                    .unwrap_or("")
            }),
            cols as usize,
        );
        let signature = (echo_y, echo.clone());
        if self.last_echo.as_ref() == Some(&signature) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        bytes.queue(MoveTo(0, echo_y))?;
        bytes.queue(SetAttribute(Attribute::Reset))?;
        bytes.queue(ResetColor)?;
        bytes.queue(Print(echo))?;
        self.write_control(bytes)?;
        self.last_echo = Some(signature);
        Ok(())
    }

    fn display_settings_echo(&self, state: &ViewerState) -> Option<String> {
        let settings = state.display_settings.as_ref()?;
        if self.kitty.is_none() {
            return Some(
                "Display settings · unavailable with ANSI graphics · Enter close".to_owned(),
            );
        }
        let value = match settings.selected {
            DisplaySetting::Quality => {
                let quality = self.graphics_config.quality;
                format!(
                    "Quality  ‹ {} › — {}",
                    quality.label(),
                    quality.description()
                )
            }
            DisplaySetting::Resolution => format!(
                "Resolution  ‹ {} › — maximum detail",
                self.kitty_quality.ceiling_label()
            ),
        };
        Some(format!(
            "Display settings · {value} · ↑↓ choose · ←→ change · Enter done"
        ))
    }

    fn write_control(&mut self, bytes: Vec<u8>) -> Result<()> {
        if let Some(output) = &mut self.output_pump {
            output.enqueue_control(bytes);
        } else {
            self.stdout
                .sync_update(|stdout| stdout.write_all(&bytes))??;
        }
        Ok(())
    }
}

struct ImageCache {
    layout: DrawLayout,
    mode_y: u16,
    cells: Vec<render::Cell>,
}

const MOD_SHIFT: u32 = 1 << 0;
const MOD_CONTROL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncodedKey {
    keycode: u32,
    modifiers: u32,
}

fn is_command_prefix(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\\' | '4')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn encode_key(key: KeyEvent) -> Option<EncodedKey> {
    let mut modifiers = 0;
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers |= MOD_SHIFT;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers |= MOD_CONTROL;
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers |= MOD_ALT;
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        modifiers |= MOD_SUPER;
    }

    let (keycode, implied_shift) = match key.code {
        KeyCode::Backspace => (14, false),
        KeyCode::Enter => (28, false),
        KeyCode::Left => (105, false),
        KeyCode::Right => (106, false),
        KeyCode::Up => (103, false),
        KeyCode::Down => (108, false),
        KeyCode::Home => (102, false),
        KeyCode::End => (107, false),
        KeyCode::PageUp => (104, false),
        KeyCode::PageDown => (109, false),
        KeyCode::Tab => (15, false),
        KeyCode::BackTab => (15, true),
        KeyCode::Delete => (111, false),
        KeyCode::Insert => (110, false),
        KeyCode::Esc => (1, false),
        KeyCode::F(number @ 1..=10) => (58 + u32::from(number), false),
        KeyCode::F(11) => (87, false),
        KeyCode::F(12) => (88, false),
        KeyCode::Char(character) => char_keycode(character)?,
        _ => return None,
    };
    if implied_shift {
        modifiers |= MOD_SHIFT;
    }
    Some(EncodedKey { keycode, modifiers })
}

fn char_keycode(character: char) -> Option<(u32, bool)> {
    let shifted = character.is_ascii_uppercase();
    let lower = character.to_ascii_lowercase();
    let letter = "qwertyuiop"
        .find(lower)
        .map(|index| 16 + index as u32)
        .or_else(|| "asdfghjkl".find(lower).map(|index| 30 + index as u32))
        .or_else(|| "zxcvbnm".find(lower).map(|index| 44 + index as u32));
    if let Some(keycode) = letter {
        return Some((keycode, shifted));
    }
    Some(match character {
        '1'..='9' => (2 + u32::from(character as u8 - b'1'), false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' | '_' => (12, character == '_'),
        '=' | '+' => (13, character == '+'),
        '[' | '{' => (26, character == '{'),
        ']' | '}' => (27, character == '}'),
        ';' | ':' => (39, character == ':'),
        '\'' | '"' => (40, character == '"'),
        '`' | '~' => (41, character == '~'),
        '\\' | '|' => (43, character == '|'),
        ',' | '<' => (51, character == '<'),
        '.' | '>' => (52, character == '>'),
        '/' | '?' => (53, character == '?'),
        ' ' => (57, false),
        _ => return None,
    })
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut output) = self.output_pump.take() {
            let active_remainder = output.take_active_remainder();
            let _ = output.restore_blocking(&self.stdout);
            if let Some(bytes) = active_remainder {
                let _ = self.stdout.write_all(&bytes);
            }
        }
        if let Some(renderer) = &mut self.kitty {
            for segment in renderer.encode_delete() {
                let _ = self.stdout.write_all(&segment);
            }
        }
        let _ = self.stdout.execute(EndSynchronizedUpdate);
        let _ = self.stdout.execute(SetAttribute(Attribute::Reset));
        let _ = self.stdout.execute(ResetColor);
        let _ = self.stdout.execute(DisableFocusChange);
        let _ = self.stdout.execute(DisableMouseCapture);
        let _ = self.stdout.execute(crossterm::terminal::EnableLineWrap);
        let _ = self.stdout.execute(Show);
        let _ = self.stdout.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn clear_stale_kitty_cells<W: Write>(
    stdout: &mut W,
    previous: DrawLayout,
    current: DrawLayout,
    terminal_rows: u16,
) -> io::Result<()> {
    let mode_y = terminal_rows.saturating_sub(2);
    if previous.cols > current.cols {
        for row in 0..previous.rows.min(current.rows).min(mode_y) {
            stdout.queue(MoveTo(current.cols, row))?;
            stdout.queue(Clear(ClearType::UntilNewLine))?;
        }
    }
    for row in current.rows..previous.rows.min(mode_y) {
        stdout.queue(MoveTo(0, row))?;
        stdout.queue(Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrawLayout {
    cols: u16,
    rows: u16,
    sample_height: u16,
    viewport: ViewportRect,
    source_width: u32,
    source_height: u32,
}

struct KittyImageCache {
    layout: DrawLayout,
    image: RgbImage,
}

struct KittyAtlasCache {
    source_width: u32,
    source_height: u32,
    image_width: u32,
    image_height: u32,
    max_cols: u16,
    max_rows: u16,
    cell_width: u32,
    cell_height: u32,
}

#[derive(Debug)]
struct KittyQuality {
    tier: usize,
    ceiling: usize,
    limits: Vec<(u32, u32)>,
}

impl KittyQuality {
    fn new(max_width: u32, max_height: u32, min_height: u32) -> Self {
        let mut limits = Vec::new();
        for (numerator, denominator) in [(1, 1), (5, 6), (2, 3), (1, 2), (1, 3)] {
            let width = scale_dimension(max_width, numerator, denominator);
            let height = scale_dimension(max_height, numerator, denominator);
            if height >= min_height && limits.last() != Some(&(width, height)) {
                limits.push((width, height));
            }
        }
        if limits
            .last()
            .is_some_and(|(_, height)| *height > min_height)
        {
            let width = ((u64::from(max_width) * u64::from(min_height) / u64::from(max_height))
                as u32)
                .max(1);
            limits.push((width, min_height));
        }
        debug_assert!(!limits.is_empty());
        Self {
            tier: 0,
            ceiling: 0,
            limits,
        }
    }

    fn limits(&self) -> (u32, u32) {
        self.limits[self.tier]
    }

    fn label(&self) -> String {
        let (width, height) = self.limits();
        if u64::from(width) * 9 == u64::from(height) * 16 {
            format!("{height}p")
        } else {
            format!("{width}x{height}")
        }
    }

    fn ceiling_limits(&self) -> (u32, u32) {
        self.limits[self.ceiling]
    }

    fn ceiling_label(&self) -> String {
        let (width, height) = self.ceiling_limits();
        if u64::from(width) * 9 == u64::from(height) * 16 {
            format!("{height}p")
        } else {
            format!("{width}x{height}")
        }
    }

    fn lower_ceiling(&mut self) -> bool {
        if self.ceiling + 1 >= self.limits.len() {
            return false;
        }
        self.ceiling += 1;
        self.tier = self.ceiling;
        true
    }

    fn raise_ceiling(&mut self) -> bool {
        if self.ceiling == 0 {
            return false;
        }
        self.ceiling -= 1;
        self.tier = self.ceiling;
        true
    }

    fn is_maximum(&self) -> bool {
        self.tier == self.ceiling
    }

    fn reset(&mut self) {
        self.tier = self.ceiling;
    }

    fn select_lowest(&mut self) {
        self.tier = self.limits.len() - 1;
    }

    fn reduce_for(&mut self, encoded_bytes: usize, frame_budget_bytes: usize) -> bool {
        let initial = self.tier;
        let (initial_width, initial_height) = self.limits[initial];
        let initial_pixels = u64::from(initial_width) * u64::from(initial_height);
        while self.tier + 1 < self.limits.len() {
            let (candidate_width, candidate_height) = self.limits[self.tier + 1];
            let candidate_pixels = u64::from(candidate_width) * u64::from(candidate_height);
            let projected = encoded_bytes as u64 * candidate_pixels / initial_pixels;
            self.tier += 1;
            if projected <= frame_budget_bytes as u64 {
                break;
            }
        }
        self.tier != initial
    }
}

fn scale_dimension(value: u32, numerator: u32, denominator: u32) -> u32 {
    ((u64::from(value) * u64::from(numerator) / u64::from(denominator)) as u32).max(1)
}

fn should_adapt_quality(
    adaptive_quality: bool,
    adaptive_navigation: bool,
    navigation_refine: bool,
    adaptive_damage: bool,
    force_max_quality: bool,
) -> bool {
    adaptive_quality
        && !force_max_quality
        && if navigation_refine {
            adaptive_navigation
        } else {
            adaptive_damage
        }
}

fn next_damage_streak(
    current: u32,
    previous_at: Option<Instant>,
    now: Instant,
    window: Duration,
) -> u32 {
    if previous_at.is_some_and(|previous| now.duration_since(previous) <= window) {
        current.saturating_add(1)
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtlasNavigation {
    Preview,
    WaitForUpload,
    RefineCurrentFrame,
}

fn atlas_navigation(atlas_ready: bool, atlas_stale: bool) -> AtlasNavigation {
    if atlas_stale {
        AtlasNavigation::RefineCurrentFrame
    } else if atlas_ready {
        AtlasNavigation::Preview
    } else {
        AtlasNavigation::WaitForUpload
    }
}

fn refine_improves_atlas(
    atlas_stale: bool,
    atlas_crop: Option<ViewportRect>,
    raster_dimensions: (u32, u32),
) -> bool {
    if atlas_stale {
        return true;
    }
    let Some(atlas_crop) = atlas_crop else {
        return true;
    };
    u64::from(raster_dimensions.0) * u64::from(raster_dimensions.1)
        > u64::from(atlas_crop.width) * u64::from(atlas_crop.height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogicalPoint {
    source_x: u32,
    source_y: u32,
    local_x: u32,
    local_y: u32,
    global_x: i64,
    global_y: i64,
}

fn map_click(
    mouse: MouseEvent,
    layout: DrawLayout,
    output: &OutputGeometry,
) -> Option<(LogicalPoint, PointerButton)> {
    let button = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => PointerButton::Left,
        MouseEventKind::Down(MouseButton::Right) => PointerButton::Right,
        _ => return None,
    };
    map_pointer_position(mouse, layout, output).map(|point| (point, button))
}

fn map_pointer_position(
    mouse: MouseEvent,
    layout: DrawLayout,
    output: &OutputGeometry,
) -> Option<LogicalPoint> {
    if mouse.column >= layout.cols || mouse.row >= layout.rows || output.transform != "Normal" {
        return None;
    }

    let sample_x = f64::from(mouse.column) + 0.5;
    let sample_y = (f64::from(mouse.row) * 2.0 + 1.0).min(f64::from(layout.sample_height) - 0.5);
    let physical_x = f64::from(layout.viewport.x)
        + sample_x / f64::from(layout.cols) * f64::from(layout.viewport.width);
    let physical_y = f64::from(layout.viewport.y)
        + sample_y / f64::from(layout.sample_height) * f64::from(layout.viewport.height);
    let source_x = physical_x
        .floor()
        .clamp(0.0, f64::from(layout.source_width.saturating_sub(1))) as u32;
    let source_y = physical_y
        .floor()
        .clamp(0.0, f64::from(layout.source_height.saturating_sub(1))) as u32;
    let local_x = (f64::from(source_x) / f64::from(layout.source_width) * f64::from(output.width))
        .floor()
        .clamp(0.0, f64::from(output.width.saturating_sub(1))) as u32;
    let local_y = (f64::from(source_y) / f64::from(layout.source_height) * f64::from(output.height))
        .floor()
        .clamp(0.0, f64::from(output.height.saturating_sub(1))) as u32;
    Some(LogicalPoint {
        source_x,
        source_y,
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

fn damage_affects_viewport(
    damage: &[crate::screencopy::DamageRect],
    viewport: ViewportRect,
) -> bool {
    damage.is_empty()
        || damage.iter().any(|rect| {
            rect.x < viewport.x.saturating_add(viewport.width)
                && viewport.x < rect.x.saturating_add(rect.width)
                && rect.y < viewport.y.saturating_add(viewport.height)
                && viewport.y < rect.y.saturating_add(rect.height)
        })
}

fn visible_region_changed(old: &RgbImage, new: &RgbImage, viewport: ViewportRect) -> bool {
    if old.dimensions() != new.dimensions()
        || viewport.x.saturating_add(viewport.width) > old.width()
        || viewport.y.saturating_add(viewport.height) > old.height()
    {
        return true;
    }
    let row_bytes = viewport.width as usize * 3;
    let image_row_bytes = old.width() as usize * 3;
    let x = viewport.x as usize * 3;
    (viewport.y..viewport.y + viewport.height).any(|y| {
        let start = y as usize * image_row_bytes + x;
        old.as_raw()[start..start + row_bytes] != new.as_raw()[start..start + row_bytes]
    })
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

    #[test]
    fn output_pump_prioritizes_control_between_complete_graphics_segments() {
        let mut output = OutputPump {
            original_flags: OFlags::empty(),
            active: None,
            control: VecDeque::new(),
            graphics: VecDeque::new(),
            graphics_limiter: None,
        };
        output.enqueue_graphics(vec![b"graphics-1".to_vec(), b"graphics-2".to_vec()]);
        output.activate_next();
        assert_eq!(output.active.as_ref().unwrap().bytes, b"graphics-1");
        output.active = None;
        output.enqueue_control(b"control".to_vec());
        output.activate_next();
        assert_eq!(output.active.as_ref().unwrap().kind, OutputKind::Control);
        assert_eq!(output.active.as_ref().unwrap().bytes, b"control");
        output.active = None;
        output.activate_next();
        assert_eq!(output.active.as_ref().unwrap().kind, OutputKind::Graphics);
        assert_eq!(output.active.as_ref().unwrap().bytes, b"graphics-2");
        assert!(output.graphics_busy());
    }

    #[test]
    fn replacing_graphics_does_not_strand_an_in_progress_upload() {
        let mut output = OutputPump {
            original_flags: OFlags::empty(),
            active: None,
            control: VecDeque::new(),
            graphics: VecDeque::new(),
            graphics_limiter: None,
        };
        output.enqueue_graphics(vec![b"old-m=1".to_vec(), b"old-m=0".to_vec()]);
        output.activate_next();
        output.active = None;
        output.replace_graphics(vec![b"new-image".to_vec()]);

        assert_eq!(output.graphics.pop_front().unwrap(), b"old-m=0");
        assert_eq!(output.graphics.pop_front().unwrap(), b"new-image");
    }

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
    fn ignores_damage_outside_the_visible_viewport() {
        let viewport = ViewportRect {
            x: 100,
            y: 100,
            width: 50,
            height: 50,
        };
        assert!(!damage_affects_viewport(
            &[crate::screencopy::DamageRect {
                x: 10,
                y: 10,
                width: 20,
                height: 20,
            }],
            viewport,
        ));
        assert!(damage_affects_viewport(
            &[crate::screencopy::DamageRect {
                x: 125,
                y: 125,
                width: 20,
                height: 20,
            }],
            viewport,
        ));
    }

    #[test]
    fn compares_only_pixels_inside_the_visible_viewport() {
        let old = RgbImage::new(4, 2);
        let mut new = old.clone();
        new.put_pixel(0, 0, image::Rgb([1, 2, 3]));
        let viewport = ViewportRect {
            x: 2,
            y: 0,
            width: 2,
            height: 2,
        };
        assert!(!visible_region_changed(&old, &new, viewport));
        new.put_pixel(3, 1, image::Rgb([4, 5, 6]));
        assert!(visible_region_changed(&old, &new, viewport));
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
        assert!(!view_only.mouse_armed);

        let mut control = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(control.handle_key(key(KeyCode::Char('i'))), Effect::Chrome);
        assert!(control.mouse_armed);
        assert_eq!(control.handle_key(key(KeyCode::Char('i'))), Effect::Chrome);
        assert!(!control.mouse_armed);
    }

    #[test]
    fn idle_inhibit_toggle_requires_control_mode_and_prefix_works() {
        let mut view_only = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(view_only.handle_key(key(KeyCode::Char('a'))), Effect::None);

        let mut control = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(
            control.handle_key(key(KeyCode::Char('a'))),
            Effect::ToggleIdleInhibit
        );

        control.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(control.handle_key(prefix), Effect::Chrome);
        assert_eq!(
            control.handle_key(key(KeyCode::Char('a'))),
            Effect::ToggleIdleInhibit
        );
    }

    #[test]
    fn keyboard_input_mode_uses_a_prefix_to_recover_commands() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('t'))), Effect::Chrome);
        assert_eq!(state.mode, InteractionMode::Input);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            Effect::SendKey(EncodedKey {
                keycode: 16,
                modifiers: 0,
            })
        );

        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert!(state.prefix_pending);
        assert_eq!(state.handle_key(key(KeyCode::Char('t'))), Effect::Chrome);
        assert_eq!(state.mode, InteractionMode::Nav);
        assert!(!state.prefix_pending);
    }

    #[test]
    fn input_prefix_exposes_viewport_navigation_commands() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);

        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Char('5'))), Effect::Redraw);
        assert_eq!(state.viewport.zoom, 5.0);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Right)), Effect::Redraw);
        assert_eq!(state.viewport.center_x, 0.54);
        assert_eq!(state.mode, InteractionMode::Input);
    }

    #[test]
    fn modified_digits_do_not_trigger_nav_zoom() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        let aliased_prefix = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
        assert!(is_command_prefix(aliased_prefix));
        assert_eq!(state.handle_key(aliased_prefix), Effect::None);
        assert_eq!(state.viewport.zoom, 1.0);
    }

    #[test]
    fn input_prefix_can_toggle_independent_mouse_control() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Char('i'))), Effect::Chrome);
        assert!(state.mouse_armed);
        assert_eq!(state.mode, InteractionMode::Input);
    }

    #[test]
    fn scroll_target_is_explicit_and_available_through_the_input_prefix() {
        let mut view_only = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(view_only.handle_key(key(KeyCode::Char('s'))), Effect::None);
        assert_eq!(view_only.scroll_target, ScrollTarget::View);

        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('s'))), Effect::Chrome);
        assert_eq!(state.scroll_target, ScrollTarget::Desktop);
        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Char('s'))), Effect::Chrome);
        assert_eq!(state.scroll_target, ScrollTarget::View);
        assert_eq!(state.mode, InteractionMode::Input);
    }

    #[test]
    fn action_palette_filters_and_executes_configured_commands() {
        let mut state = ViewerState::new(Viewport::default(), false).unwrap();
        state.actions = vec![
            Action {
                name: "terminal".into(),
                description: "Open Kitty".into(),
                command: vec!["kitty".into()],
            },
            Action {
                name: "overview".into(),
                description: "Toggle niri overview".into(),
                command: vec!["niri".into()],
            },
        ];
        assert_eq!(state.handle_key(key(KeyCode::Char('x'))), Effect::Chrome);
        assert!(state.palette.is_some());
        for character in "term".chars() {
            assert_eq!(
                state.handle_key(key(KeyCode::Char(character))),
                Effect::Chrome
            );
        }
        assert!(
            state
                .palette_echo()
                .unwrap()
                .contains("terminal — Open Kitty")
        );
        assert_eq!(state.handle_key(key(KeyCode::Enter)), Effect::RunAction(0));
        assert!(state.palette.is_none());
    }

    #[test]
    fn input_prefix_opens_palette_and_control_g_cancels_it() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.actions.push(Action {
            name: "test".into(),
            description: String::new(),
            command: vec!["true".into()],
        });
        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Char('x'))), Effect::Chrome);
        assert!(state.palette.is_some());
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Effect::Chrome
        );
        assert!(state.palette.is_none());
        assert_eq!(state.mode, InteractionMode::Input);
    }

    #[test]
    fn display_settings_group_quality_and_resolution_behind_one_key() {
        let mut state = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('g'))), Effect::Chrome);
        assert_eq!(
            state.display_settings.as_ref().unwrap().selected,
            DisplaySetting::Quality
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Right)),
            Effect::Graphics(GraphicsCommand::NextQualityMode)
        );
        assert_eq!(state.handle_key(key(KeyCode::Down)), Effect::Chrome);
        assert_eq!(
            state.display_settings.as_ref().unwrap().selected,
            DisplaySetting::Resolution
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Left)),
            Effect::Graphics(GraphicsCommand::LowerResolution)
        );
        assert_eq!(state.handle_key(key(KeyCode::Enter)), Effect::Chrome);
        assert!(state.display_settings.is_none());
    }

    #[test]
    fn help_is_discoverable_in_navigation_and_through_the_input_prefix() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        assert_eq!(state.handle_key(key(KeyCode::Char('?'))), Effect::Chrome);
        assert!(state.message.as_ref().unwrap().text.contains("g display"));

        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(state.handle_key(key(KeyCode::Char('?'))), Effect::Chrome);
        assert!(
            state
                .message
                .as_ref()
                .unwrap()
                .text
                .contains("C-\\ g display")
        );
        assert_eq!(state.mode, InteractionMode::Input);
    }

    #[test]
    fn encodes_ascii_navigation_and_modifiers_as_evdev_keys() {
        assert_eq!(
            encode_key(key(KeyCode::Char('A'))),
            Some(EncodedKey {
                keycode: 30,
                modifiers: MOD_SHIFT,
            })
        );
        assert_eq!(
            encode_key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            Some(EncodedKey {
                keycode: 46,
                modifiers: MOD_CONTROL | MOD_ALT,
            })
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('?'))),
            Some(EncodedKey {
                keycode: 53,
                modifiers: MOD_SHIFT,
            })
        );
        assert_eq!(
            encode_key(key(KeyCode::Left)),
            Some(EncodedKey {
                keycode: 105,
                modifiers: 0,
            })
        );
        assert_eq!(encode_key(key(KeyCode::Char('你'))), None);
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Some(EncodedKey {
                keycode: 57,
                modifiers: MOD_CONTROL,
            })
        );
    }

    #[test]
    fn doubled_command_prefix_is_sent_to_the_desktop() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.mode = InteractionMode::Input;
        let prefix = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(prefix), Effect::Chrome);
        assert_eq!(
            state.handle_key(prefix),
            Effect::SendKey(EncodedKey {
                keycode: 43,
                modifiers: MOD_CONTROL,
            })
        );
    }

    #[test]
    fn non_ascii_characters_use_dynamic_unicode_keys() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.mode = InteractionMode::Input;
        assert_eq!(
            state.handle_key(key(KeyCode::Char('你'))),
            Effect::SendUnicode('你')
        );
    }

    #[test]
    fn auto_refresh_deadline_is_consumed_once() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.auto_refresh_at = Some(Instant::now() - Duration::from_millis(1));
        assert!(state.next_wakeup_timeout().unwrap().is_zero());
        assert!(state.take_due_auto_refresh());
        assert!(!state.take_due_auto_refresh());
        assert!(state.next_wakeup_timeout().is_none());
    }

    #[test]
    fn echo_messages_expire_independently_from_mode_state() {
        let mut state = ViewerState::new(Viewport::default(), true).unwrap();
        state.mouse_armed = true;
        state.message_for("Clicked 1:2", Duration::ZERO);
        state.expire_message();
        assert!(state.message.is_none());
        assert!(state.mouse_armed);
        assert!(state.control);
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
        let (point, button) = map_click(
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
        assert_eq!(button, PointerButton::Left);
        assert_eq!(
            point,
            LogicalPoint {
                source_x: 995,
                source_y: 495,
                local_x: 796,
                local_y: 396,
                global_x: -804,
                global_y: 496,
            }
        );
        let (_, button) = map_click(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 49,
                row: 24,
                modifiers: KeyModifiers::NONE,
            },
            layout,
            &output,
        )
        .unwrap();
        assert_eq!(button, PointerButton::Right);
    }

    #[test]
    fn click_focus_zooms_one_step_and_moves_gradually_toward_target() {
        let layout = DrawLayout {
            cols: 100,
            rows: 50,
            sample_height: 100,
            viewport: ViewportRect {
                x: 0,
                y: 0,
                width: 2000,
                height: 1000,
            },
            source_width: 2000,
            source_height: 1000,
        };
        let target = LogicalPoint {
            source_x: 1200,
            source_y: 600,
            local_x: 960,
            local_y: 480,
            global_x: 960,
            global_y: 480,
        };
        let mut state = ViewerState::new(
            Viewport {
                zoom: 5.0,
                ..Viewport::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(state.zoom_toward(target, layout), Effect::Redraw);
        assert_eq!(state.viewport.zoom, 6.25);
        assert!((state.viewport.center_x - 0.5401).abs() < 0.0001);
        assert!((state.viewport.center_y - 0.5402).abs() < 0.0001);
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
        assert!(map_click(mouse(80, 0), layout, &output).is_none());
        assert!(map_click(mouse(0, 20), layout, &output).is_none());
        output.transform = "90".into();
        assert!(map_click(mouse(0, 0), layout, &output).is_none());
    }

    #[test]
    fn mouse_and_trackpad_scroll_pan_inside_image() {
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
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut state = ViewerState::new(
            Viewport {
                zoom: 5.0,
                ..Viewport::default()
            },
            false,
        )
        .unwrap();
        let started = Instant::now();
        assert_eq!(
            state.handle_scroll_batch(
                &[
                    mouse(MouseEventKind::ScrollLeft, 40, 10),
                    mouse(MouseEventKind::ScrollUp, 40, 10),
                    mouse(MouseEventKind::ScrollUp, 40, 10),
                    mouse(MouseEventKind::ScrollUp, 40, 10),
                ],
                layout,
                started,
            ),
            Effect::Redraw
        );
        assert_eq!(state.viewport.center_y, 0.47);
        assert_eq!(state.viewport.center_x, 0.5);

        // One cross-axis event is treated as noise.
        assert_eq!(
            state.handle_scroll_batch(
                &[mouse(MouseEventKind::ScrollRight, 40, 10)],
                layout,
                started + Duration::from_millis(20),
            ),
            Effect::None
        );
        assert_eq!(state.viewport.center_x, 0.5);

        // Sustained cross-axis input breaks the lock without waiting for a pause.
        assert_eq!(
            state.handle_scroll_batch(
                &[mouse(MouseEventKind::ScrollRight, 40, 10)],
                layout,
                started + Duration::from_millis(30),
            ),
            Effect::Redraw
        );
        assert_eq!(state.viewport.center_x, 0.52);

        // The new horizontal lock can likewise be interrupted vertically.
        assert_eq!(
            state.handle_scroll_batch(
                &[mouse(MouseEventKind::ScrollUp, 40, 10); 2],
                layout,
                started + Duration::from_millis(40),
            ),
            Effect::Redraw
        );
        assert_eq!(state.viewport.center_y, 0.45);
        assert_eq!(state.viewport.zoom, 5.0);
        assert_eq!(
            state.handle_scroll_batch(
                &[mouse(MouseEventKind::ScrollUp, 80, 10)],
                layout,
                started + SCROLL_GESTURE_TIMEOUT + Duration::from_millis(50),
            ),
            Effect::None
        );

        let mut overview = ViewerState::new(Viewport::default(), false).unwrap();
        assert_eq!(
            overview.handle_scroll_batch(
                &[mouse(MouseEventKind::ScrollDown, 40, 10)],
                layout,
                started,
            ),
            Effect::None
        );
    }

    #[test]
    fn status_is_exact_terminal_width() {
        assert_eq!(fit_status("abc", 5), "abc  ");
        assert_eq!(fit_status("abcdef", 3), "abc");
        assert_eq!(fit_status("你好", 1), "你");
    }

    #[test]
    fn graphics_rate_limiter_bounds_initial_burst_and_refills() {
        let started = Instant::now();
        let mut limiter = RateLimiter::new(1_000.0, 100.0, started);
        assert_eq!(limiter.allowance(1_000, started), 100);
        limiter.consume(100);
        assert_eq!(limiter.allowance(1_000, started), 0);
        assert_eq!(
            limiter.allowance(1_000, started + Duration::from_millis(25)),
            25
        );
        limiter.consume(25);
        assert_eq!(
            limiter.allowance(1_000, started + Duration::from_millis(225)),
            100
        );
    }

    #[test]
    fn kitty_quality_jumps_to_a_frame_budget_and_resets_after_idle() {
        let mut quality = KittyQuality::new(1920, 1080, 360);
        assert_eq!(quality.limits(), (1920, 1080));
        assert!(quality.reduce_for(6_000_000, 1_100_000));
        assert_eq!(quality.label(), "360p");
        quality.reset();
        assert_eq!(quality.label(), "1080p");

        let mut moderate = KittyQuality::new(1920, 1080, 360);
        assert!(moderate.reduce_for(1_500_000, 1_100_000));
        assert_eq!(moderate.label(), "900p");
    }

    #[test]
    fn kitty_quality_scales_custom_resolution_and_respects_its_floor() {
        let mut quality = KittyQuality::new(2560, 1600, 800);
        assert_eq!(quality.limits(), (2560, 1600));
        assert_eq!(quality.label(), "2560x1600");
        quality.select_lowest();
        assert_eq!(quality.limits(), (1280, 800));
        assert_eq!(quality.label(), "1280x800");
    }

    #[test]
    fn runtime_resolution_cap_is_the_new_adaptive_ceiling() {
        let mut quality = KittyQuality::new(1920, 1080, 360);
        assert!(quality.lower_ceiling());
        assert_eq!(quality.ceiling_label(), "900p");
        quality.select_lowest();
        quality.reset();
        assert_eq!(quality.label(), "900p");
        assert!(quality.raise_ceiling());
        assert_eq!(quality.ceiling_label(), "1080p");
        assert_eq!(quality.label(), "1080p");
        assert!(!quality.raise_ceiling());
    }

    #[test]
    fn adaptive_quality_skips_navigation_by_default() {
        assert!(should_adapt_quality(true, false, false, true, false));
        assert!(!should_adapt_quality(true, false, true, true, false));
        assert!(should_adapt_quality(true, true, true, false, false));
        assert!(!should_adapt_quality(true, false, false, false, false));
        assert!(!should_adapt_quality(true, true, false, true, true));
        assert!(!should_adapt_quality(false, true, false, true, false));
    }

    #[test]
    fn adaptive_damage_streak_requires_nearby_frames() {
        let started = Instant::now();
        let window = Duration::from_millis(500);
        assert_eq!(next_damage_streak(0, None, started, window), 1);
        assert_eq!(
            next_damage_streak(
                1,
                Some(started),
                started + Duration::from_millis(200),
                window
            ),
            2
        );
        assert_eq!(
            next_damage_streak(
                2,
                Some(started),
                started + Duration::from_millis(501),
                window
            ),
            1
        );
    }

    #[test]
    fn fresh_atlas_is_never_overlaid_with_an_equal_or_lower_resolution_refine() {
        let crop = ViewportRect {
            x: 0,
            y: 0,
            width: 960,
            height: 540,
        };
        assert!(!refine_improves_atlas(false, Some(crop), (640, 360)));
        assert!(!refine_improves_atlas(false, Some(crop), (960, 540)));
        assert!(refine_improves_atlas(false, Some(crop), (1280, 720)));
        assert!(refine_improves_atlas(true, Some(crop), (640, 360)));
        assert!(refine_improves_atlas(false, None, (640, 360)));
    }

    #[test]
    fn navigation_never_previews_a_stale_atlas() {
        assert_eq!(atlas_navigation(true, false), AtlasNavigation::Preview);
        assert_eq!(
            atlas_navigation(false, false),
            AtlasNavigation::WaitForUpload
        );
        assert_eq!(
            atlas_navigation(true, true),
            AtlasNavigation::RefineCurrentFrame
        );
        assert_eq!(
            atlas_navigation(false, true),
            AtlasNavigation::RefineCurrentFrame
        );
    }
}
