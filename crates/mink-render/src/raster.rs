//! /img: Puppeteer's element screenshot of `#container > svg` (1920px viewport, 8px body margin,
//! omitBackground), reproduced by sizing the canvas to the svg's CSS box.

use std::collections::HashMap;

use merman::OperationControl;
use merman::svg::ResvgCompatibleSvg;
use merman::svg::export::{
    RasterOptions, RasterSizeLimit, SvgConversionLimits, svg_to_jpeg_controlled,
    svg_to_png_controlled,
};
use mink_core::FetchedImage;

use crate::{
    CONTAINER_WIDTH, Engine, OutputFormat, Prepared, RenderError, RenderJob, RenderOutput,
    map_export_error, svg,
};

/// libwebp's WEBP_MAX_DIMENSION.
const WEBP_MAX_SIDE: f64 = 16383.0;
/// usvg tree (text already outlined) and resvg working state per byte of sealed SVG.
/// shortcut: an estimate from the tree's shape, not a measured bound; upgrade if a profile
/// shows a heavier tree.
pub(crate) const TREE_BYTES_PER_SVG_BYTE: u64 = 16;
/// Font caches, backend stack and allocator slack.
pub(crate) const FIXED_OVERHEAD: u64 = 8 << 20;
/// libwebp method (0 fastest .. 6 smallest). WebPEncodeRGBA uses 4; on the bench corpus 2 is
/// 10-30% faster to encode for 3-9% larger files.
const WEBP_METHOD: i32 = 2;

/// Drop shadows (Mermaid's neo look) put one filter on each node; merman's default total of
/// 128 primitives would turn a large neo diagram into an error. Every filter region is bounded
/// by the canvas, which the pixel limit already caps.
pub(crate) fn conversion_limits() -> SvgConversionLimits {
    SvgConversionLimits {
        max_total_filter_primitives: Some(4096),
        ..SvgConversionLimits::default()
    }
}

pub(crate) struct Stage {
    sealed: ResvgCompatibleSvg,
    format: OutputFormat,
    matte: Option<String>,
}

pub(crate) fn prepare(
    engine: &Engine,
    raw: &str,
    native: bool,
    job: &RenderJob,
    format: OutputFormat,
    images: &HashMap<String, FetchedImage>,
    control: &OperationControl,
) -> Result<Prepared, RenderError> {
    let doc = svg::parse(raw)?;
    let rb = svg::root_box(&doc);
    let size = rb.display_size(CONTAINER_WIDTH, job.width, job.height);
    // Puppeteer rounds the element's clip rectangle.
    let canvas = (size.0.round().max(1.0), size.1.round().max(1.0));
    if canvas.0 * canvas.1 > engine.max_raster_pixels as f64 {
        return Err(RenderError::BadRequest(format!(
            "image of {}x{} pixels exceeds the {} pixel limit",
            canvas.0, canvas.1, engine.max_raster_pixels
        )));
    }
    if format == OutputFormat::Webp && canvas.0.max(canvas.1) > WEBP_MAX_SIDE {
        return Err(RenderError::BadRequest(format!(
            "WebP images are limited to {WEBP_MAX_SIDE} pixels per side"
        )));
    }
    let view_box = svg::canvas_view_box(&rb, size, (0.0, 0.0), canvas);
    let (sealed, image_bytes) =
        engine.export_svg(&doc, native, canvas, view_box, images, control)?;

    let bg = job.bg_color.as_deref().and_then(svg::hex_color);
    let matte = if format == OutputFormat::Jpeg {
        // Transparent areas: the reference flattened them to white or black at random (see
        // tools/golden/README.md); white is the majority and merman's JPEG default.
        Some(bg.map_or("#ffffff".into(), |c| svg::opaque_on_white(&c)))
    } else {
        bg
    };
    // Peak bytes per pixel: PNG = pixmap + tiny-skia's demultiplied copy + output;
    // JPEG = pixmap + RGB copy + output; WebP = the PNG step, then PNG + RGBA + libwebp's YUVA
    // picture + its output and our copy of it.
    let per_pixel = match format {
        OutputFormat::Webp => 20,
        _ => 12,
    };
    let pixels = canvas.0 as u64 * canvas.1 as u64;
    let svg_len = sealed.as_ref().len() as u64;
    let memory =
        svg_len * (1 + TREE_BYTES_PER_SVG_BYTE) + image_bytes + pixels * per_pixel + FIXED_OVERHEAD;
    Ok(Prepared {
        stage: crate::Stage::Raster(Box::new(Stage {
            sealed,
            format,
            matte,
        })),
        memory,
    })
}

