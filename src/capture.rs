use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use image::RgbImage;

use crate::screencopy::{DamageRect, ScreencopySession};

const DAMAGE_FRAME_INTERVAL: Duration = Duration::from_millis(200);

pub struct DamageUpdate {
    pub image: RgbImage,
    pub damage: Vec<DamageRect>,
}

pub struct DamageWatcher {
    latest: Arc<Mutex<Option<Result<DamageUpdate, String>>>>,
}

impl DamageWatcher {
    pub fn spawn(runtime_dir: &Path, wayland_display: &str, output_name: &str) -> Result<Self> {
        let mut session = ScreencopySession::connect(runtime_dir, wayland_display, output_name)
            .context("cannot start damage watcher")?;
        if !session.supports_damage() {
            bail!("compositor does not support screencopy damage tracking");
        }
        let latest = Arc::new(Mutex::new(None));
        let thread_latest = Arc::clone(&latest);
        thread::Builder::new()
            .name("termway-damage".to_owned())
            .spawn(move || {
                let mut next_capture = Instant::now();
                loop {
                    if let Some(delay) = next_capture.checked_duration_since(Instant::now()) {
                        thread::sleep(delay);
                    }
                    let update = session.capture_with_damage().map(|frame| DamageUpdate {
                        image: frame.image,
                        damage: frame.damage,
                    });
                    next_capture = Instant::now() + DAMAGE_FRAME_INTERVAL;
                    let failed = update.is_err();
                    let value = update.map_err(|error| format!("{error:#}"));
                    let Ok(mut slot) = thread_latest.lock() else {
                        break;
                    };
                    *slot = Some(value);
                    if failed {
                        break;
                    }
                }
            })
            .context("cannot spawn damage watcher")?;
        Ok(Self { latest })
    }

    pub fn take_latest(&self) -> Result<Option<DamageUpdate>> {
        let mut slot = self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("damage watcher state is poisoned"))?;
        match slot.take() {
            Some(Ok(update)) => Ok(Some(update)),
            Some(Err(error)) => bail!("damage watcher failed: {error}"),
            None => Ok(None),
        }
    }
}

pub struct Capturer {
    native: Option<ScreencopySession>,
    runtime_dir: PathBuf,
    wayland_display: String,
    output_name: String,
    fallback_reason: Option<String>,
}

impl Capturer {
    pub fn new(runtime_dir: &Path, wayland_display: &str, output_name: &str) -> Self {
        let (native, fallback_reason) =
            match ScreencopySession::connect(runtime_dir, wayland_display, output_name) {
                Ok(session) => (Some(session), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
        Self {
            native,
            runtime_dir: runtime_dir.to_path_buf(),
            wayland_display: wayland_display.to_owned(),
            output_name: output_name.to_owned(),
            fallback_reason,
        }
    }

    pub fn capture(&mut self) -> Result<RgbImage> {
        if let Some(native) = &mut self.native {
            match native.capture() {
                Ok(image) => return Ok(image),
                Err(error) => {
                    self.fallback_reason = Some(format!("{error:#}"));
                    self.native = None;
                }
            }
        }
        capture_with_grim(
            &self.runtime_dir,
            &self.wayland_display,
            Some(&self.output_name),
        )
        .with_context(|| {
            self.fallback_reason.as_ref().map_or_else(
                || "grim fallback failed".to_owned(),
                |reason| format!("native screencopy failed ({reason}); grim fallback also failed"),
            )
        })
    }

    pub fn backend_name(&self) -> &'static str {
        if self.native.is_some() {
            "wlr-screencopy"
        } else {
            "grim"
        }
    }

    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }
}

pub fn capture_with_grim(
    runtime_dir: &Path,
    wayland_display: &str,
    output_name: Option<&str>,
) -> Result<RgbImage> {
    let mut command = Command::new("grim");
    command
        .args(["-t", "ppm"])
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", wayland_display);
    if let Some(output_name) = output_name {
        command.args(["-o", output_name]);
    }
    command.arg("-");

    let output = command
        .output()
        .context("cannot run grim; enter `nix develop` or install grim on the remote Linux host")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("grim failed: {}", stderr.trim());
    }
    parse_ppm(&output.stdout)
}

pub fn parse_ppm(bytes: &[u8]) -> Result<RgbImage> {
    let mut cursor = 0;
    let magic = next_token(bytes, &mut cursor).context("PPM is missing its magic value")?;
    if magic != b"P6" {
        bail!("unsupported PPM format; expected P6");
    }
    let width = parse_dimension(next_token(bytes, &mut cursor), "width")?;
    let height = parse_dimension(next_token(bytes, &mut cursor), "height")?;
    let max = parse_dimension(next_token(bytes, &mut cursor), "max channel value")?;
    if max != 255 {
        bail!("unsupported PPM max channel value {max}; expected 255");
    }

    consume_pixel_separator(bytes, &mut cursor)?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .context("PPM dimensions overflow addressable memory")?;
    let end = cursor
        .checked_add(expected)
        .context("PPM payload offset overflows addressable memory")?;
    let pixels = bytes
        .get(cursor..end)
        .context("PPM pixel payload is truncated")?;
    RgbImage::from_raw(width, height, pixels.to_vec())
        .context("PPM dimensions do not match its pixel payload")
}

fn next_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    loop {
        while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
        if bytes.get(*cursor) != Some(&b'#') {
            break;
        }
        while bytes.get(*cursor).is_some_and(|byte| *byte != b'\n') {
            *cursor += 1;
        }
    }

    let start = *cursor;
    while bytes
        .get(*cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        *cursor += 1;
    }
    (start != *cursor).then_some(&bytes[start..*cursor])
}

fn parse_dimension(token: Option<&[u8]>, name: &str) -> Result<u32> {
    let token = token.with_context(|| format!("PPM is missing its {name}"))?;
    let token = std::str::from_utf8(token).with_context(|| format!("PPM {name} is not UTF-8"))?;
    let value = token
        .parse::<u32>()
        .with_context(|| format!("PPM {name} is not an integer"))?;
    if value == 0 {
        bail!("PPM {name} must be greater than zero");
    }
    Ok(value)
}

fn consume_pixel_separator(bytes: &[u8], cursor: &mut usize) -> Result<()> {
    let separator = bytes.get(*cursor).context("PPM has no pixel payload")?;
    if !separator.is_ascii_whitespace() {
        bail!("PPM header has no whitespace before its pixel payload");
    }
    *cursor += 1;
    if *separator == b'\r' && bytes.get(*cursor) == Some(&b'\n') {
        *cursor += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_ppm() {
        let image = parse_ppm(b"P6\n2 1\n255\n\xff\x00\x00\x00\xff\x00").unwrap();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.as_raw(), &[255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn parses_comments_and_crlf() {
        let image = parse_ppm(b"P6\r\n# generated\r\n1 1\r\n255\r\n\x01\x02\x03").unwrap();
        assert_eq!(image.as_raw(), &[1, 2, 3]);
    }

    #[test]
    fn rejects_truncated_pixels() {
        assert!(parse_ppm(b"P6\n2 1\n255\n\x00\x00\x00").is_err());
    }
}
