//! The guest's screen: screenshots and text recognition.
//!
//! QEMU's `screendump` writes the framebuffer as PNG; [`Image`] decodes it (and encodes,
//! for tests). [`read_text`] recovers the text of a text console from a screenshot.
//!
//! # Reading text
//!
//! fbcond (the text VTs) and fbbootlogd (the boot log on VT 1) draw through
//! `console-draw`, and the kernel's early graphical debug draws the same way: each character
//! is an 8x16 cell at `(column * 8, row * 16)`, every pixel either the cell's foreground or
//! its background colour, with the glyph from orbclient's `FONT` (a bit set means
//! foreground, most significant bit leftmost). The cursor is the same cell with the colours
//! swapped. So the text is recovered exactly, without a general OCR: for each cell, take its
//! two colours, build the 16-byte bitmap for each choice of foreground and look it up among
//! the font's glyphs. When both choices are glyphs (a space and a full block, say), the one
//! whose background is the screen's most common background wins. A cell of one colour reads
//! as a space (so the cursor on an empty cell does not add a character; a full block `█`
//! also reads as a space). Cells with more than two colours (a desktop) or no matching
//! glyph read as [`UNKNOWN`].
//!
//! The glyph table is GNU Unifont as shipped in orbclient (GPLv2+ with the font-embedding
//! exception); the harness links it from the orbclient crate rather than copying it.
//! A console configured with a PSF font (`/etc/fbcond.toml`) cannot be read this way.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Glyph width in pixels.
pub const CELL_W: u32 = 8;
/// Glyph height in pixels.
pub const CELL_H: u32 = 16;
/// What [`read_text`] puts in a cell it cannot read.
pub const UNKNOWN: char = '\u{FFFD}';

/// A screenshot problem.
#[derive(Debug, Error)]
pub enum ScreenError {
    /// The PNG could not be decoded.
    #[error("cannot decode PNG")]
    Decode(#[from] png::DecodingError),
    /// The PNG could not be encoded.
    #[error("cannot encode PNG")]
    Encode(#[from] png::EncodingError),
    /// The PNG has a layout the harness does not read.
    #[error("unsupported PNG layout: {0:?}")]
    Layout(png::ColorType),
    /// A screen predicate that does not parse.
    #[error(
        "unknown screen predicate {0:?} (text:<s>, regex:<re>, changed, stable[:<ms>], nonblank)"
    )]
    Predicate(String),
    /// A predicate's regular expression is invalid.
    #[error("invalid regular expression in screen predicate")]
    Regex(#[from] regex::Error),
}

/// An RGB image, one `0x00RRGGBB` word per pixel, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixels, `width * height` of them.
    pub pixels: Vec<u32>,
}

