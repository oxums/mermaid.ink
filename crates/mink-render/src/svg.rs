//! String-level post-processing of merman's SVG: security stripping, the legacy /svg root
//! rewrite, Chrome-like display sizing, FontAwesome glyph substitution and image inlining.
//! Every edit is a byte-range splice located with roxmltree, so the rest of the document is
//! passed through untouched.

use std::collections::HashMap;
use std::ops::Range;

use mink_core::FetchedImage;
use roxmltree::{Document, Node};

use crate::RenderError;
use crate::coverage::FontCoverage;
use crate::fa::FaIcons;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// 400 body when merman emits XML that does not parse: user label markup (e.g. an unclosed
/// `<img>`) passed through. The parser text quotes that markup, so it is not reported.
pub(crate) const BAD_MARKUP: &str = "Diagram labels contain markup that cannot be rendered";

/// Decoded bytes of remote images (counting every occurrence) one export may inline: merman's
/// own embedded-image total, checked here before any data URL is built.
const MAX_INLINE_IMAGE_BYTES: u64 = merman::svg::export::DEFAULT_MAX_TOTAL_EMBEDDED_IMAGE_BYTES;

pub(crate) fn parse(svg: &str) -> Result<Document<'_>, RenderError> {
    Document::parse(svg).map_err(|_| RenderError::BadRequest(BAD_MARKUP.into()))
}

#[derive(Default)]
struct Edits(Vec<(Range<usize>, String)>);

impl Edits {
    fn replace(&mut self, range: Range<usize>, with: impl Into<String>) {
        self.0.push((range, with.into()));
    }

    /// Removes an attribute together with the whitespace that separates it from the previous one.
    fn remove_attr(&mut self, src: &str, range: Range<usize>) {
        let start = src[..range.start].trim_end().len();
        self.0.push((start..range.end, String::new()));
    }

    fn apply(mut self, src: &str) -> String {
        // Nested ranges (an attribute inside a removed element) collapse into the outer edit.
        self.0
            .sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
        let mut out = String::with_capacity(src.len() + 256);
        let mut at = 0;
        for (range, with) in self.0 {
            if range.start < at {
                continue;
            }
            out.push_str(&src[at..range.start]);
            out.push_str(&with);
            at = range.end;
        }
        out.push_str(&src[at..]);
        out
    }
}

/// Strips script vectors: `on*` attributes, script-like elements, and `javascript:`/`vbscript:`/
/// `data:text/html` URLs. Mermaid's DOMPurify does most of this in the browser; merman lets
/// some of it through (e.g. ER labels), and /svg is served from our origin.
fn sanitize(doc: &Document, src: &str, edits: &mut Edits) {
    for node in doc.descendants().filter(Node::is_element) {
        let name = node.tag_name().name().to_ascii_lowercase();
        let animates_dangerous = matches!(name.as_str(), "set" | "animate")
            && node.attribute("attributeName").is_some_and(|a| {
                let a = a.to_ascii_lowercase();
                a.starts_with("on") || a.ends_with("href")
            });
        if matches!(name.as_str(), "script" | "iframe" | "object" | "embed") || animates_dangerous {
            edits.replace(node.range(), "");
            continue;
        }
        for attr in node.attributes() {
            let local = attr.name().to_ascii_lowercase();
            let bad_url = matches!(local.as_str(), "href" | "src" | "action" | "formaction")
                && is_script_url(attr.value());
            if local.starts_with("on") || bad_url {
                edits.remove_attr(src, attr.range());
            }
        }
    }
}

fn is_script_url(value: &str) -> bool {
    // Browsers ignore ASCII whitespace/control characters inside the scheme.
    let scheme: String = value
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control())
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    scheme.starts_with("javascript:")
        || scheme.starts_with("vbscript:")
        || scheme.starts_with("data:text/html")
}

