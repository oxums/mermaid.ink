mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::{FORMATS, corpus, engine, expected_error, job, render, render_within};
use mink_core::FetchedImage;
use mink_render::{OutputFormat, RenderError};

fn svg_text(code: &str) -> String {
    String::from_utf8(render(&job(code, serde_json::json!({}), OutputFormat::Svg)).unwrap())
        .unwrap()
}

#[test]
fn corpus_renders_in_every_format() {
    let mut failures = Vec::new();
    for (id, code, config) in corpus() {
        for format in FORMATS {
            let result = render(&job(&code, config.clone(), format));
            let ok = match (&result, expected_error(&id)) {
                (Ok(body), false) => !body.is_empty(),
                (Err(RenderError::BadRequest(_)), true) => true,
                _ => false,
            };
            if !ok {
                failures.push(format!("{id} {format:?}: {:?}", result.map(|b| b.len())));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn output_magic_and_mime() {
    let code = "graph TD\nA-->B";
    for (format, mime, magic) in [
        (OutputFormat::Svg, "image/svg+xml", &b"<svg"[..]),
        (OutputFormat::Png, "image/png", b"\x89PNG"),
        (OutputFormat::Jpeg, "image/jpeg", b"\xff\xd8\xff"),
        (OutputFormat::Webp, "image/webp", b"RIFF"),
        (OutputFormat::Pdf, "application/pdf", b"%PDF"),
    ] {
        let out = engine()
            .render(
                &job(code, serde_json::json!({}), format),
                &HashMap::new(),
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(out.mime, mime);
        assert!(out.body.starts_with(magic), "{format:?}");
    }
}

#[test]
fn rendering_is_deterministic() {
    for (id, code, config) in corpus() {
        // handDrawn is seeded but rough paths are not part of the parity contract.
        if expected_error(&id) || id == "F03" {
            continue;
        }
        for format in FORMATS {
            let j = job(&code, config.clone(), format);
            assert!(
                render(&j).unwrap() == render(&j).unwrap(),
                "{id} {format:?} differs between runs"
            );
        }
    }
}

#[test]
fn svg_strips_script_vectors() {
    let payloads = corpus()
        .into_iter()
        .filter(|(id, ..)| id.starts_with("F21"))
        .map(|(_, code, _)| code)
        .chain([
            "erDiagram\n A[\"<img src=x onerror=alert(1)>\"] {\n string name\n }".to_owned(),
            "flowchart TD\n A[\"<a href='javascript:alert(1)'>x</a>\"]\n click A \"javascript:alert(2)\"".to_owned(),
            "erDiagram\n A[\"<svg><set attributeName='onmouseover' to='alert(1)'/></svg>\"] {\n string n\n }".to_owned(),
        ]);
    for code in payloads {
        let svg = svg_text(&code).to_ascii_lowercase();
        let doc = roxmltree::Document::parse(&svg).unwrap();
        for node in doc.descendants().filter(roxmltree::Node::is_element) {
            assert!(
                !matches!(node.tag_name().name(), "script" | "iframe"),
                "{code}"
            );
            for a in node.attributes() {
                assert!(
                    !a.name().starts_with("on"),
                    "{code}: {}={}",
                    a.name(),
                    a.value()
                );
                assert!(
                    !a.value().contains("javascript:"),
                    "{code}: {}={}",
                    a.name(),
                    a.value()
                );
            }
        }
    }
}

#[test]
fn svg_root_matches_legacy_shape() {
    let mut j = job(
        "graph TD\nA-->B",
        serde_json::json!({"theme": "default"}),
        OutputFormat::Svg,
    );
    let plain = String::from_utf8(render(&j).unwrap()).unwrap();
    assert!(
        plain.starts_with(
            r#"<svg id="mermaid-svg" width="100%" xmlns="http://www.w3.org/2000/svg""#
        ),
        "{}",
        &plain[..200]
    );
    assert!(plain.contains(r#"><style xmlns="http://www.w3.org/1999/xhtml">@import url("https://cdnjs.cloudflare.com/ajax/libs/font-awesome/7.2.0/css/all.min.css");</style><style>"#));
    assert!(plain.contains("#mermaid-svg .node rect"));
    j.bg_color = Some("#123123".into());
    j.width = Some(245.00000000000003);
    let sized = String::from_utf8(render(&j).unwrap()).unwrap();
    let root = &sized[..sized.find('>').unwrap()];
    assert!(
        root.contains(r#"style="max-width: 100%; background-color: rgb(18, 49, 35);""#),
        "{root}"
    );
    assert!(
        root.contains(r#"width="245.00000000000003px""#) && !root.contains(r#"width="100%""#),
        "{root}"
    );
}

#[test]
fn max_text_size_substitutes_the_error_diagram() {
    let (_, f11, config) = corpus().into_iter().find(|(id, ..)| id == "F11").unwrap();
    let svg = String::from_utf8(render(&job(&f11, config, OutputFormat::Svg)).unwrap()).unwrap();
    assert!(svg.contains("Maximum text size in diagram exceeded"));
    let svg = String::from_utf8(
        render(&job(
            &f11,
            serde_json::json!({"maxTextSize": 100000}),
            OutputFormat::Svg,
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(!svg.contains("Maximum text size"));
    assert!(svg.contains("Is it working?"));
}

#[test]
fn unknown_diagrams_use_the_legacy_message() {
    for code in ["", "%% only a comment", "hello world"] {
        match render(&job(code, serde_json::json!({}), OutputFormat::Svg)) {
            Err(RenderError::BadRequest(m)) => assert_eq!(m, "Unknown diagram error"),
            other => panic!("{code:?}: {:?}", other.map(|b| b.len())),
        }
    }
    assert!(matches!(
        render(&job(
            "graph TD\nA-->",
            serde_json::json!({}),
            OutputFormat::Png
        )),
        Err(RenderError::BadRequest(_))
    ));
}

#[test]
fn deadlines_become_timeouts() {
    let j = job("graph TD\nA-->B", serde_json::json!({}), OutputFormat::Png);
    assert!(matches!(
        engine().render(&j, &HashMap::new(), Instant::now()),
        Err(RenderError::Timeout)
    ));
    let big: String = (0..400).fold("flowchart TD\n".into(), |s, i| {
        s + &format!(" N{i}-->N{}\n", (i * 7 + 3) % 400)
    });
    assert!(matches!(
        render_within(
            &job(&big, serde_json::json!({}), OutputFormat::Svg),
            Duration::from_millis(5)
        ),
        Err(RenderError::Timeout)
    ));
}

#[test]
fn five_hundred_edges_fit_the_layout_budget() {
    let edges: String = (0..500)
        .map(|i| format!(" N{i}-->N{}\n", (i * 7 + 3) % 500))
        .collect();
    let svg = render_within(
        &job(
            &format!("flowchart TD\n{edges}"),
            serde_json::json!({}),
            OutputFormat::Svg,
        ),
        Duration::from_secs(120),
    );
    assert!(svg.is_ok(), "{:?}", svg.err());
}

#[test]
fn raster_limits_are_bad_requests() {
    let (_, wide, _) = corpus().into_iter().find(|(id, ..)| id == "T19w").unwrap();
    let mut j = job(&wide, serde_json::json!({}), OutputFormat::Webp);
    j.width = Some(17000.0);
    match render(&j) {
        Err(RenderError::BadRequest(m)) => assert!(m.contains("WebP"), "{m}"),
        other => panic!("{:?}", other.map(|b| b.len())),
    }
    let mut j = job("graph TD\nA-->B", serde_json::json!({}), OutputFormat::Png);
    j.width = Some(20000.0);
    assert!(matches!(render(&j), Err(RenderError::BadRequest(_))));
}

#[test]
fn raster_sizes_follow_the_element_screenshot() {
    let dims = |j: &mink_render::RenderJob| {
        let img = image::load_from_memory(&render(j).unwrap()).unwrap();
        (img.width(), img.height())
    };
    // K01's shape: viewBox 552x588 at its max-width.
    let code = "graph TD\nA[Christmas] -->|Get money| B(Go shopping)\nB --> C{Let me think}\nC -->|One| D[Laptop]\nC -->|Two| E[iPhone]\nC -->|Three| F[fa:fa-car Car]";
    let mut j = job(
        code,
        serde_json::json!({"theme": "default"}),
        OutputFormat::Png,
    );
    assert_eq!(dims(&j), (552, 588));
    j.width = Some(1000.0);
    assert_eq!(dims(&j), (1000, 1065));
    j.height = Some(1000.0);
    assert_eq!(dims(&j), (1000, 1000));
    j.width = None;
    assert_eq!(dims(&j), (939, 1000));
}

#[test]
fn jpeg_and_png_backgrounds() {
    let pixel = |j: &mink_render::RenderJob| {
        image::load_from_memory(&render(j).unwrap())
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)
            .0
    };
    let mut j = job("graph TD\nA-->B", serde_json::json!({}), OutputFormat::Png);
    assert_eq!(pixel(&j)[3], 0);
    j.bg_color = Some("#1b1b1f".into());
    assert_eq!(pixel(&j), [27, 27, 31, 255]);
    j.format = OutputFormat::Jpeg;
    j.bg_color = None;
    assert!(pixel(&j)[..3].iter().all(|&c| c > 250));
}

#[test]
fn pdf_pages_follow_chrome() {
    let page_boxes = |j: &mink_render::RenderJob| -> Vec<Vec<f32>> {
        let pdf = lopdf::Document::load_mem(&render(j).unwrap()).unwrap();
        pdf.get_pages()
            .values()
            .map(|&id| {
                let page = pdf.get_dictionary(id).unwrap();
                page.get(b"MediaBox")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_float().unwrap())
                    .collect()
            })
            .collect()
    };
    let (_, t12, config) = corpus().into_iter().find(|(id, ..)| id == "T12").unwrap();
    let mut j = job(&t12, config, OutputFormat::Pdf);
    let boxes = page_boxes(&j);
    assert_eq!(boxes.len(), 3);
    assert!(
        (boxes[0][2] - 595.92).abs() < 0.5 && (boxes[0][3] - 841.92).abs() < 0.5,
        "{boxes:?}"
    );
    j.fit = true;
    let boxes = page_boxes(&j);
    assert_eq!(boxes.len(), 1);
    assert!((boxes[0][2] - 540.0).abs() < 1.0, "{boxes:?}"); // (ceil(704.2) + 16) * 0.75
}

#[test]
fn remote_images_are_inlined_for_rasters_and_kept_for_svg() {
    let url = "https://images.example/logo.png";
    // No label: a label would widen the shape to the wrapping width (as in Mermaid).
    let code = format!("flowchart LR\n A@{{ img: \"{url}\" }}");
    assert_eq!(engine().image_urls(&code), [url]);
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 0, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let images = HashMap::from([(
        url.to_owned(),
        FetchedImage {
            bytes: png.into(),
            mime: "image/png",
            width: 40,
            height: 20,
        },
    )]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let svg = engine()
        .render(
            &job(&code, serde_json::json!({}), OutputFormat::Svg),
            &images,
            deadline,
        )
        .unwrap();
    let svg = String::from_utf8(svg.body).unwrap();
    assert!(
        svg.contains(url) && svg.contains(r#"width="40""#) && svg.contains(r#"height="20""#),
        "natural size + original href"
    );
    let raster = engine()
        .render(
            &job(&code, serde_json::json!({}), OutputFormat::Png),
            &images,
            deadline,
        )
        .unwrap();
    let img = image::load_from_memory(&raster.body).unwrap().to_rgba8();
    assert!(
        img.pixels().any(|p| p.0 == [255, 0, 0, 255]),
        "image painted"
    );
}

#[test]
fn estimate_and_identity() {
    let e = engine();
    assert!(e.identity().starts_with("merman=0.10.0;fonts="));
    let mut j = job("graph TD\nA-->B", serde_json::json!({}), OutputFormat::Png);
    let small = e.estimate_bytes(&j);
    j.width = Some(4000.0);
    j.height = Some(4000.0);
    assert!(e.estimate_bytes(&j) >= 4000 * 4000 * 4 && e.estimate_bytes(&j) > small);
}

#[test]
fn prepare_reports_the_real_canvas_memory() {
    let e = engine();
    let deadline = || Instant::now() + Duration::from_secs(60);
    let code = "graph TD\nA[Christmas] -->|Get money| B(Go shopping)";
    let mut j = job(code, serde_json::json!({}), OutputFormat::Png);
    j.width = Some(1000.0);
    let prepared = e.prepare(&j, &HashMap::new(), deadline()).unwrap();
    let memory = prepared.memory_bytes();
    let png = e.finish(prepared, deadline()).unwrap();
    let img = image::load_from_memory(&png.body).unwrap();
    assert!(
        memory >= u64::from(img.width() * img.height()) * 12,
        "{memory}"
    );
    assert_eq!(png.body, render(&j).unwrap(), "render = prepare + finish");

    // No dimensions: the pre-layout estimate guesses, prepare knows (a tall gantt chart).
    let tasks: String = (0..400)
        .map(|i| format!("    t{i} :2024-01-01, 1d\n"))
        .collect();
    let gantt = format!("gantt\n  dateFormat YYYY-MM-DD\n  section S\n{tasks}");
    let j = job(&gantt, serde_json::json!({}), OutputFormat::Webp);
    let prepared = e.prepare(&j, &HashMap::new(), deadline()).unwrap();
    assert!(prepared.memory_bytes() > e.estimate_bytes(&j));
    assert!(prepared.memory_bytes() >= 1904 * 9000 * 20);

    let svg = e
        .prepare(
            &job(code, serde_json::json!({}), OutputFormat::Svg),
            &HashMap::new(),
            deadline(),
        )
        .unwrap();
    assert_eq!(svg.memory_bytes(), 0);
    let pdf = e
        .prepare(
            &job(code, serde_json::json!({}), OutputFormat::Pdf),
            &HashMap::new(),
            deadline(),
        )
        .unwrap();
    assert!(pdf.memory_bytes() > 16 << 20);
}

#[test]
fn repeated_remote_images_are_inlined_once_and_bounded() {
    let url = "https://images.example/logo.png";
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 0, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let nodes = |n: usize| {
        let body: String = (0..n)
            .map(|i| format!(" N{i}@{{ img: \"{url}\" }}\n"))
            .collect();
        format!("flowchart LR\n{body}")
    };
    let fetched = |bytes: Vec<u8>| {
        HashMap::from([(
            url.to_owned(),
            FetchedImage {
                bytes: bytes.into(),
                mime: "image/png",
                width: 40,
                height: 20,
            },
        )])
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let j = job(&nodes(5), serde_json::json!({}), OutputFormat::Png);
    let out = engine()
        .render(&j, &fetched(png.clone()), deadline)
        .unwrap();
    let img = image::load_from_memory(&out.body).unwrap().to_rgba8();
    let red = img.pixels().filter(|p| p.0 == [255, 0, 0, 255]).count();
    assert!(
        red >= 5 * 30 * 15,
        "all five occurrences painted ({red} px)"
    );

    // 1x1 PNG padded to 1 MiB (passes header sniffing): 50 occurrences exceed the inline total.
    let mut padded = png;
    padded.resize(1 << 20, 0);
    let j = job(&nodes(50), serde_json::json!({}), OutputFormat::Png);
    match engine().render(&j, &fetched(padded), deadline) {
        Err(RenderError::BadRequest(m)) => assert!(m.contains("Remote images"), "{m}"),
        other => panic!("{:?}", other.map(|o| o.body.len())),
    }
}

#[test]
fn unparseable_label_markup_is_a_bad_request() {
    let code = "requirementDiagram\nrequirement r {\nid: 1\ntext: \"<img src=x>\"\nrisk: high\nverifymethod: test\n}";
    for format in [OutputFormat::Svg, OutputFormat::Png] {
        match render(&job(code, serde_json::json!({}), format)) {
            Err(RenderError::BadRequest(m)) => assert!(!m.contains("img"), "{m}"),
            other => panic!("{format:?}: {:?}", other.map(|b| b.len())),
        }
    }
}

#[test]
fn user_text_never_reaches_internal_errors() {
    // Egyptian hieroglyphs (no shipped font), an inline font-family and a huge label count.
    let many: String = (0..2000).map(|i| format!("  n{i}[label {i}]\n")).collect();
    let cases = [
        "graph TD\nA[\"SECRET \u{13000}\u{13001}\"] --> B".to_owned(),
        "graph TD\nA[\"<span style='font-family:SECRET'>x</span>\"] --> B".to_owned(),
        format!("graph TD\n{many}"),
    ];
    for code in &cases {
        for format in [OutputFormat::Svg, OutputFormat::Png, OutputFormat::Pdf] {
            match render(&job(code, serde_json::json!({}), format)) {
                Ok(_) | Err(RenderError::BadRequest(_)) => {}
                Err(e) => panic!("{format:?} {}: {e:?}", &code[..30]),
            }
        }
    }
}
