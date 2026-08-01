use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

const BTN_LEFT: u32 = 0x110;

pub struct VirtualPointer {
    connection: Connection,
    _queue: EventQueue<InputState>,
    _manager: ZwlrVirtualPointerManagerV1,
    pointer: ZwlrVirtualPointerV1,
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

    pub fn click(&self, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || x >= width || y >= height {
            bail!("refusing out-of-bounds pointer coordinate ({x},{y}) in {width}x{height}");
        }
        let time = self.started.elapsed().as_millis() as u32;
        self.pointer.motion_absolute(time, x, y, width, height);
        self.pointer.frame();
        self.pointer
            .button(time, BTN_LEFT, wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        self.pointer
            .button(time, BTN_LEFT, wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.connection
            .flush()
            .context("cannot send virtual click")?;
        Ok(())
    }
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
