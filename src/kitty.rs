use std::collections::BTreeMap;
use std::env;
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::ValueEnum;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::render::{RasterTile, ViewportRect};

const PAYLOAD_CHUNK_SIZE: usize = 4096;
const TILE_PLACEMENT_ID: u32 = 1;
const ATLAS_PLACEMENT_ID: u32 = 2;
const MAX_TILE_INDEX: u32 = 0x0ffe;
const ATLAS_IMAGE_OFFSETS: [u32; 2] = [0x1ffe, 0x1fff];
const PLACEHOLDER: char = '\u{10eeee}';
const DIACRITIC_COUNT: usize = 297;
// Compact form of Kitty's canonical rowcolumn-diacritics.txt. Keeping the complete table lets
// tmux placeholders address wide panes without relying on fragile left-neighbour inference.
const DIACRITIC_RANGES: &[(u32, u32)] = &[
    (0x0305, 0x0305),
    (0x030D, 0x030E),
    (0x0310, 0x0310),
    (0x0312, 0x0312),
    (0x033D, 0x033F),
    (0x0346, 0x0346),
    (0x034A, 0x034C),
    (0x0350, 0x0352),
    (0x0357, 0x0357),
    (0x035B, 0x035B),
    (0x0363, 0x036F),
    (0x0483, 0x0487),
    (0x0592, 0x0595),
    (0x0597, 0x0599),
    (0x059C, 0x05A1),
    (0x05A8, 0x05A9),
    (0x05AB, 0x05AC),
    (0x05AF, 0x05AF),
    (0x05C4, 0x05C4),
    (0x0610, 0x0617),
    (0x0657, 0x065B),
    (0x065D, 0x065E),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E2),
    (0x06E4, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EB, 0x06EC),
    (0x0730, 0x0730),
    (0x0732, 0x0733),
    (0x0735, 0x0736),
    (0x073A, 0x073A),
    (0x073D, 0x073D),
    (0x073F, 0x0741),
    (0x0743, 0x0743),
    (0x0745, 0x0745),
    (0x0747, 0x0747),
    (0x0749, 0x074A),
    (0x07EB, 0x07F1),
    (0x07F3, 0x07F3),
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    (0x0951, 0x0951),
    (0x0953, 0x0954),
    (0x0F82, 0x0F83),
    (0x0F86, 0x0F87),
    (0x135D, 0x135F),
    (0x17DD, 0x17DD),
    (0x193A, 0x193A),
    (0x1A17, 0x1A17),
    (0x1A75, 0x1A7C),
    (0x1B6B, 0x1B6B),
    (0x1B6D, 0x1B73),
    (0x1CD0, 0x1CD2),
    (0x1CDA, 0x1CDB),
    (0x1CE0, 0x1CE0),
    (0x1DC0, 0x1DC1),
    (0x1DC3, 0x1DC9),
    (0x1DCB, 0x1DCC),
    (0x1DD1, 0x1DE6),
    (0x1DFE, 0x1DFE),
    (0x20D0, 0x20D1),
    (0x20D4, 0x20D7),
    (0x20DB, 0x20DC),
    (0x20E1, 0x20E1),
    (0x20E7, 0x20E7),
    (0x20E9, 0x20E9),
    (0x20F0, 0x20F0),
    (0x2CEF, 0x2CF1),
    (0x2DE0, 0x2DFF),
    (0xA66F, 0xA66F),
    (0xA67C, 0xA67D),
    (0xA6F0, 0xA6F1),
    (0xA8E0, 0xA8F1),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB3),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xFE20, 0xFE26),
    (0x10A0F, 0x10A0F),
    (0x10A38, 0x10A38),
    (0x1D185, 0x1D189),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
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
    active_atlas: Option<u32>,
    atlas_dimensions: Option<(u32, u32)>,
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
                active_atlas: None,
                atlas_dimensions: None,
            })),
            Err(error) if mode == GraphicsMode::Auto => Ok(Self::Ansi {
                reason: Some(error.to_string()),
            }),
            Err(error) => Err(error),
        }
    }
}

impl Renderer {
    pub fn is_tmux(&self) -> bool {
        self.transport == Transport::Tmux
    }

    pub fn has_atlas(&self) -> bool {
        self.active_atlas.is_some()
    }

