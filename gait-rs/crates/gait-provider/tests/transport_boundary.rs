use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[(&str, &str)] = &[
    (
        "reqwest::Client",
        "raw reqwest clients belong in transport.rs",
    ),
    (".send().await", "HTTP execution belongs in transport.rs"),
    (
        "bytes_stream().eventsource()",
        "raw SSE opening belongs in transport.rs",
    ),
    (
        "connect_and_first_event",
        "retry composition belongs in transport.rs",
    ),
    ("guard_idle_sse", "deadline guards belong in transport.rs"),
];

#[test]
fn production_provider_code_cannot_bypass_transport() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);

    for file in files {
        if file.file_name().and_then(|name| name.to_str()) == Some("transport.rs") {
            continue;
        }
        let source = fs::read_to_string(&file).expect("read provider source");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production prefix");
        let without_comments = production
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let flattened: String = without_comments
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();

        for &(pattern, explanation) in FORBIDDEN {
            let compact_pattern: String =
                pattern.chars().filter(|ch| !ch.is_whitespace()).collect();
            if let Some(offset) = flattened.find(&compact_pattern) {
                let line = approximate_line(&without_comments, offset);
                panic!(
                    "{}:{} violates the provider transport boundary: {explanation}; provider requests must execute through ProviderTransport",
                    file.display(),
                    line,
                );
            }
        }
    }
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn approximate_line(source: &str, compact_offset: usize) -> usize {
    let mut compact_seen = 0usize;
    for (line_index, line) in source.lines().enumerate() {
        compact_seen += line.chars().filter(|ch| !ch.is_whitespace()).count();
        if compact_seen > compact_offset {
            return line_index + 1;
        }
    }
    source.lines().count().max(1)
}
