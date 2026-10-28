//! Every timer knob is listed somewhere (acs-r1k): a `ACS_*_MS` or
//! `ACS_*_SECS` name in `src/` must be a row of DESIGN §5.3's knob table or
//! of the README's environment table, so the set cannot grow unseen.

use std::path::Path;

/// Every `"ACS_…_MS"` / `"ACS_…_SECS"` string literal in `dir`'s sources.
fn knobs(dir: &Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            knobs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for chunk in text.split("\"ACS_").skip(1) {
                let name: String = chunk
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                    .collect();
                let quoted = chunk[name.len()..].starts_with('"');
                if quoted && (name.ends_with("_MS") || name.ends_with("_SECS")) {
                    out.push(format!("ACS_{name}"));
                }
            }
        }
    }
}

#[test]
fn every_timer_knob_is_in_a_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    knobs(&root.join("src"), &mut found);
    found.sort();
    found.dedup();
    assert!(found.contains(&"ACS_DEAD_MS".to_string()), "{found:?}");
    let docs = std::fs::read_to_string(root.join("docs/DESIGN.md")).unwrap()
        + &std::fs::read_to_string(root.join("README.md")).unwrap();
    let missing: Vec<_> = found
        .iter()
        .filter(|k| {
            !docs.contains(&format!("| `{k}` |")) && !docs.contains(&format!("| `{k}` (README) |"))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "timer knobs in src/ with no row in DESIGN §5.3 or the README: {missing:?}"
    );
}
