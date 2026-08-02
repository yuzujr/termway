use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::ValueEnum;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::render::{RasterTile, ViewportRect};

const PAYLOAD_CHUNK_SIZE: usize = 4096;
const TILE_PLACEMENT_ID: u32 = 1;
const ATLAS_PLACEMENT_ID: u32 = 2;
const ANCHOR_PLACEMENT_ID: u32 = 3;
const MAX_TILE_INDEX: u32 = 0x0ffe;
const ANCHOR_IMAGE_OFFSET: u32 = 0x1ffd;
const ATLAS_IMAGE_OFFSETS: [u32; 2] = [0x1ffe, 0x1fff];
const PLACEHOLDER: char = '\u{10eeee}';
const DIACRITIC_COUNT: usize = 297;
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
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

#[derive(Clone)]
pub struct Renderer {
    transport: Transport,
    image_id_base: u32,
    active_tiles: BTreeMap<u32, u32>,
    known_tile_images: BTreeSet<u32>,
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
                // Reserve thirteen low bits for tile/atlas ids and use the remaining namespace
                // bits for the process. All ids fit in the 24-bit placeholder foreground color.
                image_id_base: image_id_base(std::process::id()),
                active_tiles: BTreeMap::new(),
                known_tile_images: BTreeSet::new(),
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

fn image_id_base(pid: u32) -> u32 {
    // Namespace zero would make the first tile use the reserved invalid image id zero.
    ((pid % 2047) + 1) << 13
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
        let anchor_ready = previous_id.is_some();
        let mut output = Vec::new();
        if self.transport == Transport::Tmux && !anchor_ready {
            push_upload_transaction(
                &mut output,
                self.transport,
                self.image_id_base + ANCHOR_IMAGE_OFFSET,
                &RgbImage::from_pixel(1, 1, image::Rgb([0, 0, 0])),
            )?;
        }
        push_upload_transaction(&mut output, self.transport, image_id, image)?;
        let mut transition = Vec::new();
        if self.transport == Transport::Tmux {
            push_anchor_placement(
                &mut transition,
                self.transport,
                self.image_id_base + ANCHOR_IMAGE_OFFSET,
            );
        }
        push_atlas_placement(
            &mut transition,
            self.transport,
            image_id,
            self.anchor_image_id(),
            crop,
            cols,
            rows,
        );
        self.delete_active_tiles(&mut transition);
        if let Some(previous_id) = previous_id {
            push_delete_image(&mut transition, self.transport, previous_id);
        }
        push_synchronized_transaction(&mut output, self.transport, transition);
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
        let mut transition = Vec::new();
        push_atlas_placement(
            &mut transition,
            self.transport,
            image_id,
            self.anchor_image_id(),
            crop,
            cols,
            rows,
        );
        // Refined tiles belong to the old viewport. Kitty/tmux must not render the atlas
        // placeholder rewrite or tile reclamation separately: doing so briefly exposes the old
        // full-view atlas (or a mixture of old tiles and the new crop) on every zoom step.
        self.delete_active_tiles(&mut transition);
        let mut output = Vec::new();
        push_synchronized_transaction(&mut output, self.transport, transition);
        Ok(output)
    }

