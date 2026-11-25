//! Guard: provider API-key env var names may live ONLY in the single table
//! `gait_provider::providers::PROVIDERS` (gait-rs/crates/gait-provider/src/providers.rs).
//! One table for key names: no other production code may spell an
//! `*_API_KEY` literal or derive one via `format!("{}_API_KEY")`.

use std::fs;
use std::path::{Path, PathBuf};

const GUARD_FILE: &str = "provider_key_names_one_table.rs";
const TABLE_FILE: &str = "providers.rs";

/// Production-code prefix of `text`: everything before the first
/// `#[cfg(test)]` line whose next line opens a module (`mod`, `pub mod`, or
/// `pub(crate) mod`). A bare mid-file `#[cfg(test)]` on a single item does
/// NOT end the production part.
fn production_part(text: &str) -> &str {
    let lines: Vec<&str> = text.lines().collect();
    let mut cut: Option<usize> = None; // byte offset where production ends
    let mut offset = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let start = offset;
        offset += line.len() + 1; // +1 for the '\n' (last line may lack it)
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        if let Some(next) = lines.get(i + 1) {
            let t = next.trim_start();
            if t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("pub(crate) mod ")
            {
                cut = Some(start);
                break;
            }
        }
    }
    match cut {
        Some(i) => &text[..i],
        None => text,
    }
}

fn scan(dir: &Path, out: &mut String) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            // Skip test directories entirely.
            if p.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            scan(&p, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str());
        if name == Some(GUARD_FILE) || name == Some(TABLE_FILE) || name == Some("tests.rs") {
            continue;
        }
        let text = fs::read_to_string(&p).unwrap();
        // Only production code: stop at the first `#[cfg(test)]` line whose
        // next line opens a test module.
        let prod = production_part(&text);
        // Needles built via concat! so this guard file never matches itself.
        let lit_needle = concat!("_API_", "KEY\"");
        let fmt_needle = concat!("}_API_", "KEY");
        for (i, line) in prod.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(lit_needle) || code.contains(fmt_needle) {
                out.push_str(&format!("{}:{}: {}\n", p.display(), i + 1, line.trim()));
            }
        }
    }
}

#[test]
fn provider_key_names_live_in_one_table() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = String::new();
    scan(&root.join("src"), &mut offenders);
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let crate_dir = entry.unwrap().path().join("src");
        scan(&crate_dir, &mut offenders);
    }
    assert!(
        offenders.trim().is_empty(),
        "provider API-key env var names must live ONLY in \
         crates/gait-provider/src/providers.rs:\n{offenders}"
    );
}

#[test]
fn table_env_names_are_unique_and_well_formed() {
    let mut empty_or_malformed = String::new();
    let mut seen = std::collections::BTreeMap::new();
    let mut dups = std::collections::BTreeMap::new();
    let suffix = concat!("_API_", "KEY");
    for p in gait_provider::providers::PROVIDERS {
        if let Some(env) = p.env_var {
            // Only specs that HAVE an env var are constrained; a `None`
            // env_var (subscription-only providers) is legitimate.
            if !env.is_empty() && env.ends_with(suffix) {
                let prev = seen.insert(env, p.id);
                if let Some(first) = prev {
                    dups.entry(env).or_insert(first);
                }
            } else {
                empty_or_malformed.push_str(&format!(
                    "provider `{}` has malformed env_var {env:?} (must be non-empty and end with {suffix})\n",
                    p.id
                ));
            }
        }
    }
    let dup_report: String = dups
        .iter()
        .map(|(env, first)| format!("{env} shared by `{first}` and later providers\n"))
        .collect();
    assert!(
        empty_or_malformed.trim().is_empty() && dup_report.trim().is_empty(),
        "PROVIDERS table env-var problems:\n{}{}",
        empty_or_malformed,
        dup_report
    );
}
