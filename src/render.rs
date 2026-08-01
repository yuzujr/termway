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
    pub cols: u16,
    pub rows: u16,
    pub viewport: ViewportRect,
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

    for y in (0..sample_height).step_by(2) {
        for x in 0..sample_width {
            let top = resized.get_pixel(x, y);
            let bottom = if y + 1 < sample_height {
                resized.get_pixel(x, y + 1)
            } else {
                &Rgb([0, 0, 0])
            };
            push_cell(&mut bytes, top, bottom);
        }
        bytes.extend_from_slice(b"\x1b[0m\r\n");
    }
    bytes.extend_from_slice(b"\x1b[0m");

    Ok(RenderedFrame {
        bytes,
        cols: sample_width as u16,
        rows: rows as u16,
        viewport,
    })
}

fn validate_viewport(viewport: Viewport) -> Result<()> {
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

fn viewport_rect(width: u32, height: u32, viewport: Viewport) -> ViewportRect {
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

fn push_cell(bytes: &mut Vec<u8>, foreground: &Rgb<u8>, background: &Rgb<u8>) {
    use std::io::Write as _;

    write!(
        bytes,
        "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
        foreground[0], foreground[1], foreground[2], background[0], background[1], background[2],
    )
    .expect("writing to Vec cannot fail");
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
            "\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m▀\x1b[0m\r\n\x1b[0m"
        );
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
}