impl Image {
    /// A `width` by `height` image filled with `color`.
    pub fn filled(width: u32, height: u32, color: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![color; width as usize * height as usize],
        }
    }

    /// Decodes a PNG (8- or 16-bit grey, grey-alpha, RGB, RGBA or palette); alpha is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`ScreenError::Decode`] for a broken file and [`ScreenError::Layout`] for colour types
    /// that do not reduce to 8-bit channels.
    pub fn from_png(bytes: &[u8]) -> Result<Self, ScreenError> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf)?;
        let data = &buf[..info.buffer_size()];
        let channels = match info.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            other @ png::ColorType::Indexed => return Err(ScreenError::Layout(other)),
        };
        let stride = info.line_size;
        let mut pixels = Vec::with_capacity(info.width as usize * info.height as usize);
        for row in data.chunks(stride).take(info.height as usize) {
            for px in row.chunks(channels).take(info.width as usize) {
                let (r, g, b) = if channels < 3 {
                    (px[0], px[0], px[0])
                } else {
                    (px[0], px[1], px[2])
                };
                pixels.push(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
            }
        }
        Ok(Self {
            width: info.width,
            height: info.height,
            pixels,
        })
    }

    /// Encodes the image as an 8-bit RGB PNG.
    ///
    /// # Errors
    ///
    /// [`ScreenError::Encode`] if the encoder fails.
    pub fn to_png(&self) -> Result<Vec<u8>, ScreenError> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let data: Vec<u8> = self
            .pixels
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8])
            .collect();
        writer.write_image_data(&data)?;
        writer.finish()?;
        Ok(out)
    }

    /// The pixel at `(x, y)`.
    ///
    /// # Panics
    ///
    /// If `(x, y)` lies outside the image, or `pixels` is shorter than `width * height`.
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels[y as usize * self.width as usize + x as usize]
    }

    /// Whether every pixel has the same colour.
    pub fn is_blank(&self) -> bool {
        self.pixels.windows(2).all(|w| w[0] == w[1])
    }

    /// Pixels that differ from `other` (every pixel when the sizes differ).
    pub fn diff(&self, other: &Image) -> usize {
        if (self.width, self.height) != (other.width, other.height) {
            return self.pixels.len().max(other.pixels.len());
        }
        self.pixels
            .iter()
            .zip(&other.pixels)
            .filter(|(a, b)| a != b)
            .count()
    }

    /// The bounding box `(x, y, width, height)` of pixels that differ from `other`, if any
    /// do and the sizes match.
    pub fn diff_box(&self, other: &Image) -> Option<(u32, u32, u32, u32)> {
        if (self.width, self.height) != (other.width, other.height) {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..self.height {
            for x in 0..self.width {
                if self.pixel(x, y) != other.pixel(x, y) {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
        }
        (x0 != u32::MAX).then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }
}

/// The console font: orbclient's 8x16 glyphs, indexed by code point.
fn font() -> &'static [u8] {
    orbclient::FONT
}

/// The 16-byte bitmap of `c`, if the font has it.
pub fn glyph(c: char) -> Option<[u8; 16]> {
    let start = c as usize * 16;
    font().get(start..start + 16)?.try_into().ok()
}

fn glyph_table() -> &'static HashMap<[u8; 16], char> {
    static TABLE: OnceLock<HashMap<[u8; 16], char>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = HashMap::new();
        // Earlier ranges win when two characters share a glyph (Latin A and Cyrillic А).
        let preferred = (' '..='~')
            .chain('\u{a0}'..='\u{ff}')
            .chain('\u{2500}'..='\u{259f}');
        let rest = (0x100..0x10000u32)
            .filter_map(char::from_u32)
            .filter(|c| !c.is_control());
        for c in preferred.chain(rest) {
            if let Some(bits) = glyph(c) {
                table.entry(bits).or_insert(c);
            }
        }
        table
    })
}

/// The text read from a screenshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextScreen {
    /// Columns (screen width / 8).
    pub cols: u32,
    /// Rows (screen height / 16).
    pub rows: u32,
    /// One string per row, trailing spaces removed.
    pub lines: Vec<String>,
    /// Cells that did not read as a character.
    pub unknown: usize,
}

impl TextScreen {
    /// The rows joined by newlines, without trailing empty rows.
    pub fn text(&self) -> String {
        let last = self
            .lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        self.lines[..last].join("\n")
    }
}

/// Reads the text of a text-console screenshot (see the [module docs](self)).
pub fn read_text(image: &Image) -> TextScreen {
    let cols = image.width / CELL_W;
    let rows = image.height / CELL_H;
    let background = dominant_background(image, cols, rows);
    let table = glyph_table();
    let mut unknown = 0;
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            let c = read_cell(image, col, row, background, table);
            if c == UNKNOWN {
                unknown += 1;
            }
            line.push(c);
        }
        let trimmed = line.trim_end_matches(' ').len();
        line.truncate(trimmed);
        lines.push(line);
    }
    TextScreen {
        cols,
        rows,
        lines,
        unknown,
    }
}

fn cell_colors(image: &Image, col: u32, row: u32) -> Option<(u32, Option<u32>)> {
    let mut first = None;
    let mut second = None;
    for y in row * CELL_H..(row + 1) * CELL_H {
        for x in col * CELL_W..(col + 1) * CELL_W {
            let p = image.pixel(x, y);
            match (first, second) {
                (None, _) => first = Some(p),
                (Some(a), _) if a == p => {}
                (Some(_), None) => second = Some(p),
                (Some(_), Some(b)) if b == p => {}
                _ => return None,
            }
        }
    }
    first.map(|a| (a, second))
}

/// The most common colour of single-colour cells: the console's background.
fn dominant_background(image: &Image, cols: u32, rows: u32) -> Option<u32> {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for row in 0..rows {
        for col in 0..cols {
            if let Some((color, None)) = cell_colors(image, col, row) {
                *counts.entry(color).or_default() += 1;
            }
        }
    }
    counts.into_iter().max_by_key(|&(_, n)| n).map(|(c, _)| c)
}

