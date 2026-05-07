//! Fuzzy pickers: file finder, global grep. The TUI owns overlay rendering
//! and input routing; this crate provides the data-layer slice: candidate
//! scanning and nucleo-backed scoring.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use grep_regex::RegexMatcher;
use grep_searcher::{sinks::UTF8, Searcher};
use ignore::{WalkBuilder, WalkState};
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher,
};

pub use nucleo_matcher::Utf32String;

/// A file candidate: its path relative to the search root.
#[derive(Clone, Debug)]
pub struct FileItem {
    pub rel_path: PathBuf,
    pub haystack: Utf32String,
}

/// A grep result: the path + line number + matched line text.
#[derive(Clone, Debug)]
pub struct GrepItem {
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
    pub haystack: Utf32String,
}

/// Walk `root` respecting `.gitignore` and return a deduped list of files.
pub fn scan_files(root: &Path) -> Vec<FileItem> {
    let mut out: Vec<FileItem> = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        let display = rel.to_string_lossy().into_owned();
        let haystack = Utf32String::from(display);
        out.push(FileItem {
            rel_path: rel,
            haystack,
        });
    }
    out
}

/// Recursively grep `root` for `pattern`. Returns per-line matches.
///
/// Uses `WalkParallel` and one `Searcher` per worker thread, the same
/// pattern ripgrep itself uses. Worker threads send hits over an mpsc
/// channel; the calling thread drains it once the walk completes.
pub fn grep(root: &Path, pattern: &str) -> anyhow::Result<Vec<GrepItem>> {
    let matcher = Arc::new(RegexMatcher::new(pattern)?);
    let root: Arc<Path> = Arc::from(root.to_path_buf().into_boxed_path());
    let (tx, rx) = mpsc::channel::<GrepItem>();

    let walker = WalkBuilder::new(&*root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build_parallel();

    walker.run(|| {
        // Per-worker state. `Searcher` keeps internal buffers we reuse
        // across files; the matcher is shared via Arc; the sender is cloned
        // so each worker has its own handle (the original is dropped after
        // `run` returns, which lets the receiver loop terminate).
        let tx = tx.clone();
        let matcher = Arc::clone(&matcher);
        let root = Arc::clone(&root);
        let mut searcher = Searcher::new();
        Box::new(move |result| {
            let entry = match result {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            let Some(ft) = entry.file_type() else {
                return WalkState::Continue;
            };
            if !ft.is_file() {
                return WalkState::Continue;
            }
            let path = entry.path().to_path_buf();
            let rel = path.strip_prefix(&*root).unwrap_or(&path).to_path_buf();
            let _ = searcher.search_path(
                &*matcher,
                &path,
                UTF8(|line_no, text| {
                    let text = text.trim_end_matches('\n').to_string();
                    let display = format!("{}:{}: {}", rel.display(), line_no, text);
                    // Receiver hung up = caller dropped the result. Nothing
                    // useful left to do; signal the searcher to stop this file.
                    if tx
                        .send(GrepItem {
                            path: path.clone(),
                            line: line_no,
                            text,
                            haystack: Utf32String::from(display),
                        })
                        .is_err()
                    {
                        return Ok(false);
                    }
                    Ok(true)
                }),
            );
            WalkState::Continue
        })
    });

    drop(tx);
    Ok(rx.iter().collect())
}

/// Compute the matched character positions for `haystack` against `query`.
/// Returns char indices into `haystack`, sorted and deduped. Empty query →
/// empty vec. Used by the picker UI to bold matched chars in a row.
pub fn match_indices(haystack: &Utf32String, query: &str) -> Vec<u32> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut indices: Vec<u32> = Vec::new();
    pattern.indices(haystack.slice(..), &mut matcher, &mut indices);
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Score a corpus against `query` and return the top-N by score, descending.
/// Items with no match are filtered out. On empty query, returns the first
/// `limit` items in input order (score 0).
pub fn score<T: Clone>(items: &[(T, Utf32String)], query: &str, limit: usize) -> Vec<(T, u32)> {
    if query.is_empty() {
        return items
            .iter()
            .take(limit)
            .map(|(v, _)| (v.clone(), 0))
            .collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored: Vec<(T, u32)> = Vec::with_capacity(items.len());
    for (val, haystack) in items {
        if let Some(score) = pattern.score(haystack.slice(..), &mut matcher) {
            scored.push((val.clone(), score));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_orders_by_quality() {
        let items: Vec<(String, Utf32String)> =
            ["src/main.rs", "src/foo/bar.rs", "Cargo.toml", "README.md"]
                .iter()
                .map(|s| (s.to_string(), Utf32String::from(*s)))
                .collect();
        let hits = score(&items, "main", 10);
        assert!(!hits.is_empty());
        assert!(
            hits[0].0.contains("main"),
            "expected main.rs first; got {:?}",
            hits
        );
    }

    #[test]
    fn score_empty_query_returns_input_order() {
        let items: Vec<(String, Utf32String)> = ["a", "b", "c"]
            .iter()
            .map(|s| (s.to_string(), Utf32String::from(*s)))
            .collect();
        let hits = score(&items, "", 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, "a");
    }

    /// Lay out a small tree under a unique temp dir, run `grep`, and confirm
    /// matches across multiple files come back. Exercises the parallel walker:
    /// without per-worker `Searcher`s and channel plumbing this would either
    /// fail to compile or race on shared state.
    #[test]
    fn grep_finds_matches_across_multiple_files() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "alpha\nbeta NEEDLE here\ngamma\n").unwrap();
        fs::write(dir.join("b.txt"), "no match here\n").unwrap();
        fs::write(dir.join("sub/c.txt"), "first NEEDLE\nsecond NEEDLE\n").unwrap();

        let hits = grep(&dir, "NEEDLE").unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            hits.len(),
            3,
            "expected 3 hits across 2 files; got {hits:?}"
        );
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert!(texts.contains(&"beta NEEDLE here"));
        assert!(texts.contains(&"first NEEDLE"));
        assert!(texts.contains(&"second NEEDLE"));
    }

    #[test]
    fn grep_skips_dot_git() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-git-{}", std::process::id()));
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "MARKER\n").unwrap();
        fs::write(dir.join("real.txt"), "MARKER\n").unwrap();

        let hits = grep(&dir, "MARKER").unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(hits.len(), 1, "expected to skip .git; got {hits:?}");
        assert!(hits[0].path.ends_with("real.txt"));
    }
}
