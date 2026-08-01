use std::collections::BTreeMap;
use std::env;
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::ValueEnum;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::render::RasterTile;

const PAYLOAD_CHUNK_SIZE: usize = 4096;
const PLACEMENT_ID: u32 = 1;
const MAX_TILE_INDEX: u32 = 0x0fff;
const PLACEHOLDER: char = '\u{10eeee}';
const ROW_DIACRITICS: &[char] = &[
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}',
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum GraphicsMode {
    #[default]
    Auto,
    Ansi,
    Kitty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Direct,
    Tmux,
}

pub struct Renderer {
    transport: Transport,
    image_id_base: u32,
    active_tiles: BTreeMap<u32, u32>,
}

pub enum Selection {
    Ansi { reason: Option<String> },
    Kitty(Renderer),
}

impl Selection {
    pub fn select(mode: GraphicsMode) -> Result<Self> {
        if mode == GraphicsMode::Ansi {
            return Ok(Self::Ansi { reason: None });
        }

        match detect_transport() {
            Ok(transport) => Ok(Self::Kitty(Renderer {
                transport,
                // Unicode placeholders carry 24 bits of image id in their foreground color.
                // Reserve thirteen low bits for two image ids per tile and vary the next seven
                // by PID. All ids remain within the 24 bits carried by Unicode placeholders.
                image_id_base: 0x50_0000 | ((std::process::id() & 0x7f) << 13),
                active_tiles: BTreeMap::new(),
            })),
            Err(error) if mode == GraphicsMode::Auto => Ok(Self::Ansi {
                reason: Some(error.to_string()),
            }),
            Err(error) => Err(error),
        }
    }
}

impl Renderer {
    pub fn encode_tiles(&mut self, tiles: &[RasterTile], reset: bool) -> Result<Vec<Vec<u8>>> {
        let mut output = Vec::new();
        let mut stale_tiles = reset.then(|| std::mem::take(&mut self.active_tiles));
        for tile in tiles {
            if tile.index > MAX_TILE_INDEX {
                bail!("Kitty tile index {} exceeds {MAX_TILE_INDEX}", tile.index);
            }
            if self.transport == Transport::Tmux {
                if usize::from(tile.rows) > ROW_DIACRITICS.len() {
                    bail!(
                        "Kitty placeholder supports at most {} rows per tile, got {}",
                        ROW_DIACRITICS.len(),
                        tile.rows
                    );
                }
                if usize::from(tile.cols) > ROW_DIACRITICS.len() {
                    bail!(
                        "Kitty placeholder supports at most {} columns per tile, got {}",
                        ROW_DIACRITICS.len(),
                        tile.cols
                    );
                }
            }
            let previous_id = stale_tiles
                .as_mut()
                .and_then(|tiles| tiles.remove(&tile.index))
                .or_else(|| self.active_tiles.remove(&tile.index));
            let first_id = self.image_id_base + tile.index * 2;
            let image_id = if previous_id == Some(first_id) {
                first_id + 1
            } else {
                first_id
            };
            let payload = encode_png(&tile.image)?;
            for (index, chunk) in payload.as_bytes().chunks(PAYLOAD_CHUNK_SIZE).enumerate() {
                let more = usize::from((index + 1) * PAYLOAD_CHUNK_SIZE < payload.len());
                let control = if index == 0 {
                    format!("a=t,f=100,t=d,i={image_id},q=2,m={more}")
                } else {
                    format!("m={more},q=2")
                };
                push_apc(&mut output, self.transport, &control, chunk);
            }
            push_placement(&mut output, self.transport, image_id, tile, previous_id);
            // Never expose the background between versions. The new image/placeholder is fully
            // installed before the previous image and its placement are reclaimed.
            if self.transport == Transport::Tmux
                && let Some(previous_id) = previous_id
            {
                push_delete_image(&mut output, self.transport, previous_id);
            }
            self.active_tiles.insert(tile.index, image_id);
        }
        if let Some(stale_tiles) = stale_tiles {
            for image_id in stale_tiles.into_values() {
                push_delete_image(&mut output, self.transport, image_id);
            }
        }
        Ok(output)
    }

    pub fn encode_delete(&mut self) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        self.delete_active_images(&mut output);
        output
    }

    fn delete_active_images(&mut self, output: &mut Vec<Vec<u8>>) {
        for image_id in std::mem::take(&mut self.active_tiles).into_values() {
            push_delete_image(output, self.transport, image_id);
        }
    }
}

fn push_delete_image(output: &mut Vec<Vec<u8>>, transport: Transport, image_id: u32) {
    push_apc(output, transport, &format!("a=d,d=I,i={image_id},q=2"), &[]);
}

