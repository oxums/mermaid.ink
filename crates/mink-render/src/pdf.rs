//! /pdf: Chrome `page.pdf()` of mermaid.html (zero margins, 8px body margin, printBackground).
//! Paper mode lays the svg out at the page width and paginates like Chrome does for an atomic
//! inline that is taller than the page: page 1 stays blank and the svg continues from the top
//! of page 2. Fit mode sizes the single page to the svg box plus the body margins.

use std::collections::HashMap;

use lopdf::{Document, Object};
use merman::OperationControl;
use merman::svg::ResvgCompatibleSvg;
use merman::svg::export::{PdfFilterImageLimit, PdfOptions, PdfPagePolicy, svg_to_pdf_controlled};
use mink_core::FetchedImage;

use crate::raster::{FIXED_OVERHEAD, TREE_BYTES_PER_SVG_BYTE, conversion_limits};
use crate::{
    CONTAINER_WIDTH, Engine, Prepared, RenderError, RenderJob, RenderOutput, map_export_error, svg,
};

const BODY_MARGIN: f64 = 8.0;
const PX_TO_PT: f64 = 0.75;
/// Pixels of rasterized filter regions (drop shadows) per page; merman lowers the filter
/// resolution to stay within it. merman's default is 32M.
const FILTER_PIXELS: u64 = 8 << 20;
/// Filter regions (drop shadows) are rasterized at 1 image px per CSS px: merman's default 4
/// made T12 (neo state diagram) take 3 s instead of 0.5 s; a shadow is a blur anyway.
const FILTER_SCALE: f32 = 1.0;
/// krilla's encoder state besides the output.
const ENCODER_OVERHEAD: u64 = 16 << 20;

/// Portrait page size in points, exactly as Chrome wrote the MediaBox in the goldens.
fn paper_pt(paper: &str) -> (f64, f64) {
    match paper {
        "letter" => (612.0, 792.0),
        "legal" => (612.0, 1008.0),
        "tabloid" => (792.0, 1224.0),
        "ledger" => (1224.0, 792.0),
        "a0" => (2383.92, 3371.04),
        "a1" => (1684.08, 2383.92),
        "a2" => (1191.12, 1684.08),
        "a3" => (841.92, 1191.12),
        "a5" => (420.0, 595.92),
        "a6" => (298.08, 420.0),
        _ => (595.92, 841.92),
    }
}

/// Page size (px) and, per page, the svg box's top-left offset (px) on that page.
pub(crate) struct Layout {
    pub page: (f64, f64),
    pub size: (f64, f64),
    pub offsets: Vec<(f64, f64)>,
}

pub(crate) fn layout(job: &RenderJob, rb: &svg::RootBox) -> Layout {
    if job.fit {
        // pdf.js: width = ceil(clip.width) + clip.x * 2, measured in the 1920px viewport.
        let size = rb.display_size(CONTAINER_WIDTH, job.width, job.height);
        let page = (
            size.0.ceil() + 2.0 * BODY_MARGIN,
            size.1.ceil() + 2.0 * BODY_MARGIN,
        );
        return Layout {
            page,
            size,
            offsets: vec![(BODY_MARGIN, BODY_MARGIN)],
        };
    }
    let (w, h) = paper_pt(&job.paper);
    let (w, h) = if job.landscape { (h, w) } else { (w, h) };
    let page = (w / PX_TO_PT, h / PX_TO_PT);
    let size = rb.display_size(page.0 - 2.0 * BODY_MARGIN, job.width, job.height);
    let offsets = if BODY_MARGIN + size.1 <= page.1 {
        vec![(BODY_MARGIN, BODY_MARGIN)]
    } else {
        let pages = 1 + (size.1 / page.1).ceil() as usize;
        (0..pages)
            .map(|i| (BODY_MARGIN, page.1 * (1.0 - i as f64)))
            .collect()
    };
    Layout {
        page,
        size,
        offsets,
    }
}

pub(crate) struct Stage {
    pages: Vec<ResvgCompatibleSvg>,
    page: (f64, f64),
    bg: Option<String>,
}

pub(crate) fn prepare(
    engine: &Engine,
    raw: &str,
    native: bool,
    job: &RenderJob,
    images: &HashMap<String, FetchedImage>,
    control: &OperationControl,
) -> Result<Prepared, RenderError> {
    let doc = svg::parse(raw)?;
    let rb = svg::root_box(&doc);
    let Layout {
        page,
        size,
        offsets,
    } = layout(job, &rb);
    let mut pages = Vec::with_capacity(offsets.len());
    let mut image_bytes = 0;
    for offset in offsets {
        let view_box = svg::canvas_view_box(&rb, size, offset, page);
        let (sealed, images) = engine.export_svg(&doc, native, page, view_box, images, control)?;
        image_bytes += images;
        pages.push(sealed);
    }
    // Held: every sealed page, and every page's PDF until they are merged (lopdf then parses
    // them all and writes the result). Peak of one page: its usvg tree, filter rasters, krilla.
    // A page's PDF holds its vector content (< 2 bytes per SVG byte), images and filter images.
    // Every page draws the same diagram, so they share filters.
    let n = pages.len() as u64;
    let lens = || pages.iter().map(|p| p.as_ref().len() as u64);
    let sealed: u64 = lens().sum();
    let filter_pixels = if pages.iter().any(|p| p.as_ref().contains("<filter")) {
        FILTER_PIXELS
    } else {
        0
    };
    let page_pdfs = 2 * sealed + n * filter_pixels * 4 + image_bytes;
    let merge = if n > 1 { 4 } else { 1 };
    let memory = sealed
        + lens().max().unwrap_or(0) * TREE_BYTES_PER_SVG_BYTE
        + filter_pixels * 8
        + image_bytes
        + page_pdfs * merge
        + ENCODER_OVERHEAD
        + FIXED_OVERHEAD;
    Ok(Prepared {
        stage: crate::Stage::Pdf(Stage {
            pages,
            page,
            bg: job.bg_color.as_deref().and_then(svg::hex_color),
        }),
        memory,
    })
}

