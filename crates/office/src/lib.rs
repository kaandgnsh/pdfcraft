//! pdfcraft-office — Convert a PDF ▸ Word (.docx), PowerPoint (.pptx), HTML or RTF (L5).
//!
//! The one entry point other apps open a PDF with: [`convert`] takes the file's bytes and returns
//! an Office file they already read, so a word processor gets paragraphs, headings, tables and
//! pictures and a presentation app one slide per page. PdfCraft's own Export a PDF uses the same
//! reduction ([`pages`]), so the three apps cannot drift apart.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::Arc;

/// Points of vector artwork [`shapes`] reads from a whole document (about 16 MB, and the slide
/// XML they make): pages share content streams, so per-page limits alone don't bound a file.
pub const MAX_DOCUMENT_POINTS: usize = 1_000_000;

/// Biggest file [`convert`] reads (512 MiB). Larger ones are refused before anything is parsed.
pub const MAX_INPUT_BYTES: usize = 512 * 1024 * 1024;

/// What to convert to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Docx,
    /// A deck to rework: each page reflowed into a title and body text (PdfCraft's Export a PDF ▸
    /// PowerPoint).
    Pptx,
    /// A PDF opened as slides: each page a slide its size, with its vector artwork, pictures and
    /// text where they are on the page.
    PptxPages,
    Html,
    Rtf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The password to open an encrypted PDF with (user or owner password).
    pub password: Option<String>,
    /// Pages to convert (0-based), in this order; `None` converts every page.
    /// [`parse_page_range`] reads them from text such as `1-3, 5`.
    pub pages: Option<Vec<usize>>,
    /// The title written into the Office file; the PDF's own title when `None`.
    pub title: Option<String>,
}

/// A converted PDF.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Converted {
    pub bytes: Vec<u8>,
    /// Pages in the PDF, whether or not all were converted.
    pub page_count: usize,
    /// Pages (0-based) that draw forms nested deeper than [`pdfcraft_edit::MAX_FORM_DEPTH`] or
    /// more of them than [`pdfcraft_edit::MAX_FORM_VISITS`]: reading stops there to stay bounded
    /// on hostile files, so the text and images of those forms are missing. For
    /// [`Target::PptxPages`], also pages whose vector artwork was left out, in part or whole, to
    /// stay within [`MAX_DOCUMENT_POINTS`] or a page's own limits.
    pub truncated_pages: Vec<usize>,
    /// Converted pages (0-based) with pictures but no text: scans, whose words only text
    /// recognition can recover.
    pub image_only_pages: Vec<usize>,
    /// Pages (0-based) whose text was recognised from their picture ([`convert_with_ocr`]).
    pub ocr_pages: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the file is not a PDF")]
    NotPdf,
    #[error("the PDF is protected by a password")]
    NeedsPassword,
    #[error("the password is incorrect")]
    WrongPassword,
    #[error("the PDF is larger than {} MB", MAX_INPUT_BYTES / (1024 * 1024))]
    TooLarge,
    #[error("{0}")]
    BadPageRange(String),
    #[error("the PDF could not be read: {0}")]
    Damaged(String),
}

/// Does `bytes` hold a PDF? Its header (`%PDF-`) may follow up to 1024 bytes of junk, as
/// readers allow (ISO 32000-2 Annex H).
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.get(..bytes.len().min(1024 + 5)).is_some_and(|head| head.windows(5).any(|w| w == b"%PDF-"))
}

