use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Prefer this output when `--output` is not present. When unset, use the focused output.
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub graphics: GraphicsConfig,
    #[serde(default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphicsConfig {
    pub quality: QualityMode,
    pub resolution: Resolution,
    pub advanced: AdvancedGraphicsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdvancedGraphicsConfig {
    pub adaptive_min_height: u32,
    pub adaptive_damage_frames: u32,
    pub adaptive_damage_window_ms: u64,
    pub frame_budget_ms: u64,
    pub recovery_ms: u64,
    pub preview_ms: u64,
    pub atlas_refresh_ms: u64,
    pub tmux_bandwidth_mbps: f64,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            quality: QualityMode::Auto,
            resolution: Resolution::new(1_920, 1_080),
            advanced: AdvancedGraphicsConfig::default(),
        }
    }
}

impl Default for AdvancedGraphicsConfig {
    fn default() -> Self {
        Self {
            adaptive_min_height: 360,
            adaptive_damage_frames: 2,
            adaptive_damage_window_ms: 500,
            frame_budget_ms: 275,
            recovery_ms: 2_000,
            preview_ms: 120,
            atlas_refresh_ms: 2_000,
            tmux_bandwidth_mbps: 40.0,
        }
    }
}

impl GraphicsConfig {
    pub fn validate(&self) -> Result<()> {
        let resolution = self.resolution;
        if resolution.width == 0 || resolution.width > 7_680 {
            bail!("graphics.resolution width must be between 1 and 7680");
        }
        if resolution.height == 0 || resolution.height > 4_320 {
            bail!("graphics.resolution height must be between 1 and 4320");
        }
        let advanced = &self.advanced;
        if advanced.adaptive_min_height == 0 || advanced.adaptive_min_height > resolution.height {
            bail!(
                "graphics.advanced.adaptive_min_height must not exceed graphics.resolution height"
            );
        }
        if advanced.adaptive_damage_frames == 0 {
            bail!("graphics.advanced.adaptive_damage_frames must be greater than zero");
        }
        if advanced.adaptive_damage_window_ms == 0 {
            bail!("graphics.advanced.adaptive_damage_window_ms must be greater than zero");
        }
        if advanced.frame_budget_ms == 0 {
            bail!("graphics.advanced.frame_budget_ms must be greater than zero");
        }
        if advanced.recovery_ms == 0 {
            bail!("graphics.advanced.recovery_ms must be greater than zero");
        }
        if !advanced.tmux_bandwidth_mbps.is_finite() || advanced.tmux_bandwidth_mbps <= 0.0 {
            bail!("graphics.advanced.tmux_bandwidth_mbps must be a positive finite number");
        }
        Ok(())
    }
}

/// The three policies shown in the in-app display settings. They intentionally describe user
/// intent instead of exposing the renderer's damage and navigation heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityMode {
    Auto,
    Sharp,
    Fast,
}

impl QualityMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Sharp => "Sharp",
            Self::Fast => "Fast",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "full detail for navigation, adapts during motion",
            Self::Sharp => "always use the selected resolution",
            Self::Fast => "adapt whenever bandwidth is tight",
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Auto => Self::Fast,
            Self::Sharp => Self::Auto,
            Self::Fast => Self::Sharp,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Sharp,
            Self::Sharp => Self::Fast,
            Self::Fast => Self::Auto,
        }
    }

    pub fn adaptive_quality(self) -> bool {
        self != Self::Sharp
    }

    pub fn adaptive_navigation(self) -> bool {
        self == Self::Fast
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Resolution {
    width: u32,
    height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }
}

impl TryFrom<String> for Resolution {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let value = value.trim().to_ascii_lowercase();
        let preset = match value.as_str() {
            "native" => Some(Self::new(7_680, 4_320)),
            "2160p" | "4k" => Some(Self::new(3_840, 2_160)),
            "1440p" => Some(Self::new(2_560, 1_440)),
            "1080p" => Some(Self::new(1_920, 1_080)),
            "720p" => Some(Self::new(1_280, 720)),
            _ => None,
        };
        if let Some(resolution) = preset {
            return Ok(resolution);
        }
        let Some((width, height)) = value.split_once('x') else {
            return Err(
                "resolution must be native, 4k, 2160p, 1440p, 1080p, 720p, or WIDTHxHEIGHT"
                    .to_owned(),
            );
        };
        let width = width
            .parse::<u32>()
            .map_err(|_| "resolution width must be a number".to_owned())?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "resolution height must be a number".to_owned())?;
        Ok(Self::new(width, height))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: Vec<String>,
}

pub struct ActionRunner {
    runtime_dir: PathBuf,
    wayland_display: String,
    niri_socket: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl ActionRunner {
    pub fn new(
        runtime_dir: &Path,
        wayland_display: &str,
        niri_socket: &Path,
        environment: &[(OsString, OsString)],
    ) -> Self {
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
            wayland_display: wayland_display.to_owned(),
            niri_socket: niri_socket.to_path_buf(),
            environment: environment.to_vec(),
        }
    }