    /// Upload a full-screen navigation atlas and display a crop from it. The atlas remains in
    /// Kitty's image cache so later zoom/pan previews only need a placement command.
    pub fn encode_atlas(
        &mut self,
        image: &RgbImage,
        crop: ViewportRect,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<Vec<u8>>> {
        validate_placement(self.transport, image.dimensions(), crop, cols, rows)?;
        let first_id = self.image_id_base + ATLAS_IMAGE_OFFSETS[0];
        let image_id = if self.active_atlas == Some(first_id) {
            self.image_id_base + ATLAS_IMAGE_OFFSETS[1]
        } else {
            first_id
        };
        let previous_id = self.active_atlas;
        let mut output = Vec::new();
        push_upload(&mut output, self.transport, image_id, image)?;
        push_atlas_placement(&mut output, self.transport, image_id, crop, cols, rows);
        self.delete_active_tiles(&mut output);
        if let Some(previous_id) = previous_id {
            push_delete_image(&mut output, self.transport, previous_id);
        }
        self.active_atlas = Some(image_id);
        self.atlas_dimensions = Some(image.dimensions());
        Ok(output)
    }

    /// Re-crop the navigation atlas already cached by the terminal. No pixel payload is sent.
    pub fn encode_atlas_placement(
        &mut self,
        crop: ViewportRect,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<Vec<u8>>> {
        let image_id = self
            .active_atlas
            .context("cannot place Kitty navigation atlas before it is uploaded")?;
        let dimensions = self
            .atlas_dimensions
            .context("Kitty navigation atlas dimensions are missing")?;
        validate_placement(self.transport, dimensions, crop, cols, rows)?;
        let mut output = Vec::new();
        push_atlas_placement(&mut output, self.transport, image_id, crop, cols, rows);
        // Refined tiles belong to the old viewport. Replacing the base first prevents a blank
        // frame while their terminal-side images are reclaimed.
        self.delete_active_tiles(&mut output);
        Ok(output)
    }

    pub fn encode_tiles(&mut self, tiles: &[RasterTile], reset: bool) -> Result<Vec<Vec<u8>>> {
        let mut output = Vec::new();
        let mut stale_tiles = reset.then(|| std::mem::take(&mut self.active_tiles));
        for tile in tiles {
            if tile.index > MAX_TILE_INDEX {
                bail!("Kitty tile index {} exceeds {MAX_TILE_INDEX}", tile.index);
            }
            if self.transport == Transport::Tmux {
                if usize::from(tile.rows) > DIACRITIC_COUNT {
                    bail!(
                        "Kitty placeholder supports at most {} rows per tile, got {}",
                        DIACRITIC_COUNT,
                        tile.rows
                    );
                }
                if usize::from(tile.cols) > DIACRITIC_COUNT {
                    bail!(
                        "Kitty placeholder supports at most {} columns per tile, got {}",
                        DIACRITIC_COUNT,
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
            push_upload(&mut output, self.transport, image_id, &tile.image)?;
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
        self.delete_active_tiles(output);
        if let Some(image_id) = self.active_atlas.take() {
            push_delete_image(output, self.transport, image_id);
        }
        self.atlas_dimensions = None;
    }

    fn delete_active_tiles(&mut self, output: &mut Vec<Vec<u8>>) {
        for image_id in std::mem::take(&mut self.active_tiles).into_values() {
            push_delete_image(output, self.transport, image_id);
        }
    }
}

fn validate_placement(
    transport: Transport,
    dimensions: (u32, u32),
    crop: ViewportRect,
    cols: u16,
    rows: u16,
) -> Result<()> {
    if crop.width == 0
        || crop.height == 0
        || crop.x.saturating_add(crop.width) > dimensions.0
        || crop.y.saturating_add(crop.height) > dimensions.1
    {
        bail!("Kitty source crop {crop:?} is outside image {dimensions:?}");
    }
    if cols == 0 || rows == 0 {
        bail!("Kitty placement must occupy at least one cell");
    }
    if transport == Transport::Tmux
        && (usize::from(cols) > DIACRITIC_COUNT || usize::from(rows) > DIACRITIC_COUNT)
    {
        bail!(
            "Kitty Unicode placeholder supports at most {DIACRITIC_COUNT} rows and columns, got {cols}x{rows}"
        );
    }
    Ok(())
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

fn push_upload(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    image: &RgbImage,
) -> Result<()> {
    let payload = encode_png(image)?;
    for (index, chunk) in payload.as_bytes().chunks(PAYLOAD_CHUNK_SIZE).enumerate() {
        let more = usize::from((index + 1) * PAYLOAD_CHUNK_SIZE < payload.len());
        let control = if index == 0 {
            format!("a=t,f=100,t=d,i={image_id},q=2,m={more}")
        } else {
            format!("m={more},q=2")
        };
        push_apc(output, transport, &control, chunk);
    }
    Ok(())
}

fn push_atlas_placement(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    crop: ViewportRect,
    cols: u16,
    rows: u16,
) {
    let control = format!(
        "a=p,i={image_id},p={ATLAS_PLACEMENT_ID},x={},y={},w={},h={},c={cols},r={rows},C=1,q=2,z=-1{}",
        crop.x,
        crop.y,
        crop.width,
        crop.height,
        if transport == Transport::Tmux {
            ",U=1"
        } else {
            ""
        },
    );
    if transport == Transport::Tmux {
        push_apc(output, transport, &control, &[]);
        push_placeholders(output, image_id, 0, 0, cols, rows);
    } else {
        let mut command = b"\x1b[1;1H".to_vec();
        command.extend_from_slice(&encode_apc(Transport::Direct, &control, &[]));
        output.push(command);
    }
}

fn push_placement(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    tile: &RasterTile,
    previous_id: Option<u32>,
) {
    let control = format!(
        "a=p,i={image_id},p={TILE_PLACEMENT_ID},c={},r={},C=1,q=2,z=0{}",
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
                diacritic(usize::from(relative_row)).expect("placeholder row was validated"),
                diacritic(usize::from(relative_col)).expect("placeholder column was validated"),
            )
            .expect("writing to Vec cannot fail");
        }
        line.extend_from_slice(b"\x1b[0m");
        output.push(line);
    }
}

fn diacritic(mut index: usize) -> Option<char> {
    if index >= DIACRITIC_COUNT {
        return None;
    }
    for &(start, end) in DIACRITIC_RANGES {
        let length = (end - start + 1) as usize;
        if index < length {
            return char::from_u32(start + index as u32);
        }
        index -= length;
    }
    None
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
            active_atlas: None,
            atlas_dimensions: None,
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
        assert!(first.contains("a=p,i=7,p=1,c=2,r=1,C=1,q=2,z=0"));
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
        let placement = second.find("a=p,i=8,p=1,c=2,r=1,C=1,q=2,z=0").unwrap();
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
            active_atlas: None,
            atlas_dimensions: None,
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
        assert!(encoded.contains("a=p,i=1193046,p=1,c=2,r=2,C=1,q=2,z=0,U=1"));
        assert!(encoded.contains("\x1b[5;4H\x1b[38;2;18;52;86m"));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            diacritic(0).unwrap(),
            diacritic(0).unwrap()
        )));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            diacritic(0).unwrap(),
            diacritic(1).unwrap()
        )));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            diacritic(1).unwrap(),
            diacritic(1).unwrap()
        )));
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 4);
    }

    #[test]
    fn reset_replaces_the_grid_before_deleting_old_tiles() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            active_atlas: None,
            atlas_dimensions: None,
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
        let placement = encoded.find("a=p,i=7,p=1,c=1,r=1,C=1,q=2,z=0").unwrap();
        assert!(transmit < delete);
        assert!(transmit < placement && placement < delete);
        assert_eq!(renderer.active_tiles, BTreeMap::from([(0, 7)]));
    }

    #[test]
    fn complete_kitty_diacritic_table_supports_wide_panes() {
        let expanded = DIACRITIC_RANGES
            .iter()
            .map(|(start, end)| end - start + 1)
            .sum::<u32>() as usize;
        assert_eq!(expanded, DIACRITIC_COUNT);
        assert_eq!(diacritic(0), Some('\u{0305}'));
        assert!(diacritic(239).is_some());
        assert!(diacritic(DIACRITIC_COUNT).is_none());
    }

    #[test]
    fn atlas_reuses_terminal_pixels_for_later_crops() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        let image = RgbImage::from_pixel(400, 240, image::Rgb([1, 2, 3]));
        let first = String::from_utf8(flatten(
            renderer
                .encode_atlas(
                    &image,
                    ViewportRect {
                        x: 0,
                        y: 0,
                        width: 400,
                        height: 240,
                    },
                    80,
                    24,
                )
                .unwrap(),
        ))
        .unwrap();
        assert!(first.contains("a=t,f=100,t=d,i=8197"));
        assert!(first.contains("a=p,i=8197,p=2,x=0,y=0,w=400,h=240,c=80,r=24,C=1,q=2,z=-1"));

        let preview = String::from_utf8(flatten(
            renderer
                .encode_atlas_placement(
                    ViewportRect {
                        x: 100,
                        y: 60,
                        width: 200,
                        height: 120,
                    },
                    80,
                    24,
                )
                .unwrap(),
        ))
        .unwrap();
        assert!(!preview.contains("a=t"));
        assert!(!preview.contains("iVBOR"));
        assert!(preview.contains("x=100,y=60,w=200,h=120"));
    }

    #[test]
    fn tmux_atlas_placeholders_support_a_240_column_pane() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        let image = RgbImage::new(240, 1);
        let encoded = renderer
            .encode_atlas(
                &image,
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 240,
                    height: 1,
                },
                240,
                1,
            )
            .unwrap();
        let encoded = String::from_utf8(flatten(encoded)).unwrap();
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 240);
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            diacritic(0).unwrap(),
            diacritic(239).unwrap()
        )));
    }
}