fn read_cell(
    image: &Image,
    col: u32,
    row: u32,
    background: Option<u32>,
    table: &HashMap<[u8; 16], char>,
) -> char {
    let Some((a, second)) = cell_colors(image, col, row) else {
        return UNKNOWN;
    };
    if second.is_none() {
        return ' ';
    }
    let mut bits = [0u8; 16];
    for (r, byte) in bits.iter_mut().enumerate() {
        for c in 0..CELL_W {
            if image.pixel(col * CELL_W + c, row * CELL_H + r as u32) == a {
                *byte |= 0x80 >> c;
            }
        }
    }
    let inverse = bits.map(|byte| !byte);
    match (table.get(&bits), table.get(&inverse)) {
        (Some(&fg_a), Some(&fg_b)) => {
            if background == Some(a) {
                fg_b
            } else {
                fg_a
            }
        }
        (Some(&c), None) | (None, Some(&c)) => c,
        (None, None) => UNKNOWN,
    }
}

/// Draws text the way `console-draw` does: `cols` by `rows` cells, each line's characters
/// in `fg` on `bg`; `cursor` (column, row) is drawn with the colours swapped. Used to test
/// [`read_text`] and by fake screens.
pub fn render_text(
    lines: &[&str],
    cols: u32,
    rows: u32,
    fg: u32,
    bg: u32,
    cursor: Option<(u32, u32)>,
) -> Image {
    let mut image = Image::filled(cols * CELL_W, rows * CELL_H, bg);
    for row in 0..rows {
        let text: Vec<char> = lines
            .get(row as usize)
            .map_or(Vec::new(), |l| l.chars().collect());
        for col in 0..cols {
            let c = text.get(col as usize).copied().unwrap_or(' ');
            let (f, b) = if cursor == Some((col, row)) {
                (bg, fg)
            } else {
                (fg, bg)
            };
            let bits = glyph(c).unwrap_or([0; 16]);
            for (r, byte) in bits.iter().enumerate() {
                for x in 0..CELL_W {
                    let on = byte & (0x80 >> x) != 0;
                    let index = (row * CELL_H + r as u32) as usize * image.width as usize
                        + (col * CELL_W + x) as usize;
                    image.pixels[index] = if on { f } else { b };
                }
            }
        }
    }
    image
}

/// What `wait-screen` waits for.
#[derive(Clone, Debug)]
pub enum ScreenPredicate {
    /// The screen's text contains this string.
    Text(String),
    /// The screen's text matches this regular expression.
    Regex(Regex),
    /// The screen differs from the first screenshot of the wait.
    Changed,
    /// The screen has not changed for this long.
    Stable(Duration),
    /// The screen is not a single colour.
    NonBlank,
}

impl std::str::FromStr for ScreenPredicate {
    type Err = ScreenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(text) = s.strip_prefix("text:") {
            return Ok(Self::Text(text.to_owned()));
        }
        if let Some(re) = s.strip_prefix("regex:") {
            return Ok(Self::Regex(Regex::new(re)?));
        }
        match s {
            "changed" => return Ok(Self::Changed),
            "nonblank" => return Ok(Self::NonBlank),
            "stable" => return Ok(Self::Stable(Duration::from_millis(1000))),
            _ => {}
        }
        if let Some(ms) = s.strip_prefix("stable:")
            && let Ok(ms) = ms.parse::<u64>()
        {
            return Ok(Self::Stable(Duration::from_millis(ms)));
        }
        Err(ScreenError::Predicate(s.to_owned()))
    }
}

/// Evaluates a [`ScreenPredicate`] over successive screenshots.
#[derive(Debug)]
pub struct PredicateState {
    predicate: ScreenPredicate,
    first: Option<Image>,
    last: Option<(Image, Instant)>,
}

impl PredicateState {
    /// Starts a wait for `predicate`.
    pub fn new(predicate: ScreenPredicate) -> Self {
        Self {
            predicate,
            first: None,
            last: None,
        }
    }

