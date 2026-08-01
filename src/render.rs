use anyhow::{Result, bail};
use image::imageops::FilterType;
use image::{Rgb, RgbImage};

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub zoom: f32,
    pub center_x: f32,
    pub center_y: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub struct RenderedFrame {
    pub bytes: Vec<u8>,
    pub cells: Vec<Cell>,
    pub cols: u16,
    pub rows: u16,
    pub sample_height: u16,
    pub viewport: ViewportRect,
}

pub struct RasterFrame {
    pub image: RgbImage,
    pub viewport: ViewportRect,
}

pub struct RasterTile {
    pub image: RgbImage,
    pub index: u32,
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    foreground: [u8; 3],
    background: [u8; 3],
}

#[cfg(test)]
pub fn render_half_blocks(source: &RgbImage, max_cols: u16, max_rows: u16) -> RenderedFrame {
    render_half_blocks_viewport(source, max_cols, max_rows, Viewport::default())
        .expect("the default viewport is valid")
}

pub fn render_half_blocks_viewport(
    source: &RgbImage,
    max_cols: u16,
    max_rows: u16,
    viewport: Viewport,
) -> Result<RenderedFrame> {
    validate_viewport(viewport)?;
    let viewport = viewport_rect(source.width(), source.height(), viewport);
    let cropped = image::imageops::crop_imm(
        source,
        viewport.x,
        viewport.y,
        viewport.width,
        viewport.height,
    )
    .to_image();
    let (sample_width, sample_height) = fitted_sample_size(
        viewport.width,
        viewport.height,
        max_cols.into(),
        u32::from(max_rows) * 2,
    );
    let resized =
        image::imageops::resize(&cropped, sample_width, sample_height, FilterType::Triangle);
    let rows = sample_height.div_ceil(2);
    let mut bytes = Vec::with_capacity(sample_width as usize * rows as usize * 36);
    let mut cells = Vec::with_capacity(sample_width as usize * rows as usize);

    for y in (0..sample_height).step_by(2) {
        let mut colors = TerminalColors::default();
        for x in 0..sample_width {
            let top = resized.get_pixel(x, y);
            let bottom = if y + 1 < sample_height {
                resized.get_pixel(x, y + 1)
            } else {
                &Rgb([0, 0, 0])
            };
            let cell = Cell {
                foreground: top.0,
                background: bottom.0,
            };
            push_cell(&mut bytes, cell, &mut colors);
            cells.push(cell);
        }
        bytes.extend_from_slice(b"\x1b[0m\x1b[K\r\n");
    }
    bytes.extend_from_slice(b"\x1b[0m");

    Ok(RenderedFrame {
        bytes,
        cells,
        cols: sample_width as u16,
        rows: rows as u16,
        sample_height: sample_height as u16,
        viewport,
    })
}

pub fn render_raster_viewport(
    source: &RgbImage,
    max_width: u32,
    max_height: u32,
    viewport: Viewport,
) -> Result<RasterFrame> {
    validate_viewport(viewport)?;
    let viewport = viewport_rect(source.width(), source.height(), viewport);
    let cropped = image::imageops::crop_imm(
        source,
        viewport.x,
        viewport.y,
        viewport.width,
        viewport.height,
    )
    .to_image();
    let (fitted_width, fitted_height) = fitted_sample_size(
        viewport.width,
        viewport.height,
        max_width.max(1),
        max_height.max(1),
    );
    let scale = (fitted_width as f64 / viewport.width as f64)
        .min(fitted_height as f64 / viewport.height as f64)
        .min(1.0);
    let width = (viewport.width as f64 * scale).round().max(1.0) as u32;
    let height = (viewport.height as f64 * scale).round().max(1.0) as u32;
    let image = if width == viewport.width && height == viewport.height {
        cropped
    } else {
        image::imageops::resize(&cropped, width, height, FilterType::Triangle)
    };
    Ok(RasterFrame { image, viewport })
}

pub fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    fitted_sample_size(width, height, max_width.max(1), max_height.max(1))
}

