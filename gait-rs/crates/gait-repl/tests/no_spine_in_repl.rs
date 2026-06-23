//! gait-repl must not name core's sealed mechanism — it drives turns only via embed::run_loop.
use std::fs;
use std::path::Path;

fn scan(dir: &Path, needles: &[&str], out: &mut String) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            scan(&p, needles, out);
            continue;
        }
        if p.extension().map_or(true, |x| x != "rs") {
            continue;
        }
        for (i, line) in fs::read_to_string(&p).unwrap().lines().enumerate() {
            let code = line.split("//").next().unwrap_or(""); // ignore comments
            for n in needles {
                if code.contains(n) {
                    out.push_str(&format!("{}:{}: {}\n", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

#[test]
fn repl_does_not_reference_sealed_mechanism() {
    // gait-repl/tests/ runs with CWD = crate root (crates/gait-repl).
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = String::new();
    scan(
        &src,
        &[
            "turn::spine",
            "relay_brain_turn",
            "SignalSlot",
            "ProfileInputs",
        ],
        &mut hits,
    );
    assert!(
        hits.trim().is_empty(),
        "gait-repl still reaches into sealed mechanism:\n{hits}"
    );
}
