use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use image::RgbImage;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

pub struct ScreencopySession {
    connection: Connection,
    queue: EventQueue<CaptureState>,
    state: CaptureState,
    manager: ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
}

#[derive(Default)]
struct CaptureState {
    outputs: Vec<Output>,
    shm: Option<wl_shm::WlShm>,
    buffer: Option<ShmBuffer>,
    frame: FrameState,
}

struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
}

struct ShmBuffer {
    file: File,
    proxy: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
}

#[derive(Default)]
struct FrameState {
    status: FrameStatus,
    y_invert: bool,
}

#[derive(Default, PartialEq, Eq)]
enum FrameStatus {
    #[default]
    WaitingForBuffer,
    WaitingForReady,
    Ready,
    Failed,
}

impl ScreencopySession {
    pub fn connect(runtime_dir: &Path, display: &str, output_name: &str) -> Result<Self> {
        let socket_path = if Path::new(display).is_absolute() {
            Path::new(display).to_path_buf()
        } else {
            runtime_dir.join(display)
        };
        let socket = UnixStream::connect(&socket_path)
            .with_context(|| format!("cannot connect to Wayland at {}", socket_path.display()))?;
        let connection = Connection::from_socket(socket).context("cannot initialize Wayland")?;
        let (globals, mut queue) = registry_queue_init::<CaptureState>(&connection)
            .context("cannot read Wayland globals")?;
        let qh = queue.handle();

        // Version 1 guarantees a wl_shm buffer description. Newer compositors remain
        // backwards compatible, while dmabuf negotiation can be added independently.
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("compositor does not support wlr-screencopy")?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .context("compositor does not expose wl_shm")?;

        let mut state = CaptureState {
            shm: Some(shm),
            ..CaptureState::default()
        };
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
            .with_context(|| format!("Wayland has no output named {output_name}"))?
            .proxy
            .clone();

        Ok(Self {
            connection,
            queue,
            state,
            manager,
            output,
        })
    }

    pub fn capture(&mut self) -> Result<RgbImage> {
        self.state.frame = FrameState::default();
        let qh = self.queue.handle();
        let frame = self.manager.capture_output(0, &self.output, &qh, ());
        self.connection
            .flush()
            .context("cannot request screencopy")?;

        while !matches!(
            self.state.frame.status,
            FrameStatus::Ready | FrameStatus::Failed
        ) {
            self.queue
                .blocking_dispatch(&mut self.state)
                .context("Wayland screencopy dispatch failed")?;
        }
        frame.destroy();
        if self.state.frame.status == FrameStatus::Failed {
            bail!("compositor failed the wlr-screencopy request");
        }

        let buffer = self
            .state
            .buffer
            .as_ref()
            .context("screencopy completed without a buffer")?;
        let byte_len = buffer
            .stride
            .checked_mul(buffer.height)
            .context("screencopy buffer size overflow")? as usize;
        let mut bytes = vec![0; byte_len];
        buffer
            .file
            .read_exact_at(&mut bytes, 0)
            .context("cannot read screencopy shared memory")?;
        pixels_to_rgb(
            &bytes,
            buffer.width,
            buffer.height,
            buffer.stride,
            buffer.format,
            self.state.frame.y_invert,
        )
    }
}

impl Dispatch<wl_output::WlOutput, ()> for CaptureState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state
                .outputs
                .iter_mut()
                .find(|entry| entry.proxy == *output)
        {
            entry.name = Some(name);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let WEnum::Value(format) = format else {
                    state.frame.status = FrameStatus::Failed;
                    return;
                };
                if ensure_buffer(state, qh, width, height, stride, format).is_err() {
                    state.frame.status = FrameStatus::Failed;
                    return;
                }
                frame.copy(&state.buffer.as_ref().expect("buffer was created").proxy);
                state.frame.status = FrameStatus::WaitingForReady;
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                state.frame.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame.status = FrameStatus::Ready;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame.status = FrameStatus::Failed;
            }
            _ => {}
        }
    }
}

fn ensure_buffer(
    state: &mut CaptureState,
    qh: &QueueHandle<CaptureState>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Result<()> {
    if state.buffer.as_ref().is_some_and(|buffer| {
        buffer.width == width
            && buffer.height == height
            && buffer.stride == stride
            && buffer.format == format
    }) {
        return Ok(());
    }
    if let Some(old) = state.buffer.take() {
        old.proxy.destroy();
    }

    let size = stride
        .checked_mul(height)
        .context("screencopy buffer size overflow")?;
    let size_i32 = i32::try_from(size).context("screencopy buffer is too large for wl_shm")?;
    let width_i32 = i32::try_from(width).context("screencopy width is too large")?;
    let height_i32 = i32::try_from(height).context("screencopy height is too large")?;
    let stride_i32 = i32::try_from(stride).context("screencopy stride is too large")?;
    let fd = memfd_create(c"termway-screencopy", MemfdFlags::CLOEXEC)
        .context("cannot create screencopy shared memory")?;
    let file = File::from(fd);
    file.set_len(u64::from(size))
        .context("cannot size screencopy shared memory")?;
    let shm = state.shm.as_ref().context("wl_shm disappeared")?;
    let pool = shm.create_pool(file.as_fd(), size_i32, qh, ());
    let proxy = pool.create_buffer(0, width_i32, height_i32, stride_i32, format, qh, ());
    pool.destroy();
    state.buffer = Some(ShmBuffer {
        file,
        proxy,
        width,
        height,
        stride,
        format,
    });
    Ok(())
}

fn pixels_to_rgb(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<RgbImage> {
    if !matches!(
        format,
        wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Abgr8888
    ) {
        bail!("unsupported screencopy SHM format {format:?}");
    }
    let required = stride
        .checked_mul(height)
        .context("screencopy dimensions overflow")? as usize;
    if bytes.len() < required || stride < width.saturating_mul(4) {
        bail!("invalid screencopy pixel buffer");
    }
    let bgr = matches!(format, wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888);
    let mut rgb = vec![0; width as usize * height as usize * 3];
    for destination_y in 0..height {
        let source_y = if y_invert {
            height - 1 - destination_y
        } else {
            destination_y
        };
        for x in 0..width {
            let source = (source_y * stride + x * 4) as usize;
            let pixel = u32::from_ne_bytes(bytes[source..source + 4].try_into().unwrap());
            let destination = ((destination_y * width + x) * 3) as usize;
            let (red, green, blue) = if bgr {
                (pixel as u8, (pixel >> 8) as u8, (pixel >> 16) as u8)
            } else {
                ((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8)
            };
            rgb[destination..destination + 3].copy_from_slice(&[red, green, blue]);
        }
    }
    RgbImage::from_raw(width, height, rgb).context("invalid converted screencopy image")
}

delegate_noop!(CaptureState: ignore wl_registry::WlRegistry);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ignore ZwlrScreencopyManagerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_xrgb_with_stride_padding() {
        let pixels = [0x33, 0x22, 0x11, 0, 0x66, 0x55, 0x44, 0, 9, 9, 9, 9];
        let image = pixels_to_rgb(&pixels, 2, 1, 12, wl_shm::Format::Xrgb8888, false).unwrap();
        assert_eq!(image.as_raw(), &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn converts_abgr_and_flips_rows() {
        let pixels = [0x11, 0x22, 0x33, 0xff, 0xaa, 0xbb, 0xcc, 0xff];
        let image = pixels_to_rgb(&pixels, 1, 2, 4, wl_shm::Format::Abgr8888, true).unwrap();
        assert_eq!(image.as_raw(), &[0xaa, 0xbb, 0xcc, 0x11, 0x22, 0x33]);
    }
}
