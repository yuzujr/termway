use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::niri;

#[derive(Debug, Clone, Copy)]
pub enum Source {
    CommandLine,
    ProcessEnvironment,
    SystemdEnvironment,
    RuntimeScan,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::CommandLine => "command line",
            Self::ProcessEnvironment => "process environment",
            Self::SystemdEnvironment => "systemd user environment",
            Self::RuntimeScan => "runtime directory scan",
        };
        f.write_str(value)
    }
}

#[derive(Debug)]
pub struct GraphicalSession {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub wayland_display: Option<String>,
    pub action_environment: Vec<(OsString, OsString)>,
    pub source: Source,
}

const GRAPHICAL_ENVIRONMENT_KEYS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
];

pub fn discover(override_socket: Option<&Path>) -> Result<GraphicalSession> {
    let process_env = env::vars_os().collect::<HashMap<_, _>>();
    let systemd_env = systemd_user_environment();
    let runtime_dir = runtime_dir(&process_env, &systemd_env)?;

    if let Some(path) = override_socket {
        validate_socket(path)?;
        return Ok(session(
            path.to_path_buf(),
            runtime_dir,
            Source::CommandLine,
            &process_env,
            &systemd_env,
        ));
    }

    if let Some(path) = process_env
        .get(OsStr::new("NIRI_SOCKET"))
        .map(PathBuf::from)
        .filter(|path| validate_socket(path).is_ok())
    {
        return Ok(session(
            path,
            runtime_dir,
            Source::ProcessEnvironment,
            &process_env,
            &systemd_env,
        ));
    }

    if let Some(path) = systemd_env
        .get(OsStr::new("NIRI_SOCKET"))
        .map(PathBuf::from)
        .filter(|path| validate_socket(path).is_ok())
    {
        return Ok(session(
            path,
            runtime_dir,
            Source::SystemdEnvironment,
            &process_env,
            &systemd_env,
        ));
    }

    let preferred_display =
        value(&process_env, "WAYLAND_DISPLAY").or_else(|| value(&systemd_env, "WAYLAND_DISPLAY"));
    let candidates = scan_sockets(&runtime_dir)?;
    let valid = candidates
        .into_iter()
        .filter(|path| validate_socket(path).is_ok())
        .collect::<Vec<_>>();

    let selected = select_candidate(&valid, preferred_display.as_deref())?;
    Ok(session(
        selected,
        runtime_dir,
        Source::RuntimeScan,
        &process_env,
        &systemd_env,
    ))
}

fn session(
    socket_path: PathBuf,
    runtime_dir: PathBuf,
    source: Source,
    process_env: &HashMap<std::ffi::OsString, std::ffi::OsString>,
    systemd_env: &HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> GraphicalSession {
    let wayland_display = value(process_env, "WAYLAND_DISPLAY")
        .or_else(|| value(systemd_env, "WAYLAND_DISPLAY"))
        .or_else(|| display_from_socket(&socket_path));
    let action_environment = graphical_environment(process_env, systemd_env);
    GraphicalSession {
        runtime_dir,
        socket_path,
        wayland_display,
        action_environment,
        source,
    }
}

fn graphical_environment(
    process_env: &HashMap<OsString, OsString>,
    systemd_env: &HashMap<OsString, OsString>,
) -> Vec<(OsString, OsString)> {
    GRAPHICAL_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| {
            // The systemd user manager belongs to the local graphical session.
            // Prefer it over values such as an SSH-forwarded DISPLAY.
            systemd_env
                .get(OsStr::new(key))
                .or_else(|| process_env.get(OsStr::new(key)))
                .map(|value| (OsString::from(key), value.clone()))
        })
        .collect()
}

