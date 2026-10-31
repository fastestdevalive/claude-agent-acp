//! Hermeticity guard (rust-parity-fixes Fix 3): the adapter reads process
//! environment variables ONLY through `env_snapshot.rs` (hermetic snapshot) and
//! `process.rs` (spawn-time argv/env defaults). A direct `std::env::var*`
//! variable read anywhere else in `src/` risks host-environment pollution and
//! breaks every polluted-env gate run.

use std::path::Path;

/// The env-VARIABLE read patterns that must stay confined to `env_snapshot.rs`
/// and `process.rs`. `std::env::current_dir`/`temp_dir` are NOT flagged: they
/// read the working directory / OS temp, not an environment variable, and are
/// used legitimately elsewhere (e.g. a logout command's cwd).
const READ_PATTERNS: [&str; 4] = [
    "std::env::var(",
    "std::env::vars(",
    "std::env::var_os(",
    "std::env::vars_os(",
];

#[test]
fn inv_69_no_direct_env_var_reads_outside_snapshot_and_process() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let src = root.join("rust").join("claude-agent-acp-rs").join("src");
    assert!(src.is_dir(), "src dir should exist at {src:?}");

    let mut offenders: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&src).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "env_snapshot.rs" || name == "process.rs" {
            continue;
        }
        // `*.rs` suffixed test modules live inline in the same file (mod tests),
        // so a line is only OK if `std::env::` appears in a comment/doc.
        let text = std::fs::read_to_string(&path).expect("read source file");
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue; // comment / doc mention
            }
            if READ_PATTERNS.iter().any(|p| line.contains(p)) {
                offenders.push(format!("{}:{}: {}", path.display(), idx + 1, trimmed));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "direct env-variable reads outside env_snapshot.rs / process.rs:\n{}",
        offenders.join("\n")
    );
}
