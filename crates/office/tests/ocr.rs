//! Text recognition of scanned pages (feature `ocr`). The models come from `cargo xtask models`;
//! without them the tests say so and pass.

#![cfg(feature = "ocr")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pdfcraft_office::{Options, Target, convert, convert_with_ocr};

/// A one-page PDF whose page is a picture of text: `tests/data/scan.jpg` (a WordCraft page
/// rendered and saved as a greyscale JPEG; see ATTRIBUTION.toml) at 144 dpi.
fn scan_pdf() -> Vec<u8> {
    let jpg = include_bytes!("data/scan.jpg").to_vec();
    let doc = pdfcraft_create::from_images_with_resolution(&[("scan.jpg".into(), jpg)], pdfcraft_create::ImageResolution::Dpi(144.0)).unwrap();
    pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions::default()).unwrap()
}

fn ocr() -> Option<pdfcraft_ocr::Ocr> {
    match pdfcraft_ocr::Ocr::find() {
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("skipped: {e} (run `cargo xtask models`)");
            None
        }
    }
}

fn entry(bytes: &[u8], name: &str) -> String {
    use std::io::Read;
    let mut at = 0;
    while let Some(h) = bytes.get(at..at + 30) {
        let le16 = |i: usize| usize::from(u16::from_le_bytes([h[i], h[i + 1]]));
        let size = u32::from_le_bytes([h[18], h[19], h[20], h[21]]) as usize;
        let (method, name_len, extra) = (le16(8), le16(26), le16(28));
        let start = at + 30 + name_len + extra;
        if &bytes[at + 30..at + 30 + name_len] == name.as_bytes() {
            let mut out = String::new();
            let data = &bytes[start..start + size];
            if method == 8 {
                flate2::read::DeflateDecoder::new(data).read_to_string(&mut out).unwrap();
            } else {
                out = String::from_utf8_lossy(data).into_owned();
            }
            return out;
        }
        at = start + size;
    }
    String::new()
}

#[test]
fn without_recognition_a_scan_is_reported() {
    let out = convert(&scan_pdf(), Target::Docx, &Options::default()).unwrap();
    assert_eq!(out.image_only_pages, vec![0]);
    assert!(out.ocr_pages.is_empty());
}

#[test]
fn a_scanned_page_becomes_text() {
    let Some(ocr) = ocr() else { return };
    let out = convert_with_ocr(&scan_pdf(), Target::Docx, &Options::default(), &ocr).unwrap();
    assert_eq!(out.ocr_pages, vec![0]);
    assert!(out.image_only_pages.is_empty(), "a recognised page is no longer a scan without text");
    let xml = entry(&out.bytes, "word/document.xml");
    if std::env::var_os("OCR_SHOW").is_some() {
        println!("{xml}");
    }
    for word in ["HELLO", "WORLD", "quick", "brown", "lazy", "recognition"] {
        assert!(xml.contains(word), "{word}: {xml}");
    }
    // The picture gives way to the text it showed.
    assert!(!xml.contains("<w:drawing>"), "{xml}");
    // Each of the scan's paragraphs is one: the two sentences, a paragraph apart, stay apart.
    let paras: Vec<&str> = xml.split("<w:p>").skip(1).collect();
    assert!(paras.iter().any(|p| p.contains("HELLO") && !p.contains("quick")), "{xml}");
    assert!(!paras.iter().any(|p| p.contains("lazy") && p.contains("Scanned")), "{xml}");
    // The heading is set larger than the body: about 20 pt and 12 pt in the WordCraft page.
    let size = |word: &str| -> f64 {
        let p = paras.iter().find(|p| p.contains(word)).copied().unwrap_or("");
        p.split("w:sz w:val=\"").nth(1).and_then(|v| v.split('"').next()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 2.0
    };
    assert!((17.0..=23.0).contains(&size("HELLO")), "{}", size("HELLO"));
    assert!((10.0..=14.5).contains(&size("quick")), "{}", size("quick"));
}

#[test]
fn slides_get_recognised_text_where_it_was() {
    let Some(ocr) = ocr() else { return };
    let out = convert_with_ocr(&scan_pdf(), Target::PptxPages, &Options::default(), &ocr).unwrap();
    assert_eq!(out.ocr_pages, vec![0]);
    let slide = entry(&out.bytes, "ppt/slides/slide1.xml");
    assert!(slide.contains("HELLO") && slide.contains("lazy"), "{slide}");
}