fn runtime_dir(
    process_env: &HashMap<std::ffi::OsString, std::ffi::OsString>,
    systemd_env: &HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> Result<PathBuf> {
    if let Some(path) = process_env
        .get(OsStr::new("XDG_RUNTIME_DIR"))
        .or_else(|| systemd_env.get(OsStr::new("XDG_RUNTIME_DIR")))
    {
        return Ok(PathBuf::from(path));
    }

    let status = fs::read_to_string("/proc/self/status")
        .context("XDG_RUNTIME_DIR is unset and /proc/self/status cannot be read")?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().next())
        .context("could not determine the current uid")?;
    Ok(PathBuf::from(format!("/run/user/{uid}")))
}

fn systemd_user_environment() -> HashMap<std::ffi::OsString, std::ffi::OsString> {
    let Ok(output) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

fn scan_sockets(runtime_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(runtime_dir)
        .with_context(|| format!("cannot scan runtime directory {}", runtime_dir.display()))?;
    let mut sockets = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("niri.") || !name.ends_with(".sock") {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_socket()) {
            sockets.push(entry.path());
        }
    }
    sockets.sort();
    Ok(sockets)
}

fn select_candidate(candidates: &[PathBuf], display: Option<&str>) -> Result<PathBuf> {
    if let Some(display) = display {
        let prefix = format!("niri.{display}.");
        let matching = candidates
            .iter()
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            return Ok(matching[0].clone());
        }
    }

    match candidates {
        [only] => Ok(only.clone()),
        [] => bail!("no live niri IPC socket found; is the graphical session running?"),
        many => {
            let paths = many
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("multiple live niri sessions found ({paths}); pass --niri-socket PATH")
        }
    }
}

fn validate_socket(path: &Path) -> Result<()> {
    niri::Client::connect(path)
        .and_then(|mut client| client.request("Version").map(|_| ()))
        .with_context(|| format!("niri socket is not usable: {}", path.display()))
}

fn display_from_socket(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let middle = name.strip_prefix("niri.")?.strip_suffix(".sock")?;
    let (display, pid) = middle.rsplit_once('.')?;
    pid.parse::<u32>().ok()?;
    Some(display.to_owned())
}

fn value(vars: &HashMap<std::ffi::OsString, std::ffi::OsString>, key: &str) -> Option<String> {
    vars.get(OsStr::new(key))
        .map(|value| value.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_wayland_display_from_socket_name() {
        assert_eq!(
            display_from_socket(Path::new("/run/user/1000/niri.wayland-1.1681.sock")),
            Some("wayland-1".into())
        );
    }

    #[test]
    fn rejects_unrecognized_socket_name() {
        assert_eq!(display_from_socket(Path::new("/tmp/niri.sock")), None);
    }

    #[test]
    fn selects_candidate_matching_display() {
        let candidates = vec![
            PathBuf::from("/run/user/1000/niri.wayland-1.10.sock"),
            PathBuf::from("/run/user/1000/niri.wayland-2.11.sock"),
        ];
        assert_eq!(
            select_candidate(&candidates, Some("wayland-2")).unwrap(),
            candidates[1]
        );
    }

    #[test]
    fn refuses_ambiguous_sessions() {
        let candidates = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert!(select_candidate(&candidates, None).is_err());
    }

    #[test]
    fn graphical_environment_prefers_local_systemd_session() {
        let process = HashMap::from([
            (OsString::from("DISPLAY"), OsString::from("localhost:10.0")),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("ssh-bus"),
            ),
        ]);
        let systemd = HashMap::from([
            (OsString::from("DISPLAY"), OsString::from(":0")),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("local-bus"),
            ),
            (
                OsString::from("UNRELATED_SECRET"),
                OsString::from("ignored"),
            ),
        ]);

        let environment = graphical_environment(&process, &systemd)
            .into_iter()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get(OsStr::new("DISPLAY")),
            Some(&OsString::from(":0"))
        );
        assert_eq!(
            environment.get(OsStr::new("DBUS_SESSION_BUS_ADDRESS")),
            Some(&OsString::from("local-bus"))
        );
        assert!(!environment.contains_key(OsStr::new("UNRELATED_SECRET")));
    }
}