pub fn align_raster_to_cell_grid(
    image: &RgbImage,
    cols: u16,
    rows: u16,
    cell_width: u32,
    cell_height: u32,
    max_width: u32,
    max_height: u32,
) -> RgbImage {
    assert!(cols > 0 && rows > 0);
    assert!(cell_width > 0 && cell_height > 0);
    let divisor = gcd(cell_width, cell_height);
    let base_width = u32::from(cols) * (cell_width / divisor);
    let base_height = u32::from(rows) * (cell_height / divisor);
    let maximum_scale = (max_width / base_width)
        .min(max_height / base_height)
        .max(1);
    let source_scale = ((image.width() as f64 / base_width as f64)
        .min(image.height() as f64 / base_height as f64)
        .round() as u32)
        .max(1);
    let scale = source_scale.min(maximum_scale);
    let width = base_width * scale;
    let height = base_height * scale;
    if image.dimensions() == (width, height) {
        image.clone()
    } else {
        image::imageops::resize(image, width, height, FilterType::Triangle)
    }
}

/// Reduce insignificant low channel bits before lossless PNG compression. At seven bits the
/// maximum error is one sRGB code value, while desktop screenshots compress materially better.
pub fn reduce_color_precision(image: &mut RgbImage, bits: u8) {
    assert!((1..=8).contains(&bits));
    let mask = u8::MAX << (8 - bits);
    for channel in image.as_mut() {
        *channel &= mask;
    }
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

pub fn changed_raster_tiles(
    previous: Option<&RgbImage>,
    current: &RgbImage,
    display_cols: u16,
    display_rows: u16,
    target_tile_size: u32,
) -> Vec<RasterTile> {
    if let Some(previous) = previous {
        assert_eq!(previous.dimensions(), current.dimensions());
    }
    assert!(display_cols > 0 && display_rows > 0);
    assert!(target_tile_size > 0);
    let (width, height) = current.dimensions();
    let tile_cell_cols = cells_for_target_pixels(target_tile_size, width, display_cols);
    let tile_cell_rows = cells_for_target_pixels(target_tile_size, height, display_rows);
    let grid_cols = u32::from(display_cols).div_ceil(u32::from(tile_cell_cols));
    let grid_rows = u32::from(display_rows).div_ceil(u32::from(tile_cell_rows));
    let mut tiles = Vec::new();
    for tile_row in 0..grid_rows {
        let row = tile_row * u32::from(tile_cell_rows);
        let rows = u32::from(tile_cell_rows).min(u32::from(display_rows) - row);
        let y = pixel_boundary(row, height, display_rows);
        let bottom = pixel_boundary(row + rows, height, display_rows);
        for tile_col in 0..grid_cols {
            let col = tile_col * u32::from(tile_cell_cols);
            let cols = u32::from(tile_cell_cols).min(u32::from(display_cols) - col);
            let x = pixel_boundary(col, width, display_cols);
            let right = pixel_boundary(col + cols, width, display_cols);
            let tile_width = right - x;
            let tile_height = bottom - y;
            if previous.is_some_and(|previous| {
                !region_changed(previous, current, x, y, tile_width, tile_height)
            }) {
                continue;
            }
            tiles.push(RasterTile {
                image: image::imageops::crop_imm(current, x, y, tile_width, tile_height).to_image(),
                index: tile_row * grid_cols + tile_col,
                col: col as u16,
                row: row as u16,
                cols: cols as u16,
                rows: rows as u16,
            });
        }
    }
    tiles
}

fn cells_for_target_pixels(target: u32, pixels: u32, cells: u16) -> u16 {
    let cells = u32::from(cells);
    ((u64::from(target) * u64::from(cells)).div_ceil(u64::from(pixels)) as u32).clamp(1, cells)
        as u16
}

fn pixel_boundary(cell: u32, pixels: u32, cells: u16) -> u32 {
    (u64::from(cell) * u64::from(pixels) / u64::from(cells)) as u32
}

fn region_changed(
    previous: &RgbImage,
    current: &RgbImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> bool {
    let row_bytes = width as usize * 3;
    let image_row_bytes = previous.width() as usize * 3;
    let x = x as usize * 3;
    (y..y + height).any(|row| {
        let start = row as usize * image_row_bytes + x;
        previous.as_raw()[start..start + row_bytes] != current.as_raw()[start..start + row_bytes]
    })
}

pub fn validate_viewport(viewport: Viewport) -> Result<()> {
    if !viewport.zoom.is_finite() || viewport.zoom < 1.0 {
        bail!("zoom must be a finite number greater than or equal to 1.0");
    }
    if !viewport.center_x.is_finite() || !(0.0..=1.0).contains(&viewport.center_x) {
        bail!("center-x must be between 0.0 and 1.0");
    }
    if !viewport.center_y.is_finite() || !(0.0..=1.0).contains(&viewport.center_y) {
        bail!("center-y must be between 0.0 and 1.0");
    }
    Ok(())
}

pub fn viewport_rect(width: u32, height: u32, viewport: Viewport) -> ViewportRect {
    let crop_width = ((width as f32 / viewport.zoom).round() as u32).clamp(1, width);
    let crop_height = ((height as f32 / viewport.zoom).round() as u32).clamp(1, height);
    let center_x = viewport.center_x * width as f32;
    let center_y = viewport.center_y * height as f32;
    let x = (center_x - crop_width as f32 / 2.0)
        .round()
        .clamp(0.0, (width - crop_width) as f32) as u32;
    let y = (center_y - crop_height as f32 / 2.0)
        .round()
        .clamp(0.0, (height - crop_height) as f32) as u32;
    ViewportRect {
        x,
        y,
        width: crop_width,
        height: crop_height,
    }
}

/// Map a source-image viewport to the corresponding pixel crop in a downscaled raster. Outward
/// rounding guarantees that even a one-pixel source viewport remains visible.
pub fn map_viewport_to_raster(
    viewport: ViewportRect,
    source_width: u32,
    source_height: u32,
    raster_width: u32,
    raster_height: u32,
) -> ViewportRect {
    assert!(source_width > 0 && source_height > 0);
    assert!(raster_width > 0 && raster_height > 0);
    let x = u64::from(viewport.x) * u64::from(raster_width) / u64::from(source_width);
    let y = u64::from(viewport.y) * u64::from(raster_height) / u64::from(source_height);
    let right = (u64::from(viewport.x + viewport.width) * u64::from(raster_width))
        .div_ceil(u64::from(source_width));
    let bottom = (u64::from(viewport.y + viewport.height) * u64::from(raster_height))
        .div_ceil(u64::from(source_height));
    let x = (x as u32).min(raster_width - 1);
    let y = (y as u32).min(raster_height - 1);
    ViewportRect {
        x,
        y,
        width: (right as u32).min(raster_width).saturating_sub(x).max(1),
        height: (bottom as u32).min(raster_height).saturating_sub(y).max(1),
    }
}

fn fitted_sample_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    assert!(width > 0 && height > 0 && max_width > 0 && max_height > 0);
    let width_limited_height =
        (u64::from(height) * u64::from(max_width) / u64::from(width)).max(1) as u32;
    if width_limited_height <= max_height {
        (max_width, width_limited_height)
    } else {
        let height_limited_width =
            (u64::from(width) * u64::from(max_height) / u64::from(height)).max(1) as u32;
        (height_limited_width, max_height)
    }
}

#[derive(Default)]
struct TerminalColors {
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
}

fn push_cell(bytes: &mut Vec<u8>, cell: Cell, colors: &mut TerminalColors) {
    use std::io::Write as _;

    match (
        colors.foreground != Some(cell.foreground),
        colors.background != Some(cell.background),
    ) {
        (true, true) => write!(
            bytes,
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            cell.foreground[0],
            cell.foreground[1],
            cell.foreground[2],
            cell.background[0],
            cell.background[1],
            cell.background[2],
        ),
        (true, false) => write!(
            bytes,
            "\x1b[38;2;{};{};{}m",
            cell.foreground[0], cell.foreground[1], cell.foreground[2],
        ),
        (false, true) => write!(
            bytes,
            "\x1b[48;2;{};{};{}m",
            cell.background[0], cell.background[1], cell.background[2],
        ),
        (false, false) => Ok(()),
    }
    .expect("writing to Vec cannot fail");
    bytes.extend_from_slice("▀".as_bytes());
    colors.foreground = Some(cell.foreground);
    colors.background = Some(cell.background);
}

pub fn encode_cell_diff(previous: &[Cell], current: &[Cell], cols: u16) -> Vec<u8> {
    assert_eq!(previous.len(), current.len());
    assert!(cols > 0);
    let cols = usize::from(cols);
    let mut bytes = Vec::new();
    let mut colors = TerminalColors::default();
    let mut index = 0;
    while index < current.len() {
        if previous[index] == current[index] {
            index += 1;
            continue;
        }

        let row = index / cols;
        let start = index;
        let mut end = start + 1;
        while end < current.len() && end / cols == row && previous[end] != current[end] {
            end += 1;
        }

        use std::io::Write as _;
        write!(bytes, "\x1b[{};{}H", row + 1, start % cols + 1)
            .expect("writing to Vec cannot fail");
        for &cell in &current[start..end] {
            push_cell(&mut bytes, cell, &mut colors);
        }
        index = end;
    }
    if !bytes.is_empty() {
        bytes.extend_from_slice(b"\x1b[0m");
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_wide_image_by_width() {
        assert_eq!(fitted_sample_size(1600, 1000, 80, 48), (76, 48));
    }

    #[test]
    fn fits_tall_image_by_height() {
        assert_eq!(fitted_sample_size(1000, 1600, 80, 48), (30, 48));
    }

    #[test]
    fn encodes_top_as_foreground_and_bottom_as_background() {
        let image = RgbImage::from_raw(1, 2, vec![1, 2, 3, 4, 5, 6]).unwrap();
        let rendered = render_half_blocks(&image, 1, 1);
        assert_eq!(rendered.cols, 1);
        assert_eq!(rendered.rows, 1);
        assert_eq!(
            String::from_utf8(rendered.bytes).unwrap(),
            "\x1b[38;2;1;2;3;48;2;4;5;6m▀\x1b[0m\x1b[K\r\n\x1b[0m"
        );
    }

    #[test]
    fn cell_diff_writes_only_changed_runs_at_absolute_positions() {
        let black = Cell {
            foreground: [0, 0, 0],
            background: [0, 0, 0],
        };
        let red = Cell {
            foreground: [255, 0, 0],
            background: [0, 0, 0],
        };
        let previous = vec![black; 6];
        let mut current = previous.clone();
        current[1] = red;
        current[2] = red;
        current[5] = red;

        let diff = String::from_utf8(encode_cell_diff(&previous, &current, 3)).unwrap();
        assert_eq!(
            diff,
            concat!(
                "\x1b[1;2H",
                "\x1b[38;2;255;0;0;48;2;0;0;0m▀",
                "▀",
                "\x1b[2;3H",
                "▀",
                "\x1b[0m",
            )
        );
    }

    #[test]
    fn flat_ansi_runs_reuse_terminal_colors() {
        let image = RgbImage::from_pixel(80, 48, Rgb([12, 34, 56]));
        let rendered = render_half_blocks(&image, 80, 24);
        // A stateless encoder used about 140 KiB for this flat sample. Color state makes every
        // cell after the first in a row just the three-byte half-block glyph.
        assert!(
            rendered.bytes.len() < 10_000,
            "{} bytes",
            rendered.bytes.len()
        );
    }

    #[test]
    fn identical_cells_produce_no_terminal_output() {
        let cell = Cell {
            foreground: [1, 2, 3],
            background: [4, 5, 6],
        };
        assert!(encode_cell_diff(&[cell; 4], &[cell; 4], 2).is_empty());
    }

    #[test]
    fn zooms_around_center() {
        assert_eq!(
            viewport_rect(
                1000,
                600,
                Viewport {
                    zoom: 2.0,
                    center_x: 0.5,
                    center_y: 0.5,
                }
            ),
            ViewportRect {
                x: 250,
                y: 150,
                width: 500,
                height: 300,
            }
        );
    }

    #[test]
    fn clamps_viewport_at_edges() {
        assert_eq!(
            viewport_rect(
                1000,
                600,
                Viewport {
                    zoom: 4.0,
                    center_x: 0.0,
                    center_y: 1.0,
                }
            ),
            ViewportRect {
                x: 0,
                y: 450,
                width: 250,
                height: 150,
            }
        );
    }

    #[test]
    fn maps_source_viewport_to_downscaled_raster_with_outward_rounding() {
        assert_eq!(
            map_viewport_to_raster(
                ViewportRect {
                    x: 251,
                    y: 101,
                    width: 499,
                    height: 299,
                },
                1000,
                600,
                400,
                240,
            ),
            ViewportRect {
                x: 100,
                y: 40,
                width: 200,
                height: 120,
            }
        );
    }

    #[test]
    fn rejects_invalid_viewport() {
        let image = RgbImage::new(1, 1);
        assert!(
            render_half_blocks_viewport(
                &image,
                1,
                1,
                Viewport {
                    zoom: 0.5,
                    ..Viewport::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn raster_viewport_downscales_but_does_not_wastefully_upscale() {
        let image = RgbImage::new(100, 50);
        let downscaled = render_raster_viewport(&image, 40, 40, Viewport::default()).unwrap();
        assert_eq!(downscaled.image.dimensions(), (40, 20));

        let original = render_raster_viewport(&image, 200, 200, Viewport::default()).unwrap();
        assert_eq!(original.image.dimensions(), (100, 50));
    }

    #[test]
    fn raster_resolution_can_be_capped_without_shrinking_its_display_box() {
        let image = RgbImage::new(3200, 1800);
        let raster = render_raster_viewport(&image, 1920, 1080, Viewport::default()).unwrap();
        assert_eq!(raster.image.dimensions(), (1920, 1080));
        assert_eq!(
            fit_dimensions(raster.viewport.width, raster.viewport.height, 3200, 1800),
            (3200, 1800)
        );
    }

    #[test]
    fn cell_aligned_raster_has_the_exact_placement_aspect_ratio() {
        let image = RgbImage::new(320, 180);
        let aligned = align_raster_to_cell_grid(&image, 40, 12, 8, 16, 1920, 1080);
        assert_eq!(aligned.dimensions(), (320, 192));
        assert_eq!(aligned.width() * 12 * 16, aligned.height() * 40 * 8);
    }

    #[test]
    fn reduced_color_precision_has_a_strict_one_level_error_at_seven_bits() {
        let mut image = RgbImage::from_raw(2, 1, vec![0, 1, 2, 127, 128, 255]).unwrap();
        let original = image.clone();
        reduce_color_precision(&mut image, 7);
        assert_eq!(image.as_raw(), &[0, 0, 2, 126, 128, 254]);
        assert!(
            original
                .as_raw()
                .iter()
                .zip(image.as_raw())
                .all(|(before, after)| before.abs_diff(*after) <= 1)
        );
    }

    #[test]
    fn raster_tiles_are_cell_aligned_and_only_emit_changes() {
        let previous = RgbImage::new(320, 180);
        let mut current = previous.clone();
        current.put_pixel(150, 20, Rgb([1, 2, 3]));
        let tiles = changed_raster_tiles(Some(&previous), &current, 40, 18, 128);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].index, 1);
        assert_eq!((tiles[0].col, tiles[0].row), (16, 0));
        assert_eq!((tiles[0].cols, tiles[0].rows), (16, 13));
        assert_eq!(tiles[0].image.dimensions(), (128, 130));

        let all = changed_raster_tiles(None, &current, 40, 18, 128);
        assert_eq!(all.len(), 6);
        assert_eq!(all.last().unwrap().index, 5);
        assert_eq!((all.last().unwrap().cols, all.last().unwrap().rows), (8, 5));
        assert_eq!(all.last().unwrap().image.dimensions(), (64, 50));
    }
}
