# pdfcraft-office

PDF ▸ Word (.docx), PowerPoint (.pptx), HTML or RTF, as a library: the way the other Craft apps
open a PDF. WordCraft reads the .docx it returns, DeckCraft the .pptx (one slide per page).

- **Layer:** L5. Depends on `cos`, `model` (page geometry), `edit` (reading-order
  paragraphs and images), `create` (image files) and `export` (the writers). No UI, wasm-clean.
- **One reduction.** [`pages`] is what PdfCraft's own Export a PDF uses
  (`Document::export_office_checked`), so the apps that open PDFs and PdfCraft's export cannot
  drift apart.

```rust
let out = pdfcraft_office::convert(&bytes, Target::Docx, &Options { pages: Some(vec![0, 2]), ..Default::default() })?;
// out.bytes: the .docx; out.page_count; out.truncated_pages; out.image_only_pages (scans)
```

- **Errors:** `NotPdf`, `NeedsPassword`, `WrongPassword`, `TooLarge` (over 512 MiB, refused before
  parsing), `BadPageRange`, `Damaged`. A page that can't be read loses its text or images; the rest
  of the file still converts. The whole conversion runs under a last-resort panic guard.
- `parse_page_range("1-3, 5", count)` reads page ranges as people type them.

## Scanned pages (feature `ocr`)

With the `ocr` feature, `convert_with_ocr(&bytes, target, &opts, &ocr)` reads the words of pages
that are pictures without text (scans) with `pdfcraft-ocr` and sets them as text in place of the
picture: lines at a line pitch become a paragraph, and each paragraph's type size comes from its
word boxes. `Converted::ocr_pages` lists the pages read this way; `image_only_pages` keeps the
scans it couldn't read. The picture itself is decoded (JPEG or PNG, at most 60 megapixels), so no
renderer is needed. The models are separate files (`pdfcraft_ocr::Models`; `cargo xtask models`
in a source tree) and read the Latin alphabet without accents; the feature is off by default and
in wasm builds.