    /// Checks the screenshot taken at `now`; `true` when the predicate holds.
    pub fn check(&mut self, image: Image, now: Instant) -> bool {
        match &self.predicate {
            ScreenPredicate::Text(text) => read_text(&image).text().contains(text.as_str()),
            ScreenPredicate::Regex(re) => re.is_match(&read_text(&image).text()),
            ScreenPredicate::NonBlank => !image.is_blank(),
            ScreenPredicate::Changed => match &self.first {
                None => {
                    self.first = Some(image);
                    false
                }
                Some(first) => first.diff(&image) > 0,
            },
            ScreenPredicate::Stable(period) => {
                let period = *period;
                match &self.last {
                    Some((last, since)) if last.diff(&image) == 0 => {
                        now.duration_since(*since) >= period
                    }
                    _ => {
                        self.last = Some((image, now));
                        false
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: u32 = 0xc0c0c0;
    const BLACK: u32 = 0x000000;

    #[test]
    fn font_has_ascii_glyphs() {
        assert_eq!(glyph(' '), Some([0; 16]));
        assert_ne!(glyph('A'), glyph('B'));
        let table = glyph_table();
        assert_eq!(table.get(&glyph('A').expect("A")), Some(&'A'));
        assert_eq!(table.get(&[0; 16]), Some(&' '));
    }

    #[test]
    fn reads_rendered_console_text_through_png() {
        let lines = [
            "redox login: root",
            "Password:",
            "root:~# echo hello {world} | grep -o 'w.*d' && ls /scheme/",
            "~!@#$%^&*()_+`-=[]\\;',./<>?:\"|",
            "box ─│┌┐ block ▀▄ é ⏎",
        ];
        let image = render_text(&lines, 80, 8, WHITE, BLACK, Some((17, 0)));
        let png = image.to_png().expect("encode");
        let decoded = Image::from_png(&png).expect("decode");
        assert_eq!(decoded, image);
        let screen = read_text(&decoded);
        assert_eq!((screen.cols, screen.rows), (80, 8));
        assert_eq!(screen.unknown, 0);
        assert_eq!(screen.text(), lines.join("\n"));
    }

    #[test]
    fn colours_and_inverse_cells() {
        // Red on blue, and the inverse, read the same.
        let a = render_text(&["Ok"], 4, 1, 0xff0000, 0x0000ff, None);
        let b = render_text(&["Ok"], 4, 1, 0x0000ff, 0xff0000, None);
        assert_eq!(read_text(&a).text(), "Ok");
        assert_eq!(read_text(&b).text(), "Ok");
        // A screen that is not text reads as unknown cells.
        let mut noisy = Image::filled(16, 16, 0);
        for (i, p) in noisy.pixels.iter_mut().enumerate() {
            *p = i as u32 * 0x010101;
        }
        let screen = read_text(&noisy);
        assert_eq!(screen.unknown, 2);
    }

    #[test]
    fn predicates() {
        let t0 = Instant::now();
        let login = render_text(&["redox login:"], 20, 2, WHITE, BLACK, None);
        let shell = render_text(&["root:~#"], 20, 2, WHITE, BLACK, None);

        let mut text: PredicateState =
            PredicateState::new("text:login".parse().expect("predicate"));
        assert!(text.check(login.clone(), t0));
        assert!(!text.check(shell.clone(), t0));

        let mut re = PredicateState::new("regex:^root:.*#$".parse().expect("predicate"));
        assert!(re.check(shell.clone(), t0));

        let mut changed = PredicateState::new("changed".parse().expect("predicate"));
        assert!(!changed.check(login.clone(), t0));
        assert!(!changed.check(login.clone(), t0));
        assert!(changed.check(shell.clone(), t0));

        let mut stable = PredicateState::new("stable:500".parse().expect("predicate"));
        assert!(!stable.check(login.clone(), t0));
        assert!(!stable.check(login.clone(), t0 + Duration::from_millis(100)));
        assert!(!stable.check(shell.clone(), t0 + Duration::from_millis(200)));
        assert!(stable.check(shell.clone(), t0 + Duration::from_millis(800)));

        let mut nonblank = PredicateState::new("nonblank".parse().expect("predicate"));
        assert!(!nonblank.check(Image::filled(8, 8, 0), t0));
        assert!(nonblank.check(login.clone(), t0));

        assert!(matches!(
            "flashing".parse::<ScreenPredicate>(),
            Err(ScreenError::Predicate(_))
        ));
        let (x, y, w, h) = login.diff_box(&shell).expect("the screens differ");
        assert!(x < 12 * CELL_W && y < CELL_H && w > 0 && h <= CELL_H);
        assert_eq!(login.diff_box(&login), None);
    }
}