    pub fn spawn(&self, action: &Action) -> Result<()> {
        let mut command = Command::new(&action.command[0]);
        command
            .args(&action.command[1..])
            .envs(self.environment.iter().map(|(key, value)| (key, value)))
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .env("NIRI_SOCKET", &self.niri_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start action {:?}", action.name))?;
        thread::Builder::new()
            .name("termway-action".to_owned())
            .spawn(move || {
                let _ = child.wait();
            })
            .context("cannot start action reaper")?;
        Ok(())
    }
}

pub fn load(explicit_path: Option<&Path>) -> Result<Config> {
    let path = explicit_path.map(Path::to_path_buf).or_else(default_path);
    let Some(path) = path else {
        return Ok(Config::default());
    };
    if explicit_path.is_none() && !path.exists() {
        return Ok(Config::default());
    }
    let source = fs::read_to_string(&path)
        .with_context(|| format!("cannot read termway config at {}", path.display()))?;
    let config: Config = toml::from_str(&source)
        .with_context(|| format!("invalid termway config at {}", path.display()))?;
    validate(config).with_context(|| format!("invalid termway config at {}", path.display()))
}

fn default_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(path).join("termway/config.toml"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/termway/config.toml"))
}

fn validate(config: Config) -> Result<Config> {
    if config
        .output
        .as_ref()
        .is_some_and(|output| output.trim().is_empty())
    {
        bail!("output must not be empty");
    }
    config.graphics.validate()?;
    for (index, action) in config.actions.iter().enumerate() {
        if action.name.trim().is_empty() {
            bail!("actions[{index}].name must not be empty");
        }
        if action.name.chars().any(char::is_control) {
            bail!(
                "action {:?} has control characters in its name",
                action.name
            );
        }
        if action.command.is_empty() || action.command[0].is_empty() {
            bail!("action {:?} must have a non-empty command", action.name);
        }
        if config.actions[..index]
            .iter()
            .any(|previous| previous.name == action.name)
        {
            bail!("duplicate action name {:?}", action.name);
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions() {
        let config: Config = toml::from_str(
            r#"
                [[actions]]
                name = "terminal"
                description = "Open Kitty"
                command = ["kitty", "--single-instance"]
            "#,
        )
        .unwrap();
        let config = validate(config).unwrap();
        assert_eq!(config.actions[0].name, "terminal");
        assert_eq!(config.actions[0].command[1], "--single-instance");
    }

    #[test]
    fn parses_graphics_and_output_settings() {
        let config: Config = toml::from_str(
            r#"
                output = "DP-1"
                [graphics]
                quality = "sharp"
                resolution = "1440p"
                [graphics.advanced]
                recovery_ms = 750
            "#,
        )
        .unwrap();
        let config = validate(config).unwrap();
        assert_eq!(config.output.as_deref(), Some("DP-1"));
        assert_eq!(config.graphics.resolution, Resolution::new(2560, 1440));
        assert_eq!(config.graphics.quality, QualityMode::Sharp);
        assert_eq!(config.graphics.advanced.adaptive_min_height, 360);
        assert_eq!(config.graphics.advanced.recovery_ms, 750);
    }

    #[test]
    fn accepts_named_and_custom_graphics_settings() {
        let custom: Config = toml::from_str(
            r#"
                [graphics]
                resolution = "2560x1600"
                quality = "fast"
            "#,
        )
        .unwrap();
        assert_eq!(custom.graphics.resolution, Resolution::new(2560, 1600));
        assert_eq!(custom.graphics.quality, QualityMode::Fast);
    }

    #[test]
    fn rejects_removed_graphics_settings() {
        let legacy = toml::from_str::<Config>(
            r#"
                [graphics]
                max_width = 1600
                max_height = 900
                adaptive_quality = false
            "#,
        );
        assert!(legacy.is_err());
    }

    #[test]
    fn rejects_invalid_graphics_settings() {
        let too_tall: Config = toml::from_str(
            r#"
                [graphics]
                resolution = "720p"
                [graphics.advanced]
                adaptive_min_height = 1080
            "#,
        )
        .unwrap();
        assert!(validate(too_tall).is_err());

        let invalid_bandwidth: Config = toml::from_str(
            r#"
                [graphics]
                [graphics.advanced]
                tmux_bandwidth_mbps = 0
            "#,
        )
        .unwrap();
        assert!(validate(invalid_bandwidth).is_err());
    }

    #[test]
    fn rejects_empty_commands_and_duplicate_names() {
        let empty: Config = toml::from_str(
            r#"
                [[actions]]
                name = "broken"
                command = []
            "#,
        )
        .unwrap();
        assert!(validate(empty).is_err());

        let duplicate: Config = toml::from_str(
            r#"
                [[actions]]
                name = "same"
                command = ["true"]
                [[actions]]
                name = "same"
                command = ["false"]
            "#,
        )
        .unwrap();
        assert!(validate(duplicate).is_err());
    }
}