pub(crate) fn finish(
    engine: &Engine,
    stage: Stage,
    control: &OperationControl,
) -> Result<RenderOutput, RenderError> {
    let mut options = RasterOptions::default()
        .with_font_context(engine.fonts.for_operation(control))
        .with_size_limit(RasterSizeLimit::new(
            None,
            None,
            Some(engine.max_raster_pixels),
        ))
        .with_conversion_limits(conversion_limits());
    if let Some(matte) = stage.matte {
        options = options.with_matte(matte);
    }
    let sealed = &stage.sealed;
    let png = |o: &RasterOptions| {
        svg_to_png_controlled(sealed, o, control.clone()).map_err(map_export_error)
    };
    let (body, mime) = match stage.format {
        OutputFormat::Png => (png(&options)?, "image/png"),
        OutputFormat::Jpeg => (
            svg_to_jpeg_controlled(sealed, &options, control.clone()).map_err(map_export_error)?,
            "image/jpeg",
        ),
        _ => (webp_from_png(&png(&options)?)?, "image/webp"),
    };
    Ok(RenderOutput { body, mime })
}

/// shortcut: merman-export 0.10 exposes no RGBA pixmap (PreparedRaster renders straight into
/// its PNG/JPEG encoders, and tiny-skia's PNG compression level is fixed), so WebP decodes its
/// PNG first; upgrade when merman-export gains a public RGBA accessor.
fn webp_from_png(png_bytes: &[u8]) -> Result<Vec<u8>, RenderError> {
    let internal = |e: String| RenderError::Internal(format!("webp: {e}"));
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|e| internal(e.to_string()))?;
    let mut rgba = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or_else(|| internal("size".into()))?
    ];
    let info = reader
        .next_frame(&mut rgba)
        .map_err(|e| internal(e.to_string()))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(internal(format!(
            "unexpected PNG layout {:?}",
            info.color_type
        )));
    }
    encode_webp(&rgba[..info.buffer_size()], info.width, info.height)
        .ok_or_else(|| internal("encode failed".into()))
}

/// Lossy WebP at quality 90, like Puppeteer's `quality: 90`: WebPEncodeRGBA's config with a
/// faster method (`WEBP_METHOD`).
fn encode_webp(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    use libwebp_sys as w;
    let stride = width.checked_mul(4)?;
    if rgba.len() != stride as usize * height as usize {
        return None;
    }
    let mut config =
        w::WebPConfig::new_with_preset(w::WebPPreset::WEBP_PRESET_DEFAULT, 90.0).ok()?;
    config.method = WEBP_METHOD;
    let mut picture = w::WebPPicture::new().ok()?;
    picture.width = i32::try_from(width).ok()?;
    picture.height = i32::try_from(height).ok()?;
    let mut writer = std::mem::MaybeUninit::<w::WebPMemoryWriter>::uninit();
    // SAFETY: `config` and `picture` come from libwebp's initializers; `rgba` holds `height`
    // rows of `stride` bytes (checked above) and outlives the import, which copies it. The
    // writer is initialized before use, and the picture and writer buffers are released exactly
    // once on every path after the output is copied.
    unsafe {
        if w::WebPValidateConfig(&config) == 0 {
            return None;
        }
        w::WebPMemoryWriterInit(writer.as_mut_ptr());
        let mut writer = writer.assume_init();
        if w::WebPPictureImportRGBA(&mut picture, rgba.as_ptr(), i32::try_from(stride).ok()?) == 0 {
            w::WebPPictureFree(&mut picture);
            return None;
        }
        picture.writer = Some(w::WebPMemoryWrite);
        picture.custom_ptr = (&raw mut writer).cast();
        let ok = w::WebPEncode(&config, &mut picture) != 0;
        w::WebPPictureFree(&mut picture);
        let bytes = (ok && writer.size > 0)
            .then(|| std::slice::from_raw_parts(writer.mem, writer.size).to_vec());
        w::WebPMemoryWriterClear(&mut writer);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_round_trip() {
        let rgba: Vec<u8> = (0..16 * 8).flat_map(|i| [i as u8, 0, 255, 128]).collect();
        let webp = encode_webp(&rgba, 16, 8).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        assert_eq!(encode_webp(&rgba, 17, 8), None);
    }
}
