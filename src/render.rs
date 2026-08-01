use image::imageops::FilterType;
use image::{Rgb, RgbImage};

pub struct RenderedFrame {
    pub bytes: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}

pub fn render_half_blocks(source: &RgbImage, max_cols: u16, max_rows: u16) -> RenderedFrame {
    let (sample_width, sample_height) = fitted_sample_size(
        source.width(),
        source.height(),
        max_cols.into(),
        u32::from(max_rows) * 2,
    );
    let resized =
        image::imageops::resize(source, sample_width, sample_height, FilterType::Triangle);
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

    RenderedFrame {
        bytes,
        cols: sample_width as u16,
        rows: rows as u16,
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
}