/// Pages named like `1-3, 5` (1-based, inclusive ranges, separated by commas or semicolons) as
/// 0-based indices, in the order given and each once. Every page must exist in `count`.
pub fn parse_page_range(text: &str, count: usize) -> Result<Vec<usize>, Error> {
    let bad = || Error::BadPageRange(format!("“{text}” isn't a page range of this PDF (pages 1–{count}): use numbers and ranges such as 1-3, 5"));
    let page = |s: &str| -> Result<usize, Error> {
        let n: usize = s.trim().parse().map_err(|_| bad())?;
        if n == 0 || n > count { Err(bad()) } else { Ok(n - 1) }
    };
    let mut out = Vec::new();
    for part in text.split([',', ';']).map(str::trim) {
        if part.is_empty() {
            return Err(bad());
        }
        let (from, to) = match part.split_once('-') {
            Some((a, b)) => (page(a)?, page(b)?),
            None => {
                let p = page(part)?;
                (p, p)
            }
        };
        if from > to {
            return Err(bad());
        }
        for p in from..=to {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// A page's displayed geometry: what [`pages`] needs to put reading-order content on the page as
/// shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageBox {
    /// Displayed size in points, after `/Rotate` and `/UserUnit`.
    pub width: f32,
    pub height: f32,
    /// Effective crop box in user space `[x0, y0, x1, y1]` (the visible region).
    pub crop: [f32; 4],
    /// Clockwise page rotation in degrees (0, 90, 180, 270).
    pub rotation: u16,
}

impl PageBox {
    /// PDF user space → view space (points, y down, after `/Rotate`).
    pub fn user_to_view(&self, x: f32, y: f32) -> [f32; 2] {
        let [cx0, cy0, cx1, cy1] = self.crop;
        let (u, v) = ((x - cx0) / (cx1 - cx0).max(1e-3), (cy1 - y) / (cy1 - cy0).max(1e-3));
        let (a, b) = match self.rotation {
            90 => (1.0 - v, u),
            180 => (1.0 - u, 1.0 - v),
            270 => (v, 1.0 - u),
            _ => (u, v),
        };
        [a * self.width, b * self.height]
    }
}

/// Every page's [`PageBox`], from the page tree. A degenerate box gets a Letter-sized placeholder
/// so layout never divides by zero.
pub fn page_boxes(cos: &pdfcraft_cos::Document) -> Vec<PageBox> {
    pdfcraft_model::pages(cos)
        .iter()
        .map(|p| {
            let c = p.crop(cos);
            let (w, h) = p.display_size(cos);
            let (w, h) = if w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 { (w as f32, h as f32) } else { (612.0, 792.0) };
            let rotation = match p.rotation(cos) {
                90 => 90,
                180 => 180,
                270 => 270,
                _ => 0,
            };
            PageBox { width: w, height: h, crop: [c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32], rotation }
        })
        .collect()
}

/// A PDF font's family name as an Office file names it: the subset tag (`ABCDEF+`), style
/// suffixes (`-Bold`, `,Italic`, `PSMT`) and joined words (`TimesNewRoman`) undone, and the 14
/// standard fonts given their metric-compatible Office families. Empty for names that say nothing
/// (`F1`).
pub fn family(base_font: &str) -> String {
    let name = base_font
        .split_once('+')
        .map_or(base_font, |(tag, rest)| if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) { rest } else { base_font });
    let name = name.split([',', '-']).next().unwrap_or("").trim();
    let name = ["PSMT", "MT", "PS"].iter().fold(name.to_string(), |n, suffix| n.strip_suffix(suffix).map(str::to_string).unwrap_or(n));
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "times" | "timesroman" | "timesnewroman" => return "Times New Roman".into(),
        "helvetica" | "arial" => return "Arial".into(),
        "courier" | "couriernew" => return "Courier New".into(),
        "symbol" => return "Symbol".into(),
        "zapfdingbats" => return "ZapfDingbats".into(),
        _ => {}
    }
    // A bare resource-like name (`F1`, `TT2`) is no family.
    if name.len() <= 3 || name.chars().filter(char::is_ascii_alphabetic).count() < 3 {
        return String::new();
    }
    // Split joined words: a capital after a lower-case letter, a digit after a letter, and a
    // capital before a lower-case one after capitals (`DejaVuSans` → `DejaVu Sans`).
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i + 1).copied();
        let split = match prev {
            Some(p) if c.is_ascii_uppercase() && p.is_ascii_lowercase() => {
                // `DejaVu`: a single capital between lower-case letters inside a known word stays.
                !(out.ends_with("Deja") && c == 'V')
            }
            Some(p) if c.is_ascii_digit() && p.is_ascii_alphabetic() => true,
            Some(p) if c.is_ascii_uppercase() && p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()) => true,
            _ => false,
        };
        if split {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The pages of `cos` (`only`: these 0-based indices, in this order) as paragraphs and images in
/// display space (points, y up, after `/Rotate` and `/UserUnit`), including what form XObjects
/// draw, and the pages whose forms were left partly unread. `info` is the document's page
/// geometry ([`page_boxes`], or the renderer's own in PdfCraft).
pub fn pages(cos: &pdfcraft_cos::Document, info: &[PageBox], only: Option<&[usize]>) -> (Vec<pdfcraft_export::Page>, Vec<usize>) {
    let all: Vec<usize> = (0..info.len()).collect();
    let mut truncated = Vec::new();
    let pages = only
        .unwrap_or(&all)
        .iter()
        .filter_map(|&i| Some((i, info.get(i)?)))
        .map(|(i, info)| {
            // Export uses the visible page in points, y up. Raw PDF coordinates include
            // CropBox offsets and precede /Rotate and /UserUnit.
            let visible_rect = |r: [f64; 4]| {
                let a = info.user_to_view(r[0] as f32, r[1] as f32);
                let b = info.user_to_view(r[2] as f32, r[3] as f32);
                [a[0].min(b[0]) as f64, (info.height - a[1].max(b[1])) as f64, a[0].max(b[0]) as f64, (info.height - a[1].min(b[1])) as f64]
            };
            let crop_width = f64::from(info.crop[2] - info.crop[0]);
            let display_width = f64::from(if info.rotation % 180 == 0 { info.width } else { info.height });
            let unit = if crop_width > 0.0 && crop_width.is_finite() && display_width.is_finite() { display_width / crop_width } else { 1.0 };
            let reading = pdfcraft_edit::reading_blocks_checked(cos, i).unwrap_or_default();
            if reading.truncated {
                truncated.push(i);
            }
            let blocks = reading
                .blocks
                .into_iter()
                .filter(|b| !b.text.trim().is_empty())
                .map(|b| {
                    let f = b.base_font.to_ascii_lowercase();
                    pdfcraft_export::Block {
                        text: b.text,
                        rect: visible_rect(b.rect),
                        size: b.size * unit,
                        bold: f.contains("bold") || f.contains("black") || f.contains("heavy"),
                        italic: f.contains("italic") || f.contains("oblique"),
                        color: b.color,
                        rotation: b.rotation - f64::from(info.rotation),
                        invisible: b.invisible || b.transparent,
                        font: family(&b.base_font),
                        lines: b.rows.len().max(1),
                        indent: b.indent,
                        rows: if b.rows.len() > 1 { b.editor_text.split('\n').map(|r| r.trim().to_string()).collect() } else { Vec::new() },
                    }
                })
                .collect();
            let images = pdfcraft_edit::reading_images(cos, i)
                .unwrap_or_default()
                .iter()
                .filter_map(|im| {
                    let (ext, bytes) = pdfcraft_create::image_file(cos, im.object?).ok()?;
                    Some(pdfcraft_export::Image {
                        ext: if ext == "jpg" { "jpg" } else { "png" },
                        bytes,
                        rect: visible_rect(im.rect),
                        size: [im.matrix[0].hypot(im.matrix[1]) * unit, im.matrix[2].hypot(im.matrix[3]) * unit],
                        rotation: im.matrix[1].atan2(im.matrix[0]).to_degrees() - f64::from(info.rotation),
                    })
                })
                .collect();
            pdfcraft_export::Page { width: info.width as f64, height: info.height as f64, blocks, images, shapes: Vec::new() }
        })
        .collect();
    (pages, truncated)
}

/// The vector artwork of the pages of `cos` (`only`: these 0-based indices, in this order), each
/// page's in drawing order and in display space (points, y up, after `/Rotate` and `/UserUnit`).
pub fn shapes(cos: &pdfcraft_cos::Document, info: &[PageBox], only: Option<&[usize]>) -> (Vec<Vec<pdfcraft_export::Shape>>, Vec<usize>) {
    shapes_within(cos, info, only, MAX_DOCUMENT_POINTS)
}

/// [`shapes`] with at most `budget` points in all: pages past it keep their place with what fits,
/// and are returned (0-based) with the pages whose own reading was cut short.
pub(crate) fn shapes_within(
    cos: &pdfcraft_cos::Document,
    info: &[PageBox],
    only: Option<&[usize]>,
    budget: usize,
) -> (Vec<Vec<pdfcraft_export::Shape>>, Vec<usize>) {
    let all: Vec<usize> = (0..info.len()).collect();
    let mut left = budget;
    let mut cut = Vec::new();
    let pages = only
        .unwrap_or(&all)
        .iter()
        .filter_map(|&i| Some((i, info.get(i)?)))
        .map(|(i, b)| {
            let crop_width = f64::from(b.crop[2] - b.crop[0]);
            let display_width = f64::from(if b.rotation % 180 == 0 { b.width } else { b.height });
            let unit = if crop_width > 0.0 && crop_width.is_finite() && display_width.is_finite() { display_width / crop_width } else { 1.0 };
            let shown = |p: &[f64; 2]| {
                let [x, y] = b.user_to_view(p[0] as f32, p[1] as f32);
                [f64::from(x), f64::from(b.height - y)]
            };
            if left == 0 {
                cut.push(i);
                return Vec::new();
            }
            let read = pdfcraft_edit::reading_shapes(cos, i).unwrap_or_default();
            let mut kept = Vec::new();
            let mut full = !read.truncated;
            for s in read.shapes {
                let n: usize = s.paths.iter().map(Vec::len).sum();
                if n > left {
                    full = false;
                    left = 0;
                    break;
                }
                left -= n;
                kept.push(pdfcraft_export::Shape {
                    paths: s.paths.iter().map(|sp| sp.iter().map(shown).collect()).collect(),
                    closed: s.closed,
                    fill: s.fill,
                    stroke: s.stroke,
                    line_width: s.line_width * unit,
                });
            }
            if !full {
                cut.push(i);
            }
            kept
        })
        .collect();
    (pages, cut)
}

/// `pages` as a file of `target`'s format, titled `title`.
pub fn write(pages: &[pdfcraft_export::Page], target: Target, title: &str) -> Vec<u8> {
    match target {
        Target::Docx => pdfcraft_export::docx(pages, title),
        Target::Pptx => pdfcraft_export::pptx(pages, title),
        Target::PptxPages => pdfcraft_export::pptx_positioned(pages, title),
        Target::Html => pdfcraft_export::html(pages, title).into_bytes(),
        Target::Rtf => pdfcraft_export::rtf(pages).into_bytes(),
    }
}

/// Convert the PDF in `bytes` to `target`. Pages that can't be read lose their text or images
/// rather than failing the whole file; a file that can't be opened at all is an [`Error`].
pub fn convert(bytes: &[u8], target: Target, opts: &Options) -> Result<Converted, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::TooLarge);
    }
    if !is_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    // The last-resort guard (AGENTS.md §4): a bug on one hostile file is an error, not a crash
    // of the app that asked.
    guarded(|| convert_unguarded(bytes, target, opts, None))
}