pub(crate) fn finish(
    engine: &Engine,
    stage: Stage,
    control: &OperationControl,
) -> Result<RenderOutput, RenderError> {
    let mut options = PdfOptions::default()
        .with_font_context(engine.fonts.for_operation(control))
        .with_page_policy(PdfPagePolicy::Fixed {
            width_pt: (stage.page.0 * PX_TO_PT) as f32,
            height_pt: (stage.page.1 * PX_TO_PT) as f32,
        })
        .with_filter_image_limit(PdfFilterImageLimit::new(Some(FILTER_PIXELS)))
        .with_filter_scale(FILTER_SCALE)
        .with_conversion_limits(conversion_limits());
    // printBackground: the body background fills every page.
    if let Some(bg) = stage.bg {
        options = options.with_page_paint(bg);
    }
    let pages = stage
        .pages
        .iter()
        .map(|sealed| {
            svg_to_pdf_controlled(sealed, &options, control.clone()).map_err(map_export_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    drop(stage.pages);
    let body = match <[_; 1]>::try_from(pages) {
        Ok([single]) => single,
        Err(pages) => {
            merge(&pages).map_err(|e| RenderError::Internal(format!("pdf merge: {e}")))?
        }
    };
    Ok(RenderOutput {
        body,
        mime: "application/pdf",
    })
}

/// Appends the page of every later single-page PDF to the first one's page tree.
fn merge(pdfs: &[Vec<u8>]) -> Result<Vec<u8>, lopdf::Error> {
    let mut base = Document::load_mem(&pdfs[0])?;
    let pages_id = base.catalog()?.get(b"Pages")?.as_reference()?;
    let mut kids = Vec::new();
    for bytes in &pdfs[1..] {
        let mut doc = Document::load_mem(bytes)?;
        doc.renumber_objects_with(base.max_id + 1);
        base.max_id = doc.max_id;
        let skip = doc.catalog()?.get(b"Pages")?.as_reference()?;
        let catalog = doc.trailer.get(b"Root")?.as_reference()?;
        for page in doc.get_pages().into_values() {
            doc.get_dictionary_mut(page)?.set("Parent", pages_id);
            kids.push(Object::Reference(page));
        }
        base.objects.extend(
            doc.objects
                .into_iter()
                .filter(|(id, _)| *id != skip && *id != catalog),
        );
    }
    let tree = base.get_dictionary_mut(pages_id)?;
    let count = tree.get(b"Count")?.as_i64()? + kids.len() as i64;
    tree.get_mut(b"Kids")?.as_array_mut()?.extend(kids);
    tree.set("Count", count);
    let mut out = Vec::new();
    base.save_to(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(paper: &str, landscape: bool, fit: bool) -> RenderJob {
        RenderJob {
            code: String::new(),
            config: serde_json::Value::Null,
            format: crate::OutputFormat::Pdf,
            bg_color: None,
            width: None,
            height: None,
            paper: paper.into(),
            landscape,
            fit,
            fa_css_url: String::new(),
        }
    }

    fn root(view_box: &str, max_width: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" style="max-width: {max_width}px;" viewBox="{view_box}"/>"#
        )
    }

    /// Page counts recorded in tests/fixtures/golden (T08 is 880x1049, T12 704x1856).
    #[test]
    fn paginates_like_chrome() {
        let t08 = root("-50 -10 880 1049", "880");
        let t12 = root("4 4 704.203125 1856", "704.203125");
        let cases = [
            (&t08, "a4", false, 1),
            (&t08, "a4", true, 3),
            (&t08, "ledger", false, 2),
            (&t08, "letter", true, 3),
            (&t08, "a3", true, 1),
            (&t12, "a4", false, 3),
            (&t12, "a4", true, 4),
            (&t12, "a2", false, 1),
            (&t12, "ledger", false, 3),
        ];
        for (svg, paper, landscape, pages) in cases {
            let doc = roxmltree::Document::parse(svg).unwrap();
            let l = layout(&job(paper, landscape, false), &svg::root_box(&doc));
            assert_eq!(l.offsets.len(), pages, "{paper} landscape={landscape}");
        }
    }

    #[test]
    fn fit_page_matches_legacy() {
        let svg = root("4 4 552 588", "552");
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let l = layout(&job("a3", true, true), &svg::root_box(&doc));
        assert_eq!((l.page.0 * PX_TO_PT, l.page.1 * PX_TO_PT), (426.0, 453.0)); // golden 426 x 452.88
    }
}
