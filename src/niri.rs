use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct Client {
    socket_path: PathBuf,
    stream: BufReader<UnixStream>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub version: String,
    pub output_count: usize,
    pub window_count: usize,
    pub focused_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputGeometry {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: String,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("cannot connect to niri IPC at {}", path.display()))?;
        Ok(Self {
            socket_path: path.to_path_buf(),
            stream: BufReader::new(stream),
        })
    }

    pub fn request(&mut self, request: &str) -> Result<Value> {
        let encoded = serde_json::to_string(request)?;
        self.stream
            .get_mut()
            .write_all(format!("{encoded}\n").as_bytes())
            .with_context(|| format!("cannot write to {}", self.socket_path.display()))?;

        let mut line = String::new();
        let bytes = self.stream.read_line(&mut line)?;
        if bytes == 0 {
            bail!("niri closed the IPC socket before replying to {request}");
        }
        let reply: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid niri reply to {request}"))?;
        unwrap_reply(reply)
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let version_reply = self.request("Version")?;
        let outputs_reply = self.request("Outputs")?;
        let windows_reply = self.request("Windows")?;
        let focused_reply = self.request("FocusedWindow")?;

        let version = variant(&version_reply, "Version")?
            .as_str()
            .context("niri Version response is not a string")?
            .to_owned();
        let outputs = variant(&outputs_reply, "Outputs")?;
        let windows = variant(&windows_reply, "Windows")?;
        let focused = variant(&focused_reply, "FocusedWindow")?;

        let focused_window = if focused.is_null() {
            None
        } else {
            let title = focused.get("title").and_then(Value::as_str);
            let app_id = focused.get("app_id").and_then(Value::as_str);
            Some(match (title, app_id) {
                (Some(title), Some(app_id)) => format!("{title} [{app_id}]"),
                (Some(title), None) => title.to_owned(),
                (None, Some(app_id)) => format!("[{app_id}]"),
                (None, None) => "untitled".to_owned(),
            })
        };

        Ok(Snapshot {
            version,
            output_count: outputs
                .as_object()
                .context("niri Outputs response is not an object")?
                .len(),
            window_count: windows
                .as_array()
                .context("niri Windows response is not an array")?
                .len(),
            focused_window,
        })
    }

    pub fn focused_output_name(&mut self) -> Result<Option<String>> {
        let reply = self.request("FocusedOutput")?;
        let output = variant(&reply, "FocusedOutput")?;
        if output.is_null() {
            return Ok(None);
        }
        output
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .map(Some)
            .context("niri FocusedOutput response has no string name")
    }

    pub fn output_geometry(&mut self, name: &str) -> Result<OutputGeometry> {
        let reply = self.request("Outputs")?;
        let outputs = variant(&reply, "Outputs")?
            .as_object()
            .context("niri Outputs response is not an object")?;
        let output = outputs
            .get(name)
            .with_context(|| format!("niri has no output named {name}"))?;
        parse_output_geometry(output)
    }
}

fn parse_output_geometry(output: &Value) -> Result<OutputGeometry> {
    let name = output
        .get("name")
        .and_then(Value::as_str)
        .context("niri output has no string name")?
        .to_owned();
    let logical = output
        .get("logical")
        .context("niri output is disabled and has no logical geometry")?;
    Ok(OutputGeometry {
        name,
        x: json_i32(logical, "x")?,
        y: json_i32(logical, "y")?,
        width: json_u32(logical, "width")?,
        height: json_u32(logical, "height")?,
        scale: logical
            .get("scale")
            .and_then(Value::as_f64)
            .context("niri output logical scale is not a number")?,
        transform: logical
            .get("transform")
            .and_then(Value::as_str)
            .context("niri output logical transform is not a string")?
            .to_owned(),
    })
}

fn json_i32(value: &Value, field: &str) -> Result<i32> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|number| i32::try_from(number).ok())
        .with_context(|| format!("niri output logical {field} is not an i32"))
}

fn json_u32(value: &Value, field: &str) -> Result<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .with_context(|| format!("niri output logical {field} is not a u32"))
}

pub fn probe_event_stream(path: &Path, timeout: Duration) -> Result<Value> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(b"\"EventStream\"\n")?;
    let mut reader = BufReader::new(stream);

    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    let reply = unwrap_reply(serde_json::from_str(&reply)?)?;
    if reply != Value::String("Handled".into()) {
        bail!("unexpected EventStream acknowledgement: {reply}");
    }

    let mut event = String::new();
    let bytes = reader
        .read_line(&mut event)
        .context("niri event stream did not produce an initial state event")?;
    if bytes == 0 {
        bail!("niri closed the event stream before sending initial state");
    }
    Ok(serde_json::from_str(&event)?)
}

pub fn read_events(path: &Path, count: usize) -> Result<Vec<Value>> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(b"\"EventStream\"\n")?;
    let mut reader = BufReader::new(stream);

    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    let reply = unwrap_reply(serde_json::from_str(&reply)?)?;
    if reply != Value::String("Handled".into()) {
        bail!("unexpected EventStream acknowledgement: {reply}");
    }
    reader.get_mut().shutdown(Shutdown::Write)?;

    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("niri event stream ended after {} events", events.len());
        }
        events.push(serde_json::from_str(&line)?);
    }
    Ok(events)
}

pub fn event_name(event: &Value) -> &str {
    event
        .as_object()
        .and_then(|object| object.keys().next())
        .map(String::as_str)
        .unwrap_or("unknown event")
}

fn unwrap_reply(reply: Value) -> Result<Value> {
    let object = reply.as_object().context("niri reply is not an object")?;
    if let Some(error) = object.get("Err") {
        bail!("niri returned an error: {error}");
    }
    object
        .get("Ok")
        .cloned()
        .context("niri reply contains neither Ok nor Err")
}

fn variant<'a>(value: &'a Value, name: &str) -> Result<&'a Value> {
    value
        .as_object()
        .and_then(|object| object.get(name))
        .with_context(|| format!("expected niri {name} response, got {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_successful_reply() {
        assert_eq!(
            unwrap_reply(json!({"Ok": {"Version": "26.04"}})).unwrap(),
            json!({"Version": "26.04"})
        );
    }

    #[test]
    fn reports_niri_error() {
        assert!(unwrap_reply(json!({"Err": "not allowed"})).is_err());
    }

    #[test]
    fn extracts_event_variant_name() {
        assert_eq!(
            event_name(&json!({"WindowsChanged": {"windows": []}})),
            "WindowsChanged"
        );
    }

    #[test]
    fn parses_output_geometry() {
        let output = json!({
            "name": "eDP-1",
            "logical": {
                "x": -1920,
                "y": 0,
                "width": 2048,
                "height": 1280,
                "scale": 1.25,
                "transform": "Normal"
            }
        });
        assert_eq!(
            parse_output_geometry(&output).unwrap(),
            OutputGeometry {
                name: "eDP-1".into(),
                x: -1920,
                y: 0,
                width: 2048,
                height: 1280,
                scale: 1.25,
                transform: "Normal".into(),
            }
        );
    }
}
