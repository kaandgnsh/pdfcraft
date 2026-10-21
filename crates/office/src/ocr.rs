//! Scanned pages as text: a page's picture is decoded and read by `pdfcraft-ocr`, and the lines
//! it finds become paragraphs where they were on the page.

use pdfcraft_export::{Block, Image};

/// Pictures bigger than this many pixels are not read (a hostile size, or a poster).
const MAX_PIXELS: u64 = 60_000_000;

/// A recognised line on the page: its text, `[x0, y0, x1, y1]` in points (y up) and its type size
/// in points.
struct Placed {
    text: String,
    rect: [f64; 4],
    height: f64,
}

/// How tall a word's box stands against the type size, by its letters (common sans and serif
/// faces): capitals and digits reach the cap height, lower case with ascenders about as far, and a
/// descender (g, j, p, q, y) adds the depth below the baseline.
fn box_share(word: &str) -> f64 {
    let lower = word.chars().any(char::is_lowercase);
    let descends = word.chars().any(|c| matches!(c, 'g' | 'j' | 'p' | 'q' | 'y' | ',' | ';'));
    match (lower, descends) {
        (_, true) => 0.95,
        (true, false) => 0.75,
        (false, false) => 0.72,
    }
}

/// The words `ocr` reads in the picture `im`, as paragraphs on the page; none when the picture
/// can't be decoded or read, is turned, or shows no text.
pub(crate) fn recognize(ocr: &pdfcraft_ocr::Ocr, im: &Image) -> Vec<Block> {
    if im.rotation.abs() > 0.5 || !im.rect.iter().all(|v| v.is_finite()) {
        return Vec::new();
    }
    let Ok(decoded) = image::load_from_memory(&im.bytes) else { return Vec::new() };
    let (w, h) = (decoded.width(), decoded.height());
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Vec::new();
    }
    let rgb = decoded.to_rgb8();
    let Ok(lines) = ocr.recognize(rgb.as_raw(), w, h) else { return Vec::new() };
    let [x0, y0, x1, y1] = im.rect;
    let (sx, sy) = ((x1 - x0) / f64::from(w), (y1 - y0) / f64::from(h));
    let placed: Vec<Placed> = lines
        .iter()
        .filter_map(|line| {
            let text = line.text();
            let words: Vec<[f64; 4]> = line.words.iter().map(|wd| wd.rect.map(f64::from)).filter(|r| r.iter().all(|v| v.is_finite())).collect();
            if text.trim().is_empty() || words.is_empty() {
                return None;
            }
            let [l, t, r, b] = words.iter().copied().reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])?;
            // The type size each word's box suggests; the line's is their median.
            let mut sizes: Vec<f64> = line
                .words
                .iter()
                .map(|wd| (f64::from(wd.rect[3]) - f64::from(wd.rect[1])).abs() / box_share(&wd.text))
                .filter(|v| v.is_finite())
                .collect();
            sizes.sort_by(f64::total_cmp);
            let height = sizes.get(sizes.len() / 2).copied().unwrap_or(b - t) * sy;
            Some(Placed { text: text.trim().to_string(), rect: [x0 + l * sx, y1 - b * sy, x0 + r * sx, y1 - t * sy], height })
        })
        .filter(|p| p.height > 0.0)
        .collect();
    paragraphs(placed)
}

/// Lines into paragraphs: a line joins the one above when it is the same height, starts near it
/// and follows at a line pitch (a paragraph's spacing, further down, starts another).
fn paragraphs(lines: Vec<Placed>) -> Vec<Block> {
    let mut groups: Vec<Vec<Placed>> = Vec::new();
    for line in lines {
        let joins = groups.last().and_then(|g| g.last()).is_some_and(|prev| {
            let h = prev.height.max(line.height);
            (prev.height - line.height).abs() <= h * 0.15
                && (prev.rect[0] - line.rect[0]).abs() <= h * 2.0
                && prev.rect[3] - line.rect[3] <= h * 1.5
                && prev.rect[3] > line.rect[3]
        });
        match groups.last_mut() {
            Some(g) if joins => g.push(line),
            _ => groups.push(vec![line]),
        }
    }
    groups
        .into_iter()
        .filter_map(|g| {
            let rect = g.iter().map(|l| l.rect).reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])?;
            let height = g.iter().map(|l| l.height).sum::<f64>() / g.len() as f64;
            let rows: Vec<String> = g.iter().map(|l| l.text.clone()).collect();
            Some(Block {
                text: rows.join(" "),
                rect,
                size: height.clamp(4.0, 400.0),
                bold: false,
                italic: false,
                color: [0.0; 3],
                rotation: 0.0,
                invisible: false,
                font: String::new(),
                lines: rows.len(),
                indent: 0.0,
                rows: if rows.len() > 1 { rows } else { Vec::new() },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, top: f64, size: f64) -> Placed {
        Placed { text: text.into(), rect: [72.0, top - size, 300.0, top], height: size }
    }

    #[test]
    fn lines_at_a_pitch_join_and_spacing_parts_them() {
        let blocks = paragraphs(vec![
            line("Heading", 700.0, 20.0),
            line("first line of a paragraph", 670.0, 12.0),
            line("its second line", 656.0, 12.0),
            line("a new paragraph after spacing", 632.0, 12.0),
        ]);
        let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["Heading", "first line of a paragraph its second line", "a new paragraph after spacing"]);
        assert_eq!(blocks[1].rows, ["first line of a paragraph", "its second line"]);
        assert!(blocks[0].size > blocks[1].size);
    }

    #[test]
    fn a_word_box_gives_its_type_size() {
        assert!((box_share("HELLO") - 0.72).abs() < 1e-9);
        assert!((box_share("brown") - 0.75).abs() < 1e-9);
        assert!((box_share("jumps") - 0.95).abs() < 1e-9);
    }
}
