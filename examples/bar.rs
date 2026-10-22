---
[package]
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

//! A bar layout: copy to `~/.pi/bar.rs`. pi runs it with what it knows as
//! JSON on stdin and draws the layout it prints; see "The bar" in the README.

use serde::Deserialize;
use serde_json::json;
use std::path::Path;

// Only the facts this script reads; serde ignores the rest.
#[derive(Deserialize)]
struct Facts {
    root: String,
    running: bool,
}

fn main() {
    let facts: Facts = serde_json::from_reader(std::io::stdin()).expect("facts on stdin");
    let name = Path::new(&facts.root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut left = vec![
        // The script's own text, in a theme colour.
        json!({ "text": name, "style": "heading" }),
        // Parts pi fills in, by name.
        json!("model"),
        json!("effort"),
        // A label and its part with nothing between them; both go while the
        // session has spent nothing.
        json!({ "group": [{ "text": "spent " }, { "part": "spent", "style": "emphasis" }] }),
        // A status part, from the run in flight or the last one.
        json!("ctx"),
    ];
    if facts.running {
        left.push(json!({ "text": "working", "style": { "color": "#e5c07b", "sgr": ["bold"] } }));
    }

    let layout = json!({
        "lines": [{
            "left": left,
            "right": [{ "part": "update", "style": "emphasis" }, "flash"],
            "sep": { "text": "  " }
        }],
        // Rerun every so often only for state outside pi; a change in pi's
        // own reruns it anyway.
        "refresh": null
    });
    println!("{layout}");
}
