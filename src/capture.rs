use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use image::RgbImage;

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