    pub fn encode_tiles(&mut self, tiles: &[RasterTile], reset: bool) -> Result<Vec<Vec<u8>>> {
        if self.transport == Transport::Tmux && self.active_atlas.is_none() {
            bail!("cannot place Kitty tiles in tmux before the relative-placement anchor");
        }
        let mut output = Vec::new();
        let mut stale_tiles = reset.then(|| std::mem::take(&mut self.active_tiles));
        for tile in tiles {
            let mut transaction = Vec::new();
            if tile.index > MAX_TILE_INDEX {
                bail!("Kitty tile index {} exceeds {MAX_TILE_INDEX}", tile.index);
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
            push_upload(&mut transaction, self.transport, image_id, &tile.image)?;
            push_placement(
                &mut transaction,
                self.transport,
                image_id,
                self.anchor_image_id(),
                tile,
                previous_id,
            );
            // Never expose the background between versions. The new image/placeholder is fully
            // installed before the previous image and its placement are reclaimed.
            if self.transport == Transport::Tmux
                && let Some(previous_id) = previous_id
            {
                push_delete_image(&mut transaction, self.transport, previous_id);
            }
            // Kitty requires every chunk of an image upload to finish before another graphics
            // command. Treat one tile upload + placement as an indivisible queue unit so a newer
            // navigation preview can safely discard later tiles without stranding m=1 uploads.
            output.push(transaction.into_iter().flatten().collect());
            self.active_tiles.insert(tile.index, image_id);
            self.known_tile_images.insert(image_id);
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
        let had_atlas = self.active_atlas.take().is_some();
        if had_atlas {
            for offset in ATLAS_IMAGE_OFFSETS {
                push_delete_image(output, self.transport, self.image_id_base + offset);
            }
        }
        if had_atlas && self.transport == Transport::Tmux {
            push_delete_image(
                output,
                self.transport,
                self.image_id_base + ANCHOR_IMAGE_OFFSET,
            );
        }
        self.atlas_dimensions = None;
    }

    fn delete_active_tiles(&mut self, output: &mut Vec<Vec<u8>>) {
        self.active_tiles.clear();
        for image_id in std::mem::take(&mut self.known_tile_images) {
            push_delete_image(output, self.transport, image_id);
        }
    }

    fn anchor_image_id(&self) -> Option<u32> {
        (self.transport == Transport::Tmux).then_some(self.image_id_base + ANCHOR_IMAGE_OFFSET)
    }
}

/// Developer-only visual fixture used by `scripts/visual-regression.sh`.
///
/// It first displays a solid magenta refined tile over a four-quadrant navigation atlas. Pressing
/// `+` switches to the solid red top-left atlas crop. A correct atomic transition can only show
/// magenta or red, never the complete four-quadrant atlas, a blank frame, or a partially updated
/// grid.
pub fn run_visual_fixture(segment_delay: Duration) -> Result<()> {
    let mut terminal = FixtureTerminal::enter()?;
    let (cols, rows) = crossterm::terminal::size()?;
    if cols == 0 || rows == 0 {
        bail!("visual fixture needs a non-empty terminal");
    }

    let mut renderer = match Selection::select(GraphicsMode::Kitty)? {
        Selection::Kitty(renderer) => renderer,
        Selection::Ansi { reason } => bail!(
            "visual fixture requires Kitty graphics{}",
            reason.map_or_else(String::new, |reason| format!(": {reason}"))
        ),
    };
    let atlas = RgbImage::from_fn(400, 240, |x, y| match (x < 200, y < 120) {
        (true, true) => image::Rgb([224, 32, 32]),
        (false, true) => image::Rgb([32, 224, 32]),
        (true, false) => image::Rgb([32, 32, 224]),
        (false, false) => image::Rgb([224, 224, 32]),
    });
    terminal.write_segments(
        renderer.encode_atlas(
            &atlas,
            ViewportRect {
                x: 0,
                y: 0,
                width: atlas.width(),
                height: atlas.height(),
            },
            cols,
            rows,
        )?,
        segment_delay,
    )?;
    terminal.write_segments(
        renderer.encode_tiles(
            &[RasterTile {
                image: RgbImage::from_pixel(400, 240, image::Rgb([224, 32, 224])),
                index: 0,
                col: 0,
                row: 0,
                cols,
                rows,
            }],
            false,
        )?,
        segment_delay,
    )?;

    loop {
        match event::read().context("cannot read visual fixture input")? {
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && matches!(key.code, KeyCode::Char('+') | KeyCode::Char('=')) =>
            {
                terminal.write_segments(
                    renderer.encode_atlas_placement(
                        ViewportRect {
                            x: 0,
                            y: 0,
                            width: atlas.width() / 2,
                            height: atlas.height() / 2,
                        },
                        cols,
                        rows,
                    )?,
                    segment_delay,
                )?;
            }
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) =>
            {
                terminal.write_segments(renderer.encode_delete(), Duration::ZERO)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

struct FixtureTerminal {
    stdout: io::Stdout,
}

impl FixtureTerminal {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("cannot enable raw mode for visual fixture")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            Hide,
            crossterm::terminal::DisableLineWrap,
            Clear(ClearType::All),
        ) {
            let _ = disable_raw_mode();
            return Err(error).context("cannot initialize visual fixture terminal");
        }
        Ok(Self { stdout })
    }

    fn write_segments(&mut self, segments: Vec<Vec<u8>>, delay: Duration) -> Result<()> {
        for segment in segments {
            self.stdout.write_all(&segment)?;
            self.stdout.flush()?;
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        Ok(())
    }
}

impl Drop for FixtureTerminal {
    fn drop(&mut self) {
        let _ = execute!(
            self.stdout,
            crossterm::terminal::EnableLineWrap,
            Show,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

fn validate_placement(
    _transport: Transport,
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
    Ok(())
}

fn push_delete_image(output: &mut Vec<Vec<u8>>, transport: Transport, image_id: u32) {
    push_apc(output, transport, &format!("a=d,d=I,i={image_id},q=2"), &[]);
}

fn push_synchronized_transaction(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    commands: Vec<Vec<u8>>,
) {
    let command_bytes = commands.iter().map(Vec::len).sum::<usize>();
    let mut transaction = Vec::with_capacity(command_bytes + 64);
    transaction.extend_from_slice(&encode_passthrough(transport, BEGIN_SYNCHRONIZED_UPDATE));
    transaction.extend(commands.into_iter().flatten());
    transaction.extend_from_slice(&encode_passthrough(transport, END_SYNCHRONIZED_UPDATE));
    output.push(transaction);
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

fn push_upload_transaction(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    image: &RgbImage,
) -> Result<()> {
    let mut chunks = Vec::new();
    push_upload(&mut chunks, transport, image_id, image)?;
    output.push(chunks.into_iter().flatten().collect());
    Ok(())
}

fn push_atlas_placement(
    output: &mut Vec<Vec<u8>>,
    transport: Transport,
    image_id: u32,
    anchor_image_id: Option<u32>,
    crop: ViewportRect,
    cols: u16,
    rows: u16,
) {
    let relative = anchor_image_id.map_or_else(String::new, |anchor_image_id| {
        format!(",P={anchor_image_id},Q={ANCHOR_PLACEMENT_ID},H=0,V=0")
    });
    let control = format!(
        "a=p,i={image_id},p={ATLAS_PLACEMENT_ID},x={},y={},w={},h={},c={cols},r={rows},C=1,q=2,z=-1{relative}",
        crop.x, crop.y, crop.width, crop.height,
    );
    if transport == Transport::Tmux {
        push_apc(output, transport, &control, &[]);
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
    anchor_image_id: Option<u32>,
    tile: &RasterTile,
    previous_id: Option<u32>,
) {
    let relative = anchor_image_id.map_or_else(String::new, |anchor_image_id| {
        format!(
            ",P={anchor_image_id},Q={ANCHOR_PLACEMENT_ID},H={},V={}",
            tile.col, tile.row
        )
    });
    let control = format!(
        "a=p,i={image_id},p={TILE_PLACEMENT_ID},c={},r={},C=1,q=2,z=0{relative}",
        tile.cols, tile.rows,
    );
    if transport == Transport::Tmux {
        push_apc(output, transport, &control, &[]);
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

fn push_anchor_placement(output: &mut Vec<Vec<u8>>, transport: Transport, image_id: u32) {
    debug_assert_eq!(transport, Transport::Tmux);
    push_apc(
        output,
        transport,
        &format!("a=p,i={image_id},p={ANCHOR_PLACEMENT_ID},c=1,r=1,C=1,q=2,z=-2,U=1"),
        &[],
    );
    push_placeholders(output, image_id, ANCHOR_PLACEMENT_ID, 0, 0, 1, 1);
}

fn push_placeholders(
    output: &mut Vec<Vec<u8>>,
    image_id: u32,
    placement_id: u32,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
) {
    use std::io::Write as _;

    let red = (image_id >> 16) & 0xff;
    let green = (image_id >> 8) & 0xff;
    let blue = image_id & 0xff;
    let placement_red = (placement_id >> 16) & 0xff;
    let placement_green = (placement_id >> 8) & 0xff;
    let placement_blue = placement_id & 0xff;
    for relative_row in 0..rows {
        let mut line = Vec::new();
        write!(
            line,
            "\x1b[{};{}H\x1b[38;2;{red};{green};{blue};58;2;{placement_red};{placement_green};{placement_blue}m",
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
        if !supports_relative_placements(&terminal) {
            bail!(
                "tmux client terminal {terminal:?} does not advertise Kitty relative placements; use --graphics ansi"
            );
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

fn supports_relative_placements(value: &str) -> bool {
    value.to_ascii_lowercase().contains("kitty")
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

fn encode_passthrough(transport: Transport, command: &[u8]) -> Vec<u8> {
    match transport {
        Transport::Direct => command.to_vec(),
        Transport::Tmux => {
            let mut wrapped = Vec::with_capacity(command.len() + 16);
            wrapped.extend_from_slice(b"\x1bPtmux;");
            for &byte in command {
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
        assert!(supports_relative_placements("xterm-kitty"));
        assert!(!supports_relative_placements("WezTerm"));
    }

    #[test]
    fn process_image_namespace_fits_all_reserved_ids_in_24_bits() {
        for pid in [0, 1, 2046, 2047, u32::MAX] {
            let base = image_id_base(pid);
            assert!(base > 0);
            assert!(base + ATLAS_IMAGE_OFFSETS[1] <= 0x00ff_ffff);
        }
        assert_ne!(image_id_base(100), image_id_base(101));
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
            known_tile_images: BTreeSet::new(),
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
    fn each_tiled_upload_is_one_cancellable_protocol_transaction() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        let image = RgbImage::from_fn(128, 128, |x, y| {
            let value = x.wrapping_mul(1_664_525) ^ y.wrapping_mul(1_013_904_223);
            image::Rgb([value as u8, (value >> 8) as u8, (value >> 16) as u8])
        });
        let segments = renderer
            .encode_tiles(
                &[RasterTile {
                    image,
                    index: 0,
                    col: 0,
                    row: 0,
                    cols: 16,
                    rows: 8,
                }],
                false,
            )
            .unwrap();
        assert_eq!(segments.len(), 1);
        let encoded = String::from_utf8_lossy(&segments[0]);
        assert!(encoded.matches("m=1").count() > 1);
        assert!(encoded.ends_with("\x1b\\"));
        assert!(encoded.contains("a=p,i=7"));
    }

    #[test]
    fn atlas_upload_chunks_form_one_non_cancellable_protocol_transaction() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        let image = RgbImage::from_fn(256, 256, |x, y| {
            let value = x.wrapping_mul(1_664_525) ^ y.wrapping_mul(1_013_904_223);
            image::Rgb([value as u8, (value >> 8) as u8, (value >> 16) as u8])
        });
        let segments = renderer
            .encode_atlas(
                &image,
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 256,
                    height: 256,
                },
                32,
                16,
            )
            .unwrap();
        assert_eq!(segments.len(), 2);
        let upload = String::from_utf8_lossy(&segments[0]);
        assert!(upload.matches("m=1").count() > 1);
        assert!(upload.contains("m=0"));
        assert!(!upload.contains("a=p"));
        assert!(segments[1].starts_with(BEGIN_SYNCHRONIZED_UPDATE));
    }

    #[test]
    fn tmux_tiles_are_relative_to_one_unicode_placeholder_anchor() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 0x12_3456,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
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
        renderer
            .encode_atlas(
                &RgbImage::new(4, 4),
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                4,
                4,
            )
            .unwrap();
        let encoded =
            String::from_utf8(flatten(renderer.encode_tiles(&[tile], false).unwrap())).unwrap();
        assert!(!encoded.contains("U=1"));
        assert!(encoded.contains(&format!(
            "a=p,i=1193046,p=1,c=2,r=2,C=1,q=2,z=0,P={},Q=3,H=3,V=4",
            0x12_3456 + ANCHOR_IMAGE_OFFSET
        )));
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 0);
    }

    #[test]
    fn reset_replaces_the_grid_before_deleting_old_tiles() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
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
            known_tile_images: BTreeSet::new(),
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
        assert!(preview.len() < 256, "{} preview bytes", preview.len());
    }

    #[test]
    fn direct_atlas_preview_atomically_replaces_refined_tiles() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        renderer
            .encode_atlas(
                &RgbImage::new(4, 4),
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                2,
                2,
            )
            .unwrap();
        renderer
            .encode_tiles(
                &[RasterTile {
                    image: RgbImage::new(4, 4),
                    index: 0,
                    col: 0,
                    row: 0,
                    cols: 2,
                    rows: 2,
                }],
                false,
            )
            .unwrap();

        let segments = renderer
            .encode_atlas_placement(
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                2,
                2,
            )
            .unwrap();
        assert_eq!(segments.len(), 1);
        let transition = &segments[0];
        assert!(transition.starts_with(BEGIN_SYNCHRONIZED_UPDATE));
        assert!(transition.ends_with(END_SYNCHRONIZED_UPDATE));
        let encoded = String::from_utf8_lossy(transition);
        let placement = encoded.find("x=0,y=0,w=2,h=2").unwrap();
        let deletion = encoded.find("a=d,d=I,i=7").unwrap();
        assert!(placement < deletion);
    }

    #[test]
    fn tmux_atlas_preview_passes_one_synchronized_transaction_to_kitty() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        renderer
            .encode_atlas(
                &RgbImage::new(4, 4),
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                2,
                2,
            )
            .unwrap();

        let segments = renderer
            .encode_atlas_placement(
                ViewportRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                2,
                2,
            )
            .unwrap();
        assert_eq!(segments.len(), 1);
        let begin = encode_passthrough(Transport::Tmux, BEGIN_SYNCHRONIZED_UPDATE);
        let end = encode_passthrough(Transport::Tmux, END_SYNCHRONIZED_UPDATE);
        assert!(segments[0].starts_with(&begin));
        assert!(segments[0].ends_with(&end));
        let encoded = String::from_utf8_lossy(&segments[0]);
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 0);
        assert!(encoded.contains("x=0,y=0,w=2,h=2"));
        assert!(encoded.contains("P=8196,Q=3,H=0,V=0"));
    }

    #[test]
    fn tmux_atlas_uses_one_anchor_even_in_a_240_column_pane() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
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
        assert_eq!(encoded.matches(PLACEHOLDER).count(), 1);
        assert!(encoded.contains(";58;2;0;0;3m"));
        assert!(encoded.contains(&format!(
            "{PLACEHOLDER}{}{}",
            diacritic(0).unwrap(),
            diacritic(0).unwrap()
        )));
        assert!(encoded.contains("c=240,r=1,C=1,q=2,z=-1,P=8196,Q=3,H=0,V=0"));
    }

    #[test]
    fn tmux_atlas_rebuild_restores_the_screen_anchor_after_resize() {
        let mut renderer = Renderer {
            transport: Transport::Tmux,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        let viewport = ViewportRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        renderer
            .encode_atlas(&RgbImage::new(4, 4), viewport, 2, 2)
            .unwrap();
        let rebuilt = String::from_utf8(flatten(
            renderer
                .encode_atlas(&RgbImage::new(4, 4), viewport, 3, 3)
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(rebuilt.matches(PLACEHOLDER).count(), 1);
        assert!(rebuilt.contains("p=3,c=1,r=1,C=1,q=2,z=-2,U=1"));
        assert!(rebuilt.contains("c=3,r=3,C=1,q=2,z=-1,P=8196,Q=3"));
        assert!(!rebuilt.contains("a=t,f=100,t=d,i=8196"));
    }

    #[test]
    fn atlas_transition_reclaims_both_generations_of_replaced_tiles() {
        let mut renderer = Renderer {
            transport: Transport::Direct,
            image_id_base: 7,
            active_tiles: BTreeMap::new(),
            known_tile_images: BTreeSet::new(),
            active_atlas: None,
            atlas_dimensions: None,
        };
        for color in [image::Rgb([1, 2, 3]), image::Rgb([4, 5, 6])] {
            renderer
                .encode_tiles(
                    &[RasterTile {
                        image: RgbImage::from_pixel(1, 1, color),
                        index: 0,
                        col: 0,
                        row: 0,
                        cols: 1,
                        rows: 1,
                    }],
                    false,
                )
                .unwrap();
        }
        let encoded = String::from_utf8(flatten(
            renderer
                .encode_atlas(
                    &RgbImage::new(1, 1),
                    ViewportRect {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    },
                    1,
                    1,
                )
                .unwrap(),
        ))
        .unwrap();
        assert!(encoded.contains("a=d,d=I,i=7,q=2"));
        assert!(encoded.contains("a=d,d=I,i=8,q=2"));
        assert!(renderer.known_tile_images.is_empty());
    }
}
