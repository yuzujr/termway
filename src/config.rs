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
    #[serde(default)]
    pub actions: Vec<Action>,
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
