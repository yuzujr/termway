use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const SCROLL_DISTANCE_PER_STEP: f64 = 15.0;
const XKB_KEYMAP: &str = concat!(
    "xkb_keymap {\n",
    "  xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n",
    "  xkb_types { include \"complete\" };\n",
    "  xkb_compat { include \"complete\" };\n",
    "  xkb_symbols { include \"pc+us+inet(evdev)\" };\n",
    "  xkb_geometry { include \"pc(pc105)\" };\n",
    "};\n\0",
);

pub struct VirtualPointer {
    connection: Connection,
    _queue: EventQueue<InputState>,
    _manager: ZwlrVirtualPointerManagerV1,
    pointer: ZwlrVirtualPointerV1,
    started: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
}

impl PointerButton {
    fn evdev_code(self) -> u32 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
        }
    }
}

pub struct VirtualKeyboard {
    connection: Connection,
    _queue: EventQueue<InputState>,
    _manager: ZwpVirtualKeyboardManagerV1,
    _seat: wl_seat::WlSeat,
    keyboard: ZwpVirtualKeyboardV1,
    unicode_keyboard: ZwpVirtualKeyboardV1,
    _keymap: File,
    unicode_keymap: Option<File>,
    unicode_chars: Vec<char>,
    started: Instant,
}

#[derive(Default)]
struct InputState {
    outputs: Vec<Output>,
}

struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
}

impl VirtualPointer {
    pub fn connect(runtime_dir: &Path, display: &str, output_name: &str) -> Result<Self> {
        let socket_path = if Path::new(display).is_absolute() {
            Path::new(display).to_path_buf()
        } else {
            runtime_dir.join(display)
        };
        let socket = UnixStream::connect(&socket_path)
            .with_context(|| format!("cannot connect to Wayland at {}", socket_path.display()))?;
        let connection = Connection::from_socket(socket).context("cannot initialize Wayland")?;
        let (globals, mut queue) = registry_queue_init::<InputState>(&connection)
            .context("cannot read Wayland globals")?;
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 2..=2, ())
            .context("compositor does not support wlr virtual pointer v2")?;