/// [`convert`], with the words of scanned pages (pictures without text) recognised by `ocr` and
/// set as text in place of the picture. Pages it reads nothing on stay pictures.
#[cfg(feature = "ocr")]
pub fn convert_with_ocr(bytes: &[u8], target: Target, opts: &Options, ocr: &pdfcraft_ocr::Ocr) -> Result<Converted, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::TooLarge);
    }
    if !is_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    guarded(|| convert_unguarded(bytes, target, opts, Some(&|im: &pdfcraft_export::Image| ocr::recognize(ocr, im))))
}

/// Run `f`, turning a panic into [`Error::Damaged`].
fn guarded(f: impl FnOnce() -> Result<Converted, Error>) -> Result<Converted, Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let why = p.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| p.downcast_ref::<String>().cloned());
        Err(Error::Damaged(why.unwrap_or_else(|| "unknown error".into())))
    })
}

/// How many pages the PDF in `bytes` has, without converting it (to check a page range first).
pub fn page_count(bytes: &[u8], password: Option<&str>) -> Result<usize, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::TooLarge);
    }
    if !is_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(bytes, password).map(|cos| pdfcraft_model::pages(&cos).len())))
        .unwrap_or_else(|_| Err(Error::Damaged("the page tree could not be read".into())))
}

fn open(bytes: &[u8], password: Option<&str>) -> Result<pdfcraft_cos::Document, Error> {
    pdfcraft_cos::Document::open_with_password(Arc::new(bytes.to_vec()), password).map_err(|e| match e {
        pdfcraft_cos::CosError::NeedsPassword => Error::NeedsPassword,
        pdfcraft_cos::CosError::WrongPassword => Error::WrongPassword,
        other => Error::Damaged(other.to_string()),
    })
}

