//! Golden corpus access (§12.2).
//!
//! `corpus/` holds real captured logs per firmware. A profile PR must include
//! corpus samples plus goldens for every edge case it claims to handle — this
//! module is how tests reach them, and it fails loudly rather than skipping when
//! a file is missing, so a deleted corpus file can never quietly disable a test.

use std::path::{Path, PathBuf};

/// Repository `corpus/` directory, located relative to this crate.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus")
        .canonicalize()
        .unwrap_or_else(|e| panic!("corpus/ must exist next to the workspace root: {e}"))
}

/// Read one corpus file, by path relative to `corpus/`.
pub fn corpus_file(rel: &str) -> Vec<u8> {
    let p = corpus_dir().join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("corpus file {} missing: {e}", p.display()))
}

/// Read one corpus file as UTF-8-lossy text.
pub fn corpus_text(rel: &str) -> String {
    String::from_utf8_lossy(&corpus_file(rel)).into_owned()
}

/// Every `*.log` under a corpus subdirectory, sorted for determinism.
pub fn corpus_files(subdir: &str) -> Vec<PathBuf> {
    let dir = corpus_dir().join(subdir);
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("corpus dir {} missing: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    v.sort();
    assert!(!v.is_empty(), "corpus/{subdir} has no .log files");
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_reachable_from_tests() {
        assert!(corpus_dir().is_dir());
        assert!(!corpus_files("linux").is_empty());
    }

    #[test]
    #[should_panic(expected = "missing")]
    fn a_missing_corpus_file_fails_loudly_rather_than_skipping() {
        corpus_file("linux/definitely-not-here.log");
    }
}
