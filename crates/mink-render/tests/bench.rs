//! Per-format render timings. Run in release:
//! `cargo test -p mink-render --release --test bench -- --ignored --nocapture`

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::{FORMATS, corpus, engine, job, render};

#[test]
#[ignore]
fn per_format_timings() {
    let started = Instant::now();
    engine();
    println!("engine (fonts) load: {:?}", started.elapsed());
    for (id, code, config) in corpus()
        .into_iter()
        .filter(|(id, ..)| ["T01", "T08", "T12", "T13", "T19w", "T25"].contains(&id.as_str()))
    {
        let mut line = format!("{id:5}");
        for format in FORMATS {
            let j = job(&code, config.clone(), format);
            render(&j).unwrap(); // warm caches
            let runs = 10;
            let (mut prepare, mut total, mut bytes) = (Duration::ZERO, Duration::ZERO, 0);
            for _ in 0..runs {
                let deadline = Instant::now() + Duration::from_secs(60);
                let t = Instant::now();
                let prepared = engine().prepare(&j, &HashMap::new(), deadline).unwrap();
                prepare += t.elapsed();
                bytes = engine().finish(prepared, deadline).unwrap().body.len();
                total += t.elapsed();
            }
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / runs as f64;
            // total (of which prepare: layout + export SVG), output size
            line += &format!(
                "  {format:?} {:>6.1}ms ({:>5.1}) {:>7}B",
                ms(total),
                ms(prepare),
                bytes
            );
        }
        println!("{line}");
    }
}