/// Recognises a scan's words: the picture in, its text as blocks on the page out.
type Recognize<'a> = &'a dyn Fn(&pdfcraft_export::Image) -> Vec<pdfcraft_export::Block>;

fn convert_unguarded(bytes: &[u8], target: Target, opts: &Options, recognize: Option<Recognize>) -> Result<Converted, Error> {
    let cos = open(bytes, opts.password.as_deref())?;
    let boxes = page_boxes(&cos);
    let page_count = boxes.len();
    if let Some(only) = &opts.pages
        && let Some(missing) = only.iter().find(|&&p| p >= page_count)
    {
        return Err(Error::BadPageRange(format!("page {} isn't in this PDF (pages 1–{page_count})", missing.saturating_add(1))));
    }
    let (mut pages, truncated_pages) = pages(&cos, &boxes, opts.pages.as_deref());
    let mut truncated_pages = truncated_pages;
    if target == Target::PptxPages {
        let (art, cut) = shapes(&cos, &boxes, opts.pages.as_deref());
        for (page, art) in pages.iter_mut().zip(art) {
            page.shapes = art;
        }
        truncated_pages.extend(cut);
        truncated_pages.sort_unstable();
        truncated_pages.dedup();
    }
    let indices: Vec<usize> = opts.pages.clone().unwrap_or_else(|| (0..page_count).collect());
    let mut ocr_pages = Vec::new();
    if let Some(recognize) = recognize {
        for (page, &i) in pages.iter_mut().zip(&indices) {
            if !page.blocks.is_empty() {
                continue;
            }
            // The scan is the page's biggest picture.
            let area = |im: &pdfcraft_export::Image| (im.rect[2] - im.rect[0]).abs() * (im.rect[3] - im.rect[1]).abs();
            let Some(at) = page.images.iter().enumerate().max_by(|(_, a), (_, b)| area(a).total_cmp(&area(b))).map(|(at, _)| at) else { continue };
            let blocks = page.images.get(at).map(recognize).unwrap_or_default();
            if !blocks.is_empty() {
                page.images.remove(at);
                page.blocks = blocks;
                ocr_pages.push(i);
            }
        }
    }
    let image_only_pages = pages.iter().zip(&indices).filter(|(p, _)| p.blocks.is_empty() && !p.images.is_empty()).map(|(_, &i)| i).collect();
    let title = opts.title.clone().or_else(|| info_title(&cos)).unwrap_or_default();
    Ok(Converted { bytes: write(&pages, target, &title), page_count, truncated_pages, image_only_pages, ocr_pages })
}

/// The document's `/Info` `/Title`, when it has a non-empty one.
fn info_title(cos: &pdfcraft_cos::Document) -> Option<String> {
    let info = cos.resolve(cos.trailer().get(b"Info")?);
    let title = cos.resolve(info.as_dict()?.get(b"Title")?);
    Some(title.as_string()?.to_text()).filter(|t| !t.trim().is_empty())
}

#[cfg(feature = "ocr")]
mod ocr;

#[cfg(test)]
mod tests;
