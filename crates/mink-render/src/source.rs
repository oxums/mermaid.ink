//! Diagram-source rewrites done before merman sees the code: Mermaid's maxTextSize
//! substitution and natural sizes for remote image shapes.

use std::collections::HashMap;

use mink_core::FetchedImage;

/// Mermaid's `MAX_TEXTLENGTH_EXCEEDED_MSG` diagram.
pub(crate) const MAX_TEXT_EXCEEDED: &str =
    "graph TB;a[Maximum text size in diagram exceeded];style a fill:#faa";

/// Mermaid 12 `renderDiagram`: the source is replaced when its preprocessed length (UTF-16 units,
/// without front matter, `%%{init}%%` directives and `%%` comment lines) exceeds maxTextSize.
pub(crate) fn exceeds_max_text_size(code: &str, max: f64) -> bool {
    let code = code.replace("\r\n", "\n").replace('\r', "\n");
    let mut text = code.as_str();
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        text = rest[end + 4..].trim_start_matches(|c| c != '\n');
    }
    let mut len = 0usize;
    let mut rest = text;
    // `%%{ ... }%%` directives may span lines; drop them before the comment pass.
    let mut without_directives = String::with_capacity(rest.len());
    while let Some(start) = rest.find("%%{") {
        without_directives.push_str(&rest[..start]);
        rest = rest[start..]
            .find("}%%")
            .map_or("", |end| &rest[start + end + 3..]);
    }
    without_directives.push_str(rest);
    let mut leading = true;
    for line in without_directives.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("%%") && !t.starts_with("%%{") && t.trim_end().len() > 2 {
            continue;
        }
        let line = if leading { line.trim_start() } else { line };
        leading &= line.is_empty();
        len += line.encode_utf16().count();
    }
    len as f64 > max
}

/// https URLs of remote images: `img:` shape values and `<img src>` in labels, in order, deduped.
pub(crate) fn image_urls(code: &str) -> Vec<String> {
    let mut found: Vec<(usize, &str)> = code
        .match_indices("img:")
        .chain(code.match_indices("src="))
        .filter_map(|(at, key)| {
            let value = code[at + key.len()..]
                .trim_start()
                .trim_start_matches(['"', '\'']);
            let end = value
                .find(['"', '\'', ' ', '\n', ',', '}', '>'])
                .unwrap_or(value.len());
            value.starts_with("https://").then(|| (at, &value[..end]))
        })
        .collect();
    found.sort_unstable();
    let mut urls: Vec<String> = Vec::new();
    for (_, url) in found {
        if !urls.iter().any(|u| u == url) {
            urls.push(url.to_owned());
        }
    }
    urls
}

/// Mermaid sizes an image shape from the decoded image (`naturalWidth/Height`) when `w`/`h` are
/// omitted; merman defaults to a 60px square. Inject the natural size for fetched images.
/// shortcut: only the missing dimension is filled in, so `constraint: on` with one explicit side
/// keeps merman's aspect ratio; upgrade with an upstream intrinsic-size hook.
pub(crate) fn inject_image_sizes(code: &str, images: &HashMap<String, FetchedImage>) -> String {
    if images.is_empty() {
        return code.to_owned();
    }
    let mut out = String::with_capacity(code.len() + 64);
    let mut rest = code;
    while let Some(start) = rest.find("@{") {
        let Some(len) = block_len(&rest[start..]) else {
            break;
        };
        let block = &rest[start..start + len];
        out.push_str(&rest[..start]);
        out.push_str(&block[..len - 1]);
        let url = image_urls(block).into_iter().next();
        if let Some(img) = url.and_then(|u| images.get(&u)) {
            if !has_key(block, 'w') {
                out.push_str(&format!(", w: {}", img.width));
            }
            if !has_key(block, 'h') {
                out.push_str(&format!(", h: {}", img.height));
            }
        }
        out.push('}');
        rest = &rest[start + len..];
    }
    out.push_str(rest);
    out
}

/// Length of an `@{ ... }` block including braces, skipping quoted strings.
fn block_len(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(2) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '}') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

fn has_key(block: &str, key: char) -> bool {
    let bytes = block.as_bytes();
    block.match_indices(key).any(|(i, _)| {
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = block[i + 1..].trim_start();
        matches!(before, Some(b'{' | b',' | b' ' | b'\n' | b'\t')) && after.starts_with(':')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> FetchedImage {
        FetchedImage {
            bytes: Default::default(),
            mime: "image/png",
            width: w,
            height: h,
        }
    }

    #[test]
    fn max_text_size_counts_like_mermaid() {
        assert!(!exceeds_max_text_size("graph TD\nA-->B", 50000.0));
        assert!(exceeds_max_text_size("graph TD\nA-->B", 5.0));
        let padded = format!("graph TD\n{}A-->B", "%% comment comment\n".repeat(10));
        assert!(!exceeds_max_text_size(&padded, 20.0));
        let directive = "%%{init: {'theme': 'forest'}}%%\ngraph TD\nA-->B";
        assert!(!exceeds_max_text_size(directive, 16.0));
        let fm = "---\ntitle: a long title here\n---\ngraph TD\nA-->B";
        assert!(!exceeds_max_text_size(fm, 16.0));
        assert!(exceeds_max_text_size("graph TD\nA[ééé]-->B", 15.0)); // 18 UTF-16 units
    }

    #[test]
    fn finds_image_urls_in_order() {
        let code = r#"flowchart LR
  A@{ img: "https://b.example/x.png", label: "x" } --> B["<img src='https://a.example/y.jpg'/>"]
  C@{ img: "https://b.example/x.png" }
  D@{ img: 'http://insecure.example/z.png' }"#;
        assert_eq!(
            image_urls(code),
            ["https://b.example/x.png", "https://a.example/y.jpg"]
        );
    }

    #[test]
    fn injects_missing_natural_sizes() {
        let images = HashMap::from([("https://e.x/a.png".to_owned(), img(120, 80))]);
        let code = r#"A@{ img: "https://e.x/a.png", label: "w}" } --> B@{ img: "https://e.x/a.png", h: 40 } --> C@{ shape: rect }"#;
        assert_eq!(
            inject_image_sizes(code, &images),
            r#"A@{ img: "https://e.x/a.png", label: "w}" , w: 120, h: 80} --> B@{ img: "https://e.x/a.png", h: 40 , w: 120} --> C@{ shape: rect }"#
        );
    }
}