fn encode_png(image: &RgbImage) -> Result<String> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .context("cannot encode Kitty frame as PNG")?;
    Ok(STANDARD.encode(png))
}

fn push_placement(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    tile: &RasterTile,
    previous_id: Option<u32>,
) {
    let control = format!(
        "a=p,i={image_id},p={PLACEMENT_ID},c={},r={},C=1,q=2{}",
        tile.cols,
        tile.rows,
        if transport == Transport::Tmux {
            ",U=1"
        } else {
            ""
        },
    );
    if transport == Transport::Tmux {
        push_apc(output, transport, &control, &[]);
        push_placeholders(output, image_id, tile.col, tile.row, tile.cols, tile.rows);
        return;
    }

    use std::io::Write as _;

    let mut command = Vec::new();
    write!(
        command,
        "\x1b[{};{}H",
        u32::from(tile.row) + 1,
        u32::from(tile.col) + 1
    )
    .expect("writing to Vec cannot fail");
    command.extend_from_slice(&encode_apc(Transport::Direct, &control, &[]));
    if let Some(previous_id) = previous_id {
        command.extend_from_slice(&encode_apc(
            Transport::Direct,
            &format!("a=d,d=I,i={previous_id},q=2"),
            &[],
        ));
    }
    output.push(command);
}

fn push_placeholders(
    output: &mut Vec<Vec<u8>>,
    image_id: u32,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
) {
    use std::io::Write as _;

    let red = (image_id >> 16) & 0xff;
    let green = (image_id >> 8) & 0xff;
    let blue = image_id & 0xff;
    for relative_row in 0..rows {
        let mut line = Vec::new();
        write!(
            line,
            "\x1b[{};{}H\x1b[38;2;{red};{green};{blue}m",
            u32::from(row) + u32::from(relative_row) + 1,
            u32::from(col) + 1,
        )
        .expect("writing to Vec cannot fail");
        for relative_col in 0..cols {
            write!(
                line,
                "{PLACEHOLDER}{}{}",
                ROW_DIACRITICS[usize::from(relative_row)],
                ROW_DIACRITICS[usize::from(relative_col)],
            )
            .expect("writing to Vec cannot fail");
        }
        line.extend_from_slice(b"\x1b[0m");
        output.push(line);
    }
}

fn detect_transport() -> Result<Transport> {
    if env::var_os("TMUX").is_some() {
        let passthrough = tmux_value(&["show-options", "-gqv", "allow-passthrough"])?;
        if !matches!(passthrough.as_str(), "on" | "all") {
            bail!("tmux has allow-passthrough={passthrough:?}; enable it or use --graphics ansi");
        }
        let terminal = tmux_client_terminal()?;
        if !supports_kitty_graphics(&terminal) {
            bail!("tmux client terminal {terminal:?} is not known to support Kitty graphics");
        }
        return Ok(Transport::Tmux);
    }

    let terminal = env::var("TERM").unwrap_or_default();
    let program = env::var("TERM_PROGRAM").unwrap_or_default();
    if env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var_os("WEZTERM_PANE").is_some()
        || env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || supports_kitty_graphics(&terminal)
        || supports_kitty_graphics(&program)
    {
        Ok(Transport::Direct)
    } else {
        bail!("terminal does not advertise Kitty graphics support")
    }
}