        let mut state = InputState::default();
        for global in globals.contents().clone_list() {
            if global.interface == wl_output::WlOutput::interface().name {
                let proxy = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    global.version.min(4),
                    &qh,
                    (),
                );
                state.outputs.push(Output { proxy, name: None });
            }
        }
        queue
            .roundtrip(&mut state)
            .context("cannot read Wayland output names")?;
        let output = state
            .outputs
            .iter()
            .find(|output| output.name.as_deref() == Some(output_name))
            .with_context(|| format!("Wayland has no output named {output_name}"))?;
        let pointer =
            manager.create_virtual_pointer_with_output(None, Some(&output.proxy), &qh, ());
        connection
            .flush()
            .context("cannot create virtual pointer")?;

        Ok(Self {
            connection,
            _queue: queue,
            _manager: manager,
            pointer,
            started: Instant::now(),
        })
    }

    pub fn click(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        button: PointerButton,
    ) -> Result<()> {
        if width == 0 || height == 0 || x >= width || y >= height {
            bail!("refusing out-of-bounds pointer coordinate ({x},{y}) in {width}x{height}");
        }
        let time = self.started.elapsed().as_millis() as u32;
        self.pointer.motion_absolute(time, x, y, width, height);
        self.pointer.frame();
        self.pointer
            .button(time, button.evdev_code(), wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        self.pointer
            .button(time, button.evdev_code(), wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.connection
            .flush()
            .context("cannot send virtual click")?;
        Ok(())
    }

    pub fn scroll(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        horizontal: bool,
        steps: i32,
    ) -> Result<()> {
        if width == 0 || height == 0 || x >= width || y >= height {
            bail!("refusing out-of-bounds pointer coordinate ({x},{y}) in {width}x{height}");
        }
        if steps == 0 {
            return Ok(());
        }
        let time = self.started.elapsed().as_millis() as u32;
        let axis = if horizontal {
            wl_pointer::Axis::HorizontalScroll
        } else {
            wl_pointer::Axis::VerticalScroll
        };
        self.pointer.motion_absolute(time, x, y, width, height);
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.axis_discrete(
            time,
            axis,
            f64::from(steps) * SCROLL_DISTANCE_PER_STEP,
            steps,
        );
        self.pointer.frame();
        self.connection
            .flush()
            .context("cannot send virtual scroll")?;
        Ok(())
    }
}

impl VirtualKeyboard {
    pub fn connect(runtime_dir: &Path, display: &str) -> Result<Self> {
        let connection = connect_wayland(runtime_dir, display)?;
        let (globals, queue) = registry_queue_init::<InputState>(&connection)
            .context("cannot read Wayland globals")?;
        let qh = queue.handle();
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("compositor does not support virtual keyboard v1")?;
        let seat_global = globals
            .contents()
            .clone_list()
            .into_iter()
            .find(|global| global.interface == wl_seat::WlSeat::interface().name)
            .context("Wayland compositor has no seat")?;
        let seat = globals.registry().bind::<wl_seat::WlSeat, _, _>(
            seat_global.name,
            seat_global.version.min(1),
            &qh,
            (),
        );
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let unicode_keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let keymap = send_keymap(&keyboard)?;
        connection
            .flush()
            .context("cannot create virtual keyboard")?;
        Ok(Self {
            connection,
            _queue: queue,
            _manager: manager,
            _seat: seat,
            keyboard,
            unicode_keyboard,
            _keymap: keymap,
            unicode_keymap: None,
            unicode_chars: Vec::new(),
            started: Instant::now(),
        })
    }

    pub fn key(&self, keycode: u32, modifiers: u32) -> Result<()> {
        let time = self.started.elapsed().as_millis() as u32;
        self.keyboard.modifiers(modifiers, 0, 0, 0);
        self.keyboard
            .key(time, keycode, wl_keyboard::KeyState::Pressed.into());
        self.keyboard
            .key(time, keycode, wl_keyboard::KeyState::Released.into());
        self.keyboard.modifiers(0, 0, 0, 0);
        self.connection.flush().context("cannot send virtual key")?;
        Ok(())
    }

    pub fn unicode(&mut self, character: char) -> Result<()> {
        let keycode = match self
            .unicode_chars
            .iter()
            .position(|entry| *entry == character)
        {
            Some(index) => index as u32 + 1,
            None => {
                if self.unicode_chars.len() >= 512 {
                    self.unicode_chars.clear();
                }
                self.unicode_chars.push(character);
                let keymap = unicode_keymap(&self.unicode_chars);
                self.unicode_keymap = Some(send_keymap_text(&self.unicode_keyboard, &keymap)?);
                self.unicode_chars.len() as u32
            }
        };
        let time = self.started.elapsed().as_millis() as u32;
        self.unicode_keyboard.modifiers(0, 0, 0, 0);
        self.unicode_keyboard
            .key(time, keycode, wl_keyboard::KeyState::Pressed.into());
        self.unicode_keyboard
            .key(time, keycode, wl_keyboard::KeyState::Released.into());
        self.connection
            .flush()
            .context("cannot send Unicode virtual key")?;
        Ok(())
    }
}

fn connect_wayland(runtime_dir: &Path, display: &str) -> Result<Connection> {
    let socket_path = if Path::new(display).is_absolute() {
        Path::new(display).to_path_buf()
    } else {
        runtime_dir.join(display)
    };
    let socket = UnixStream::connect(&socket_path)
        .with_context(|| format!("cannot connect to Wayland at {}", socket_path.display()))?;
    Connection::from_socket(socket).context("cannot initialize Wayland")
}

fn send_keymap(keyboard: &ZwpVirtualKeyboardV1) -> Result<File> {
    send_keymap_text(keyboard, XKB_KEYMAP)
}

fn send_keymap_text(keyboard: &ZwpVirtualKeyboardV1, keymap: &str) -> Result<File> {
    let fd = memfd_create(c"termway-keymap", MemfdFlags::CLOEXEC)
        .context("cannot create keymap memory file")?;
    let mut file = File::from(fd);
    file.write_all(keymap.as_bytes())
        .context("cannot write virtual keyboard keymap")?;
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1.into(),
        file.as_fd(),
        keymap.len() as u32,
    );
    Ok(file)
}

fn unicode_keymap(characters: &[char]) -> String {
    use std::fmt::Write as _;

    let mut keymap = String::from(
        "xkb_keymap {\n\
         xkb_keycodes \"termway\" {\n\
         minimum = 8;\n",
    );
    writeln!(keymap, "maximum = {};", characters.len() + 9).unwrap();
    for index in 0..characters.len() {
        writeln!(keymap, "<K{}> = {};", index + 1, index + 9).unwrap();
    }
    keymap.push_str(
        "};\n\
         xkb_types \"termway\" { include \"complete\" };\n\
         xkb_compatibility \"termway\" { include \"complete\" };\n\
         xkb_symbols \"termway\" {\n",
    );
    for (index, character) in characters.iter().enumerate() {
        writeln!(
            keymap,
            "key <K{}> {{ [ U{:04X} ] }};",
            index + 1,
            u32::from(*character)
        )
        .unwrap();
    }
    keymap.push_str("};\n};\n\0");
    keymap
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for InputState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = state
                .outputs
                .iter_mut()
                .find(|output| output.proxy == *proxy)
        {
            output.name = Some(name);
        }
    }
}

delegate_noop!(InputState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(InputState: ignore ZwlrVirtualPointerV1);
delegate_noop!(InputState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(InputState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(InputState: ignore wl_seat::WlSeat);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_unicode_xkb_keymap() {
        let keymap = unicode_keymap(&['你', '😀']);
        assert!(keymap.contains("<K1> = 9;"));
        assert!(keymap.contains("key <K1> { [ U4F60 ] };"));
        assert!(keymap.contains("key <K2> { [ U1F600 ] };"));
        assert!(keymap.ends_with('\0'));
    }

    #[test]
    fn maps_virtual_pointer_buttons_to_linux_evdev_codes() {
        assert_eq!(PointerButton::Left.evdev_code(), BTN_LEFT);
        assert_eq!(PointerButton::Right.evdev_code(), BTN_RIGHT);
    }
}
