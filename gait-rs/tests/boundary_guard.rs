//! Bulletproof boundary: sealed mechanism is named only inside gait-core (src/).
use std::fs;
use std::path::{Path, PathBuf};

fn scan(dir: &Path, needles: &[&str], out: &mut String) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            scan(&p, needles, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        for (i, line) in fs::read_to_string(&p).unwrap().lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for n in needles {
                if code.contains(n) {
                    out.push_str(&format!("{}:{}: {}\n", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

#[test]
fn sealed_mechanism_is_not_named_by_consumers() {
    // CWD for a workspace-root integration test is the gait-core crate root (gait-rs/).
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let needles = [
        "turn::spine",
        "turn::brain",
        "turn::hand",
        "relay_brain_turn",
        "SignalSlot",
        "ProfileInputs",
        "__brain",
    ];
    let mut leaks = String::new();
    for c in [
        "crates/gait-cli/src",
        "crates/gait-repl/src",
        "crates/gait-workflow/src",
    ] {
        scan(&root.join(c), &needles, &mut leaks);
    }
    assert!(
        leaks.trim().is_empty(),
        "sealed mechanism leaked into a consumer crate:\n{leaks}"
    );
}