fn tmux_client_terminal() -> Result<String> {
    let mut command = Command::new("tmux");
    command.args(["display-message", "-p"]);
    if let Some(pane) = env::var_os("TMUX_PANE") {
        command.arg("-t").arg(pane);
    }
    let output = command
        .arg("#{client_termname}")
        .output()
        .context("cannot query the tmux client terminal")?;
    if !output.status.success() {
        bail!("tmux client terminal query failed")
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn tmux_value(arguments: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(arguments)
        .output()
        .context("cannot query tmux graphics support")?;
    if !output.status.success() {
        bail!("tmux capability query failed")
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn supports_kitty_graphics(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("kitty") || value.contains("wezterm") || value.contains("ghostty")
}

fn push_apc(output: &mut Vec<Vec<u8>>, transport: Transport, control: &str, payload: &[u8]) {
    output.push(encode_apc(transport, control, payload));
}

fn encode_apc(transport: Transport, control: &str, payload: &[u8]) -> Vec<u8> {
    let mut command = Vec::with_capacity(control.len() + payload.len() + 6);
    command.extend_from_slice(b"\x1b_G");
    command.extend_from_slice(control.as_bytes());
    command.push(b';');
    command.extend_from_slice(payload);
    command.extend_from_slice(b"\x1b\\");

    match transport {
        Transport::Direct => command,
        Transport::Tmux => {
            let mut wrapped = Vec::with_capacity(command.len() + 16);
            wrapped.extend_from_slice(b"\x1bPtmux;");
            for byte in command {
                if byte == 0x1b {
                    wrapped.push(0x1b);
                }
                wrapped.push(byte);
            }
            wrapped.extend_from_slice(b"\x1b\\");
            wrapped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(segments: Vec<Vec<u8>>) -> Vec<u8> {
        segments.into_iter().flatten().collect()
    }

    #[test]
    fn recognizes_known_kitty_graphics_terminals() {
        assert!(supports_kitty_graphics("xterm-kitty"));
        assert!(supports_kitty_graphics("WezTerm"));
        assert!(supports_kitty_graphics("ghostty"));
        assert!(!supports_kitty_graphics("xterm-256color"));
    }

    #[test]
    fn wraps_apc_for_tmux_and_escapes_inner_escape_bytes() {
        let mut direct = Vec::new();
        push_apc(&mut direct, Transport::Direct, "a=d", &[]);
        assert_eq!(flatten(direct), b"\x1b_Ga=d;\x1b\\");

        let mut tmux = Vec::new();
        push_apc(&mut tmux, Transport::Tmux, "a=d", &[]);
        assert_eq!(flatten(tmux), b"\x1bPtmux;\x1b\x1b_Ga=d;\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn png_frames_are_chunked_and_replace_the_previous_image() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
        };
        let tile = RasterTile {
            image: RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])),
            index: 0,
            col: 4,
            row: 2,
            cols: 2,
            rows: 1,
        };
        let first =
            String::from_utf8(flatten(renderer.encode_tiles(&[tile], false).unwrap())).unwrap();
        assert!(first.contains("a=t,f=100,t=d,i=7,q=2,m=0"));
        assert!(first.contains("a=p,i=7,p=1,c=2,r=1,C=1,q=2"));
        assert!(!first.contains("a=d"));
        assert!(first.contains("\x1b[3;5H"));

        let replacement = RasterTile {
            image: RgbImage::from_pixel(2, 2, image::Rgb([4, 5, 6])),
            index: 0,
            col: 4,
            row: 2,
            cols: 2,
            rows: 1,
        };
        let second = String::from_utf8(flatten(
            renderer.encode_tiles(&[replacement], false).unwrap(),
        ))
        .unwrap();
        let transmit = second.find("a=t,f=100,t=d,i=8").unwrap();
        let placement = second.find("a=p,i=8,p=1,c=2,r=1,C=1,q=2").unwrap();
        let delete = second.find("a=d,d=I,i=7,q=2").unwrap();
        assert!(transmit < placement && placement < delete);
        assert!(!second.contains("a=f"));
    }

    #[test]
    fn tmux_frames_use_virtual_placements_and_unicode_placeholders() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 0x12_3456,
            active_tiles: BTreeMap::new(),
        };
        let tile = RasterTile {
            image: RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])),
            index: 0,
            col: 3,
            row: 4,
            cols: 2,
            rows: 2,
        };
        let encoded =
            String::from_utf8(flatten(renderer.encode_tiles(&[tile], false).unwrap())).unwrap();
        assert!(encoded.contains("U=1"));
        assert!(encoded.contains("a=p,i=1193046,p=1,c=2,r=2,C=1,q=2,U=1"));
        assert!(encoded.contains("\x1b[5;4H\x1b[38;2;18;52;86m"));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            ROW_DIACRITICS[0], ROW_DIACRITICS[0]
        )));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            ROW_DIACRITICS[0], ROW_DIACRITICS[1]
        )));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            ROW_DIACRITICS[1], ROW_DIACRITICS[1]
        )));
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 4);
    }

    #[test]
    fn reset_replaces_the_grid_before_deleting_old_tiles() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
        };
        let tile = RasterTile {
            image: RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])),
            index: 3,
            col: 0,
            row: 0,
            cols: 1,
            rows: 1,
        };
        renderer.encode_tiles(&[tile], false).unwrap();

        let replacement = RasterTile {
            image: RgbImage::from_pixel(1, 1, image::Rgb([4, 5, 6])),
            index: 0,
            col: 0,
            row: 0,
            cols: 1,
            rows: 1,
        };
        let encoded = String::from_utf8(flatten(
            renderer.encode_tiles(&[replacement], true).unwrap(),
        ))
        .unwrap();
        let delete = encoded.find("a=d,d=I,i=13,q=2").unwrap();
        let transmit = encoded.find("a=t,f=100,t=d,i=7").unwrap();
        let placement = encoded.find("a=p,i=7,p=1,c=1,r=1,C=1,q=2").unwrap();
        assert!(transmit < delete);
        assert!(transmit < placement && placement < delete);
        assert_eq!(renderer.active_tiles, BTreeMap::from([(0, 7)]));
    }
}
