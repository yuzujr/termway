use std::path::Path;
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
    session: ScreencopySession,
}

impl Capturer {
    pub fn new(runtime_dir: &Path, wayland_display: &str, output_name: &str) -> Result<Self> {
        let session = ScreencopySession::connect(runtime_dir, wayland_display, output_name)
            .context("cannot start screencopy capture")?;
        Ok(Self { session })
    }

    pub fn capture(&mut self) -> Result<RgbImage> {
        self.session.capture()
    }
}
