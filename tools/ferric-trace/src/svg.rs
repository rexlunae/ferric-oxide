//! A small, self-contained flame graph SVG drawn from folded stacks
//! ([`crate::flamegraph::Folded`]).
//!
//! The root is at the bottom and each frame sits on its caller, as wide as the samples it was
//! on. Frames are sorted by name at each level, so the x axis is not time. Each frame has a
//! `<title>` (shown on hover) with its name, sample count and share; frames too narrow to read
//! are left out. Colours are a warm hue picked from a hash of the name, so a function keeps its
//! colour from one graph to the next. There is no script, so the file opens anywhere.

use std::collections::BTreeMap;
use std::fmt::Write;

/// The image width, in pixels.
pub const WIDTH: f64 = 1200.0;

const FRAME_HEIGHT: f64 = 16.0;
const MARGIN: f64 = 10.0;
const TITLE_HEIGHT: f64 = 30.0;
/// Narrower frames are not drawn.
const MIN_WIDTH: f64 = 0.1;
/// Roughly the advance of one 11-pixel character.
const CHAR_WIDTH: f64 = 6.6;

#[derive(Default)]
struct Node<'a> {
    value: u64,
    children: BTreeMap<&'a str, Node<'a>>,
}

impl<'a> Node<'a> {
    fn add(&mut self, frames: &[&'a str], count: u64) {
        self.value += count;
        if let Some((first, rest)) = frames.split_first() {
            self.children.entry(first).or_default().add(rest, count);
        }
    }

    fn depth(&self) -> usize {
        self.children
            .values()
            .map(|c| c.depth() + 1)
            .max()
            .unwrap_or(0)
    }
}

/// Draws `stacks` (each `;`-separated stack, root first, with its count) as an SVG with the
/// heading `title`.
pub fn render(stacks: &BTreeMap<String, u64>, title: &str) -> String {
    let mut root = Node::default();
    for (stack, &count) in stacks {
        let frames: Vec<&str> = stack.split(';').collect();
        root.add(&frames, count);
    }
    let depth = root.depth();
    let height = TITLE_HEIGHT + depth as f64 * FRAME_HEIGHT + 2.0 * MARGIN;
    let mut svg = String::new();
    let _ = write!(
        svg,
        "<?xml version=\"1.0\" standalone=\"no\"?>\n\
         <svg version=\"1.1\" width=\"{WIDTH}\" height=\"{height}\" viewBox=\"0 0 {WIDTH} {height}\" \
         xmlns=\"http://www.w3.org/2000/svg\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#f8f8f0\"/>\n\
         <text x=\"{}\" y=\"20\" font-family=\"monospace\" font-size=\"15\" text-anchor=\"middle\">{}</text>\n\
         <g font-family=\"monospace\" font-size=\"11\">\n",
        WIDTH / 2.0,
        escape(title)
    );
    if root.value > 0 {
        let scale = (WIDTH - 2.0 * MARGIN) / root.value as f64;
        let bottom = height - MARGIN;
        let mut layout = Layout {
            svg: &mut svg,
            scale,
            total: root.value,
            bottom,
        };
        let mut x = MARGIN;
        for (name, child) in &root.children {
            layout.frame(name, child, x, 0);
            x += child.value as f64 * scale;
        }
    }
    svg.push_str("</g>\n</svg>\n");
    svg
}

struct Layout<'s> {
    svg: &'s mut String,
    scale: f64,
    total: u64,
    bottom: f64,
}

impl Layout<'_> {
    fn frame(&mut self, name: &str, node: &Node<'_>, x: f64, level: usize) {
        let width = node.value as f64 * self.scale;
        if width < MIN_WIDTH {
            return;
        }
        let y = self.bottom - (level + 1) as f64 * FRAME_HEIGHT;
        let share = 100.0 * node.value as f64 / self.total as f64;
        let (r, g, b) = colour(name);
        let _ = write!(
            self.svg,
            "<g><title>{} ({} samples, {share:.2}%)</title>\
             <rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{width:.1}\" height=\"{}\" fill=\"rgb({r},{g},{b})\" rx=\"2\"/>",
            escape(name),
            node.value,
            FRAME_HEIGHT - 1.0
        );
        if let Some(label) = label(name, width) {
            let _ = write!(
                self.svg,
                "<text x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                x + 3.0,
                y + FRAME_HEIGHT - 4.5,
                escape(&label)
            );
        }
        self.svg.push_str("</g>\n");
        let mut child_x = x;
        for (child_name, child) in &node.children {
            self.frame(child_name, child, child_x, level + 1);
            child_x += child.value as f64 * self.scale;
        }
    }
}

/// The name cut to fit `width`, with `..` when cut; `None` if not even three characters fit.
fn label(name: &str, width: f64) -> Option<String> {
    let fits = ((width - 6.0) / CHAR_WIDTH).floor();
    if fits < 3.0 {
        return None;
    }
    let fits = fits as usize;
    if name.chars().count() <= fits {
        return Some(name.to_owned());
    }
    let mut cut: String = name.chars().take(fits - 2).collect();
    cut.push_str("..");
    Some(cut)
}

/// A warm colour from an FNV-1a hash of the name.
fn colour(name: &str) -> (u8, u8, u8) {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let pick = |shift: u32, span: u64| (hash >> shift) % span;
    (
        200 + pick(0, 56) as u8,
        60 + pick(8, 170) as u8,
        pick(16, 60) as u8,
    )
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stacks(lines: &[(&str, u64)]) -> BTreeMap<String, u64> {
        lines.iter().map(|(s, c)| ((*s).to_owned(), *c)).collect()
    }

    #[test]
    fn draws_each_frame_once_with_its_share() {
        let svg = render(
            &stacks(&[("ion;main;read", 3), ("ion;main;write<&str>", 1)]),
            "a & b",
        );
        assert!(svg.starts_with("<?xml"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains(">a &amp; b</text>"));
        assert_eq!(svg.matches("<rect x=").count(), 4);
        assert!(svg.contains("<title>main (4 samples, 100.00%)</title>"));
        assert!(svg.contains("<title>read (3 samples, 75.00%)</title>"));
        assert!(svg.contains("write&lt;&amp;str&gt;"));
        // The root frame spans the whole width between the margins.
        assert!(svg.contains(&format!("width=\"{:.1}\"", WIDTH - 2.0 * MARGIN)));
    }

    #[test]
    fn leaves_out_frames_too_narrow_to_see() {
        let svg = render(&stacks(&[("a", 1_000_000), ("b", 1)]), "t");
        assert!(svg.contains("<title>a "));
        assert!(!svg.contains("<title>b "));
    }

    #[test]
    fn an_empty_graph_is_still_an_svg() {
        let svg = render(&BTreeMap::new(), "empty");
        assert!(!svg.contains("<rect x="));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn labels_fit_their_frame() {
        assert_eq!(label("main", 100.0).as_deref(), Some("main"));
        assert_eq!(
            label("a_long_function_name", 50.0).as_deref(),
            Some("a_lo..")
        );
        assert_eq!(label("main", 10.0), None);
        assert_eq!(colour("main"), colour("main"));
    }
}