/// The legacy /svg body: sanitized, `xmlns:xlink`, FontAwesome `@import` as first child,
/// bgColor and setSize applied to the root like mermaid.mjs + svg.js do in Chrome.
pub(crate) fn finish_svg(
    src: &str,
    bg: Option<&str>,
    width: Option<f64>,
    height: Option<f64>,
    fa_css_url: &str,
) -> Result<String, RenderError> {
    let doc = parse(src)?;
    let root = doc.root_element();
    let mut edits = Edits::default();
    sanitize(&doc, src, &mut edits);

    let sized = width.is_some() || height.is_some();
    let mut style: Vec<String> = root
        .attribute("style")
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty() && !(sized && d.starts_with("max-width")))
        .map(String::from)
        .collect();
    if sized {
        style.insert(0, "max-width: 100%".into());
    }
    if let Some(bg) = bg.and_then(css_color) {
        style.retain(|d| !d.starts_with("background-color"));
        style.push(format!("background-color: {bg}"));
    }
    let style = style
        .iter()
        .map(|d| format!("{d};"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut tail = String::new();
    match root.attributes().find(|a| a.name() == "style") {
        Some(a) => edits.replace(a.range_value(), escape_attr(&style)),
        None if !style.is_empty() => tail.push_str(&format!(r#" style="{}""#, escape_attr(&style))),
        None => {}
    }
    // setSize: removeAttribute('width'), then setAttribute, which edits an existing height in place.
    let attr = |name| {
        root.attributes()
            .find(|a| a.namespace().is_none() && a.name() == name)
    };
    if let Some(a) = attr("width").filter(|_| sized) {
        edits.remove_attr(src, a.range());
    }
    if let Some(w) = width {
        tail.push_str(&format!(r#" width="{w}px""#));
    }
    match (height, attr("height")) {
        (Some(h), Some(a)) => edits.replace(a.range_value(), format!("{h}px")),
        (Some(h), None) => tail.push_str(&format!(r#" height="{h}px""#)),
        (None, _) => {}
    }
    if root.lookup_prefix(XLINK_NS).is_none() {
        tail.push_str(&format!(r#" xmlns:xlink="{XLINK_NS}""#));
    }
    let tag_end = start_tag_end(src, root);
    edits.replace(
        tag_end - 1..tag_end,
        format!(
            r#"{tail}><style xmlns="http://www.w3.org/1999/xhtml">@import url("{}");</style>"#,
            escape_text(fa_css_url)
        ),
    );
    Ok(edits.apply(src))
}

/// Byte offset just past the root start tag's `>`.
fn start_tag_end(src: &str, root: Node) -> usize {
    let after_attrs = root
        .attributes()
        .map(|a| a.range().end)
        .max()
        .unwrap_or(root.range().start);
    after_attrs + src[after_attrs..].find('>').map_or(0, |i| i + 1)
}

/// The root's viewBox and CSS size inputs, as Chrome would lay the inline `<svg>` out.
pub(crate) struct RootBox {
    pub view_box: Option<[f64; 4]>,
    width: Length,
    height: Length,
    max_width: Option<f64>,
    /// preserveAspectRatio: x/y share of the slack (0, 0.5, 1), or None for `none`; and `slice`.
    align: Option<(f64, f64)>,
    slice: bool,
}

#[derive(Clone, Copy)]
enum Length {
    Auto,
    Percent(f64),
    Px(f64),
}

fn length(v: Option<&str>) -> Length {
    let Some(v) = v.map(str::trim) else {
        return Length::Auto;
    };
    if let Some(p) = v.strip_suffix('%') {
        return p.trim().parse().map_or(Length::Auto, Length::Percent);
    }
    v.strip_suffix("px")
        .unwrap_or(v)
        .trim()
        .parse()
        .map_or(Length::Auto, Length::Px)
}

pub(crate) fn root_box(doc: &Document) -> RootBox {
    let root = doc.root_element();
    let view_box = root.attribute("viewBox").and_then(|v| {
        let n: Vec<f64> = v
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| [n[0], n[1], n[2], n[3]])
    });
    let max_width = root.attribute("style").and_then(|s| {
        s.split(';').find_map(|d| {
            let (k, v) = d.split_once(':')?;
            (k.trim() == "max-width").then(|| v.trim().strip_suffix("px")?.trim().parse().ok())?
        })
    });
    let (align, slice) = aspect_ratio(root.attribute("preserveAspectRatio").unwrap_or(""));
    RootBox {
        view_box,
        width: length(root.attribute("width")),
        height: length(root.attribute("height")),
        max_width,
        align,
        slice,
    }
}

/// `<align> [meet|slice]`; anything unparseable is the default `xMidYMid meet`.
fn aspect_ratio(v: &str) -> (Option<(f64, f64)>, bool) {
    let mut words = v.split_whitespace();
    let align = words.next().unwrap_or("");
    let slice = words.next() == Some("slice");
    if align == "none" {
        return (None, false);
    }
    let share = |s: Option<&str>| match s? {
        "Min" => Some(0.0),
        "Mid" => Some(0.5),
        "Max" => Some(1.0),
        _ => None,
    };
    let parsed = align
        .strip_prefix('x')
        .and_then(|a| Some((share(a.get(..3))?, share(a.get(3..)?.strip_prefix('Y'))?)));
    (
        Some(parsed.unwrap_or((0.5, 0.5))),
        slice && parsed.is_some(),
    )
}

impl RootBox {
    /// CSS size of the inline svg inside a container `container` px wide. An explicit job size
    /// replaces the width attribute and drops max-width (legacy setSize + `maxWidth = null`).
    pub(crate) fn display_size(
        &self,
        container: f64,
        job_w: Option<f64>,
        job_h: Option<f64>,
    ) -> (f64, f64) {
        let sized = job_w.is_some() || job_h.is_some();
        let (attr_w, attr_h, max_w) = if sized {
            let h = job_h.map_or(self.height, Length::Px);
            (job_w.map_or(Length::Auto, Length::Px), h, None)
        } else {
            (self.width, self.height, self.max_width)
        };
        let ratio = self.view_box.map(|v| v[2] / v[3]);
        let w = match (attr_w, attr_h, ratio) {
            (Length::Px(w), _, _) => w,
            (Length::Percent(p), _, _) => container * p / 100.0,
            (Length::Auto, Length::Px(h), Some(r)) => h * r,
            _ => container,
        };
        let w = max_w.map_or(w, |m| w.min(m));
        let h = match (attr_h, ratio) {
            (Length::Px(h), _) => h,
            (_, Some(r)) => w / r,
            _ => 150.0, // CSS default height of a replaced element without a ratio
        };
        (w, h)
    }
}

/// Root attribute values for drawing the diagram, displayed at `size` px with its top-left
/// at `offset` px, onto a `canvas` px viewport (Chrome's placement for the root's
/// `preserveAspectRatio`; the exported root then uses `none`).
pub(crate) fn canvas_view_box(
    rb: &RootBox,
    size: (f64, f64),
    offset: (f64, f64),
    canvas: (f64, f64),
) -> [f64; 4] {
    let [vx, vy, vw, vh] = rb.view_box.unwrap_or([0.0, 0.0, size.0, size.1]);
    let (sx, sy) = (size.0 / vw, size.1 / vh);
    let ((sx, sy), (ax, ay)) = match rb.align {
        None => ((sx, sy), (0.0, 0.0)),
        Some(align) => {
            let s = if rb.slice { sx.max(sy) } else { sx.min(sy) };
            ((s, s), align)
        }
    };
    let dx = offset.0 + (size.0 - vw * sx) * ax;
    let dy = offset.1 + (size.1 - vh * sy) * ay;
    [vx - dx / sx, vy - dy / sy, canvas.0 / sx, canvas.1 / sy]
}

/// The SVG handed to resvg/krilla: sanitized, root resized to `canvas` px with `view_box`,
/// `fa:` icons as font glyphs, CSS drop shadows as SVG filters, per-run font fallback, and
/// fetched remote images inlined once per URL. Also returns the memory those images take once
/// usvg resolves them (decoded bytes and pixels, per occurrence).
pub(crate) fn for_export(
    doc: &Document,
    canvas: (f64, f64),
    view_box: [f64; 4],
    fa: &FaIcons,
    coverage: &FontCoverage,
    images: &HashMap<String, FetchedImage>,
) -> Result<(String, u64), RenderError> {
    let src = doc.input_text();
    let root = doc.root_element();
    let mut edits = Edits::default();
    sanitize(doc, src, &mut edits);

    for attr in root.attributes() {
        if attr.namespace().is_none()
            && matches!(
                attr.name(),
                "width" | "height" | "viewBox" | "preserveAspectRatio"
            )
        {
            edits.remove_attr(src, attr.range());
        }
    }

    let mut defs = String::new();
    let (image_memory, inlined) = inline_images(doc, images, &mut defs)?;
    for (node, use_el) in inlined {
        edits.replace(node.range(), use_el);
    }
    let mut shadows = DropShadows::default();
    for node in doc.descendants().filter(Node::is_element) {
        match node.tag_name().name() {
            "i" => {
                if let Some(glyph) = node.attribute("class").and_then(|c| fa.glyph_span(c)) {
                    edits.replace(node.range(), glyph);
                }
            }
            "style" => {
                for text in node.children().filter(Node::is_text) {
                    if let Some(css) = shadows.rewrite(&src[text.range()]) {
                        edits.replace(text.range(), css);
                    }
                }
            }
            _ => {}
        }
        for attr in node.attributes().filter(|a| a.name() == "style") {
            if let Some(css) = shadows.rewrite(&src[attr.range_value()]) {
                edits.replace(attr.range_value(), css);
            }
        }
    }
    defs.push_str(&shadows.filters);

    let [x, y, w, h] = view_box;
    let tag_end = start_tag_end(src, root);
    let defs = if defs.is_empty() {
        defs
    } else {
        format!("<defs>{defs}</defs>")
    };
    edits.replace(
        tag_end - 1..tag_end,
        format!(
            r#" width="{}" height="{}" viewBox="{x} {y} {w} {h}" preserveAspectRatio="none">{defs}"#,
            canvas.0, canvas.1
        ),
    );

    for text in doc.descendants().filter(Node::is_text) {
        let parent = text.parent_element();
        if parent.is_some_and(|p| matches!(p.tag_name().name(), "style" | "script" | "title")) {
            continue;
        }
        let range = text.range();
        if let Some(wrapped) = wrap_fallback(&src[range.clone()], parent, coverage) {
            edits.replace(range, wrapped);
        }
    }
    Ok((edits.apply(src), image_memory))
}

/// usvg decodes a data URL again for every element that references it, so a remote image used
/// N times is defined once in `defs` and each `<image>` becomes a `<use>` of a symbol carrying
/// its preserveAspectRatio. The decoded total (per occurrence) is checked before any base64 is
/// built. Returns the resolved-image memory and the (element, replacement) pairs.
fn inline_images<'a, 'i>(
    doc: &'a Document<'i>,
    images: &HashMap<String, FetchedImage>,
    defs: &mut String,
) -> Result<(u64, Vec<(Node<'a, 'i>, String)>), RenderError> {
    let src = doc.input_text();
    let occurrences: Vec<(Node, &str, &FetchedImage)> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "image" && n.tag_name().namespace() == Some(SVG_NS))
        .filter_map(|n| {
            let url = n.attributes().find(|a| a.name() == "href")?.value();
            let img = images.get(url).filter(|i| i.width > 0 && i.height > 0)?;
            Some((n, url, img))
        })
        .collect();
    let total: u64 = occurrences.iter().map(|o| o.2.bytes.len() as u64).sum();
    if total > MAX_INLINE_IMAGE_BYTES {
        return Err(RenderError::BadRequest(format!(
            "Remote images exceed the {MAX_INLINE_IMAGE_BYTES} byte limit"
        )));
    }
    let memory = occurrences
        .iter()
        .map(|o| o.2.bytes.len() as u64 + u64::from(o.2.width) * u64::from(o.2.height) * 4)
        .sum();
    let mut urls: Vec<&str> = Vec::new();
    let mut symbols: Vec<(&str, &str)> = Vec::new();
    let mut out = Vec::with_capacity(occurrences.len());
    for (node, url, img) in occurrences {
        let par = node.attribute("preserveAspectRatio").unwrap_or("");
        let (w, h) = (img.width, img.height);
        let n = urls.iter().position(|u| *u == url).unwrap_or_else(|| {
            urls.push(url);
            defs.push_str(&format!(
                r#"<image id="mink-img{}" width="{w}" height="{h}" preserveAspectRatio="none" href="{}"/>"#,
                urls.len() - 1,
                data_url(img)
            ));
            urls.len() - 1
        });
        let k = symbols.iter().position(|s| *s == (url, par)).unwrap_or_else(|| {
            symbols.push((url, par));
            defs.push_str(&format!(
                r##"<symbol id="mink-sym{}" viewBox="0 0 {w} {h}" preserveAspectRatio="{}"><use href="#mink-img{n}"/></symbol>"##,
                symbols.len() - 1,
                escape_attr(par)
            ));
            symbols.len() - 1
        });
        // The <use> keeps every other attribute (geometry, class, style, transform, clip).
        let mut el = format!(r##"<use href="#mink-sym{k}""##);
        for attr in node.attributes() {
            let name = attr.name();
            let script = name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("on"));
            if !script && !matches!(name, "href" | "preserveAspectRatio") {
                el.push(' ');
                el.push_str(&src[attr.range()]);
            }
        }
        // SVG 2 auto-sizes a sizeless <image> to its natural size; a <use> would take 100%.
        for (name, v) in [("width", w), ("height", h)] {
            if node.attribute(name).is_none() {
                el.push_str(&format!(r#" {name}="{v}""#));
            }
        }
        el.push_str("/>");
        out.push((node, el));
    }
    Ok((memory, out))
}

/// merman's resvg cascade admits only `filter: none|url(#id)`, so the CSS `drop-shadow()` of
/// Mermaid's neo look would be dropped. Each chain of drop-shadows becomes one SVG filter of
/// `feDropShadow` primitives (identical chains share one).
#[derive(Default)]
struct DropShadows {
    chains: Vec<String>,
    filters: String,
}

impl DropShadows {
    /// `css` with every parseable drop-shadow chain replaced by `url(#mink-dsN)`.
    fn rewrite(&mut self, css: &str) -> Option<String> {
        const FN: &str = "drop-shadow(";
        if !css.contains(FN) {
            return None;
        }
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(at) = rest.find(FN) {
            out.push_str(&rest[..at]);
            let mut chain = String::new();
            let mut tail = &rest[at..];
            while let Some(args) = tail.strip_prefix(FN) {
                let Some(end) = close_paren(args) else { break };
                let Some(primitive) = drop_shadow(&args[..end]) else {
                    break;
                };
                chain.push_str(&primitive);
                tail = &args[end + 1..];
                if !tail.trim_start().starts_with(FN) {
                    break;
                }
                tail = tail.trim_start();
            }
            if chain.is_empty() || tail.starts_with(FN) {
                // unparseable: keep the original text (merman then drops the filter)
                out.push_str(FN);
                rest = &rest[at + FN.len()..];
                continue;
            }
            let id = self.chains.iter().position(|c| *c == chain).unwrap_or_else(|| {
                self.filters.push_str(&format!(
                    r#"<filter id="mink-ds{}" x="-20%" y="-20%" width="140%" height="140%">{chain}</filter>"#,
                    self.chains.len()
                ));
                self.chains.push(chain);
                self.chains.len() - 1
            });
            out.push_str(&format!("url(#mink-ds{id})"));
            rest = tail;
        }
        out.push_str(rest);
        Some(out)
    }
}

/// Byte index of the `)` closing an already opened parenthesis.
fn close_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `<dx> <dy> [<blur>]` and an optional color (first or last) -> one `feDropShadow`.
fn drop_shadow(args: &str) -> Option<String> {
    let mut lengths = Vec::new();
    let mut color = None;
    let (mut depth, mut start) = (0, None);
    let mut tokens = Vec::new();
    for (i, c) in args.char_indices().chain([(args.len(), ' ')]) {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    tokens.push(&args[s..i]);
                }
                continue;
            }
            _ => {}
        }
        start.get_or_insert(i);
    }
    for token in tokens {
        let length = match token.strip_suffix("px") {
            Some(v) => v.parse::<f64>().ok().filter(|v| v.is_finite()),
            None => (token == "0").then_some(0.0),
        };
        match length {
            Some(v) => lengths.push(v),
            None if color.is_none() => color = Some(token.parse::<svgtypes::Color>().ok()?),
            None => return None,
        }
    }
    let (dx, dy, blur) = match lengths[..] {
        [dx, dy] => (dx, dy, 0.0),
        [dx, dy, blur] if blur >= 0.0 => (dx, dy, blur),
        _ => return None,
    };
    let c = color.unwrap_or_else(svgtypes::Color::black);
    // CSS blur radius = 2 standard deviations.
    Some(format!(
        r##"<feDropShadow dx="{dx}" dy="{dy}" stdDeviation="{}" flood-color="#{:02x}{:02x}{:02x}" flood-opacity="{}"/>"##,
        blur / 2.0,
        c.red,
        c.green,
        c.blue,
        f64::from(c.alpha) / 255.0
    ))
}

/// Per-run font fallback, as Chrome does per character: usvg picks one fallback font per line.
/// Runs the primary font lacks are wrapped in a span naming the first FONT_DIR family that has
/// them. Emoji runs always take the emoji font: usvg's per-glyph fallback cannot shape
/// multi-codepoint emoji (flags, skin tones, ZWJ sequences, VS16).
fn wrap_fallback(text: &str, parent: Option<Node>, coverage: &FontCoverage) -> Option<String> {
    const EMOJI: &str = "Noto Color Emoji";
    let is_emoji = |c: char| matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{200D}' | '\u{FE0F}' | '\u{20E3}' | '\u{E0020}'..='\u{E007F}');
    // Spaces, joiners and combining marks stay in the current text run (shaping, bidi).
    let joins = |c: char| {
        c.is_whitespace()
            || matches!(c, '\u{0300}'..='\u{036F}' | '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{206F}' | '\u{FE00}'..='\u{FE0E}' | '\u{20D0}'..='\u{20FF}')
    };
    if text.is_ascii() {
        return None;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut runs: Vec<(usize, Option<&str>)> = Vec::new(); // (start byte, family)
    for (i, &(at, c)) in chars.iter().enumerate() {
        let current = runs.last().and_then(|r| r.1);
        // A run may start with a text-presentation symbol that VS16 turns into an emoji (❤️).
        let emoji_start = matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{E0020}'..='\u{E007F}')
            || chars.get(i + 1).is_some_and(|n| n.1 == '\u{FE0F}');
        let family = if emoji_start || (current == Some(EMOJI) && is_emoji(c)) {
            Some(EMOJI)
        } else if current != Some(EMOJI) && joins(c) {
            current
        } else {
            coverage.fallback(c)
        };
        if runs.last().is_none_or(|r| r.1 != family) {
            runs.push((at, family));
        }
    }
    if runs.iter().all(|r| r.1.is_none()) {
        return None;
    }
    let tag = match parent?.tag_name().namespace() {
        Some(SVG_NS) => "tspan",
        _ => "span",
    };
    let mut out = String::with_capacity(text.len() + 64);
    for (j, &(start, family)) in runs.iter().enumerate() {
        let end = runs.get(j + 1).map_or(text.len(), |r| r.0);
        match family {
            Some(f) => out.push_str(&format!(
                r#"<{tag} style="font-family:'{f}'">{}</{tag}>"#,
                &text[start..end]
            )),
            None => out.push_str(&text[start..end]),
        }
    }
    Some(out)
}

fn data_url(img: &FetchedImage) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&img.bytes);
    format!("data:{};base64,{b64}", img.mime)
}

/// Chrome's CSSOM serialization of `style.backgroundColor = value`, or None when Chrome would
/// reject the value (it then writes nothing): keywords lowercased, everything else rgb()/rgba().
pub(crate) fn css_color(value: &str) -> Option<String> {
    let v = value.trim();
    if let Some(hex) = v.strip_prefix('#') {
        // svgtypes reads non-hex digits as 0; Chrome rejects them.
        if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
    }
    let c: svgtypes::Color = v.parse().ok()?;
    if v.bytes().all(|b| b.is_ascii_alphabetic()) && !v.eq_ignore_ascii_case("transparent") {
        return Some(v.to_ascii_lowercase());
    }
    Some(if c.alpha == 255 {
        format!("rgb({}, {}, {})", c.red, c.green, c.blue)
    } else {
        format!(
            "rgba({}, {}, {}, {})",
            c.red,
            c.green,
            c.blue,
            css_alpha(c.alpha)
        )
    })
}

/// Blink prints the shortest of 2 or 3 decimals that maps back to the same 8-bit alpha.
fn css_alpha(a: u8) -> String {
    let two = (f64::from(a) / 255.0 * 100.0).round() / 100.0;
    let v = if (two * 255.0).round() as u8 == a {
        two
    } else {
        (f64::from(a) / 255.0 * 1000.0).round() / 1000.0
    };
    format!("{v}")
}

/// `#rrggbbaa` for merman's export color parser (which knows hex and three keywords only).
pub(crate) fn hex_color(value: &str) -> Option<String> {
    css_color(value)?;
    let c: svgtypes::Color = value.trim().parse().ok()?;
    Some(format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        c.red, c.green, c.blue, c.alpha
    ))
}

/// JPEG has no alpha: Chrome's screenshot flattens a translucent background; we flatten on white.
pub(crate) fn opaque_on_white(hex: &str) -> String {
    let n = |i: usize| u32::from_str_radix(&hex[i..i + 2], 16).unwrap_or(255);
    let a = n(7);
    let mix = |c: u32| (c * a + 255 * (255 - a) + 127) / 255;
    format!("#{:02x}{:02x}{:02x}", mix(n(1)), mix(n(3)), mix(n(5)))
}

fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_color_serialization() {
        assert_eq!(css_color("#123123").as_deref(), Some("rgb(18, 49, 35)"));
        assert_eq!(css_color("#abc").as_deref(), Some("rgb(170, 187, 204)"));
        assert_eq!(
            css_color("#FF000055").as_deref(),
            Some("rgba(255, 0, 0, 0.333)")
        );
        assert_eq!(
            css_color("#ff000080").as_deref(),
            Some("rgba(255, 0, 0, 0.5)")
        );
        assert_eq!(css_color("SlateGray").as_deref(), Some("slategray"));
        assert_eq!(css_color("#zzzabc"), None);
        assert_eq!(css_color("#12345"), None);
        assert_eq!(css_color("red; x"), None);
        assert_eq!(css_color("non-existant-color"), None);
        assert_eq!(hex_color("slategray").as_deref(), Some("#708090ff"));
        assert_eq!(opaque_on_white("#ff000080"), "#ff7f7f");
    }

    #[test]
    fn finish_svg_rewrites_root_like_legacy() {
        let src = r#"<svg id="mermaid-svg" width="100%" xmlns="http://www.w3.org/2000/svg" style="max-width: 552px;" viewBox="4 4 552 588"><g onclick="x()"><a xlink:href="javascript:alert(1)" xmlns:xlink="http://www.w3.org/1999/xlink">t</a><script>alert(1)</script></g></svg>"#;
        let out = finish_svg(src, Some("#123123"), Some(1000.0), None, "https://fa/x.css").unwrap();
        assert_eq!(
            out,
            r#"<svg id="mermaid-svg" xmlns="http://www.w3.org/2000/svg" style="max-width: 100%; background-color: rgb(18, 49, 35);" viewBox="4 4 552 588" width="1000px" xmlns:xlink="http://www.w3.org/1999/xlink"><style xmlns="http://www.w3.org/1999/xhtml">@import url("https://fa/x.css");</style><g><a xmlns:xlink="http://www.w3.org/1999/xlink">t</a></g></svg>"#
        );
        let tall = src.replace(r#"width="100%""#, r#"width="100%" height="565""#);
        let out = finish_svg(&tall, None, Some(10.0), None, "u").unwrap();
        assert!(
            out.contains(r#"height="565""#) && out.contains(r#"width="10px""#),
            "{out}"
        );
        let out = finish_svg(&tall, None, None, Some(20.0), "u").unwrap();
        assert!(
            out.contains(r#"height="20px""#) && !out.contains("width="),
            "{out}"
        );
        let plain = finish_svg(src, Some("!bad"), None, None, "u").unwrap();
        assert!(
            plain.contains(r#"width="100%""#) && plain.contains(r#"style="max-width: 552px;""#)
        );
    }

    #[test]
    fn display_size_follows_chrome_layout() {
        let doc = Document::parse(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" style="max-width: 3103.6875px;" viewBox="0 0 3103.6875 62.48"/>"#).unwrap();
        let rb = root_box(&doc);
        let (w, h) = rb.display_size(1904.0, None, None);
        assert_eq!((w.round(), h.round()), (1904.0, 38.0));
        let (w, h) = rb.display_size(1904.0, None, Some(100.0));
        assert_eq!((w.round(), h), (4967.0, 100.0));
        let doc = Document::parse(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" style="max-width: 400px;"/>"#,
        )
        .unwrap();
        assert_eq!(
            root_box(&doc).display_size(1904.0, None, None),
            (400.0, 150.0)
        );
        let doc = Document::parse(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="565" style="max-width: 1300px;" viewBox="0 -25 1300 540"/>"#).unwrap();
        assert_eq!(
            root_box(&doc).display_size(1904.0, None, None),
            (1300.0, 565.0)
        );
    }

    #[test]
    fn emoji_runs_get_the_emoji_font() {
        let doc = Document::parse(r#"<text xmlns="http://www.w3.org/2000/svg">x</text>"#).unwrap();
        let svg_parent = doc.root_element();
        let none = FontCoverage::empty();
        assert_eq!(wrap_fallback("plain ❤ text", Some(svg_parent), &none), None);
        assert_eq!(
            wrap_fallback("a 👍🏽 and ❤️!", Some(svg_parent), &none).unwrap(),
            r#"a <tspan style="font-family:'Noto Color Emoji'">👍🏽</tspan> and <tspan style="font-family:'Noto Color Emoji'">❤️</tspan>!"#
        );
    }

    #[test]
    fn drop_shadows_become_filters() {
        let mut ds = DropShadows::default();
        let css = ".node{filter:drop-shadow(1px 2px 2px rgba(0, 0, 0, 0.5));} .x{filter: drop-shadow(1px 2px 2px rgba(0, 0, 0, 0.5)) drop-shadow(red 0 3px)}";
        assert_eq!(
            ds.rewrite(css).unwrap(),
            ".node{filter:url(#mink-ds0);} .x{filter: url(#mink-ds1)}"
        );
        assert_eq!(
            ds.filters,
            concat!(
                r##"<filter id="mink-ds0" x="-20%" y="-20%" width="140%" height="140%"><feDropShadow dx="1" dy="2" stdDeviation="1" flood-color="#000000" flood-opacity="0.5019607843137255"/></filter>"##,
                r##"<filter id="mink-ds1" x="-20%" y="-20%" width="140%" height="140%"><feDropShadow dx="1" dy="2" stdDeviation="1" flood-color="#000000" flood-opacity="0.5019607843137255"/><feDropShadow dx="0" dy="3" stdDeviation="0" flood-color="#ff0000" flood-opacity="1"/></filter>"##
            )
        );
        // same chain again: reused; unparseable arguments: left alone
        assert_eq!(
            ds.rewrite("a{filter:drop-shadow(1px 2px 2px rgba(0, 0, 0, 0.5))}")
                .unwrap(),
            "a{filter:url(#mink-ds0)}"
        );
        assert_eq!(
            ds.rewrite("a{filter:drop-shadow(1em 2em)}").unwrap(),
            "a{filter:drop-shadow(1em 2em)}"
        );
        assert_eq!(ds.rewrite("a{fill:red}"), None);
    }

    #[test]
    fn remote_images_are_defined_once() {
        let src = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><image href="https://i/a.png" x="1" y="2" width="4" height="4" class="c"/><image href="https://i/a.png" x="5" width="2" height="2" preserveAspectRatio="xMinYMin slice"/><image href="https://i/b.png"/></svg>"#;
        let doc = Document::parse(src).unwrap();
        let img = FetchedImage {
            bytes: vec![1u8; 3000].into(),
            mime: "image/png",
            width: 8,
            height: 4,
        };
        let images = HashMap::from([("https://i/a.png".to_owned(), img.clone())]);
        let mut defs = String::new();
        let (memory, uses) = inline_images(&doc, &images, &mut defs).unwrap();
        assert_eq!(memory, 2 * (3000 + 8 * 4 * 4));
        assert_eq!(defs.matches("data:image/png;base64,").count(), 1);
        assert_eq!(defs.matches("<symbol").count(), 2);
        assert_eq!(
            uses[0].1,
            r##"<use href="#mink-sym0" x="1" y="2" width="4" height="4" class="c"/>"##
        );
        assert_eq!(
            uses[1].1,
            r##"<use href="#mink-sym1" x="5" width="2" height="2"/>"##
        );
        assert_eq!(uses.len(), 2, "unfetched images are left alone");

        // 12 occurrences of a 3 MiB image: over the decoded total before any base64 is built.
        let big = FetchedImage {
            bytes: vec![0u8; 3 << 20].into(),
            ..img
        };
        let many = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg">{}</svg>"#,
            r#"<image href="https://i/a.png"/>"#.repeat(12)
        );
        let doc = Document::parse(&many).unwrap();
        let images = HashMap::from([("https://i/a.png".to_owned(), big)]);
        assert!(matches!(
            inline_images(&doc, &images, &mut String::new()),
            Err(RenderError::BadRequest(_))
        ));
    }

    #[test]
    fn canvas_view_box_places_content() {
        let doc =
            Document::parse(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="4 4 100 50"/>"#)
                .unwrap();
        let rb = root_box(&doc);
        // 200x100 display at (8, 8) on a 300x300 page: scale 2, origin shifted by 4 user units.
        assert_eq!(
            canvas_view_box(&rb, (200.0, 100.0), (8.0, 8.0), (300.0, 300.0)),
            [0.0, 0.0, 150.0, 150.0]
        );
        // letterboxed: 100x100 box -> scale 1, content centered vertically (+25px)
        assert_eq!(
            canvas_view_box(&rb, (100.0, 100.0), (0.0, 0.0), (100.0, 100.0)),
            [4.0, -21.0, 100.0, 100.0]
        );
        // journey's xMinYMin: letterbox slack goes below the content instead
        let top_left = |par: &str| {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="4 4 100 50" preserveAspectRatio="{par}"/>"#
            );
            let doc = Document::parse(&svg).unwrap();
            canvas_view_box(&root_box(&doc), (100.0, 100.0), (0.0, 0.0), (100.0, 100.0))
        };
        assert_eq!(top_left("xMinYMin meet"), [4.0, 4.0, 100.0, 100.0]);
        assert_eq!(top_left("xMidYMax"), [4.0, -46.0, 100.0, 100.0]);
        assert_eq!(top_left("bogus"), [4.0, -21.0, 100.0, 100.0]);
        // none stretches: x scale 1, y scale 2
        assert_eq!(top_left("none"), [4.0, 4.0, 100.0, 50.0]);
        // slice: scale 2, centered horizontally (50px cut on each side)
        assert_eq!(top_left("xMidYMid slice"), [29.0, 4.0, 50.0, 50.0]);
    }
}
