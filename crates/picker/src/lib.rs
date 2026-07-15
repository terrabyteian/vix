//! Fuzzy pickers: file finder, global grep. The TUI owns overlay rendering
//! and input routing; this crate provides the data-layer slice: candidate
//! scanning and nucleo-backed scoring.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    grep_cancellable(root, pattern, &Arc::new(AtomicU64::new(0)), 0)
}

/// Cancellable variant of [`grep`]. The walker checks `current` against
/// `target_gen` between files and between matched lines; when they
/// diverge the walk quits early and returns whatever's been collected so
/// far. Used by the picker so a fresh keystroke can supersede an
/// in-flight grep without waiting for the old one to finish.
///
/// `current` is shared across grep generations: the caller bumps it
/// whenever a newer query comes in. A worker holds the value its request
/// was issued with (`target_gen`) and bails when the live value moves
/// past it.
pub fn grep_cancellable(
    root: &Path,
    pattern: &str,
    current: &Arc<AtomicU64>,
    target_gen: u64,
) -> anyhow::Result<Vec<GrepItem>> {
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
        let current = Arc::clone(current);
        let mut searcher = Searcher::new();
        Box::new(move |result| {
            // Cheapest cancel check: per-file. Per-line is also wired below
            // for files with thousands of matches, but most cancels land
            // here on the next directory entry.
            if current.load(Ordering::Relaxed) != target_gen {
                return WalkState::Quit;
            }
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
                    if current.load(Ordering::Relaxed) != target_gen {
                        return Ok(false);
                    }
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

/// Persistent nucleo scorer: owns a `Matcher` (its scratch buffers) and
/// caches the most recently parsed `Pattern` by query string, so a
/// keystroke-driven rescore doesn't allocate a fresh `Matcher` — and, when
/// the query hasn't changed since the last call, doesn't re-parse the
/// pattern either. Callers (the picker overlay) hold one `Scorer` for the
/// lifetime of a picker session rather than building one per call.
pub struct Scorer {
    matcher: Matcher,
    pattern: Option<(String, Pattern)>,
}

impl Default for Scorer {
    fn default() -> Self {
        Self::new()
    }
}

impl Scorer {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            pattern: None,
        }
    }

    /// Return the parsed `Pattern` for `query`, reusing the cached one if
    /// `query` matches what was cached last time. Re-parses (and replaces
    /// the cache) on any change.
    fn pattern_for(&mut self, query: &str) -> &Pattern {
        let stale = match &self.pattern {
            Some((cached, _)) => cached != query,
            None => true,
        };
        if stale {
            let parsed = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
            self.pattern = Some((query.to_string(), parsed));
        }
        &self.pattern.as_ref().unwrap().1
    }

    /// Index-keyed scoring: writes `(item_index, score)` for matching
    /// haystacks into `out`, sorted descending by score and capped at
    /// `limit`. Reuses `out`'s existing capacity so callers can reuse the
    /// same buffer across keystrokes.
    ///
    /// Empty `query` produces `(index, 0)` for the first `limit` entries in
    /// input order.
    pub fn rescore_indices<'a>(
        &mut self,
        haystacks: impl Iterator<Item = (usize, &'a Utf32String)>,
        query: &str,
        limit: usize,
        out: &mut Vec<(usize, u32)>,
    ) {
        out.clear();
        if query.is_empty() {
            for (i, _) in haystacks.take(limit) {
                out.push((i, 0));
            }
            return;
        }
        // Clone the (small) parsed pattern so the `&mut self.matcher` used
        // below doesn't conflict with the `&Pattern` borrow returned here.
        let pattern = self.pattern_for(query).clone();
        for (i, h) in haystacks {
            if let Some(s) = pattern.score(h.slice(..), &mut self.matcher) {
                out.push((i, s));
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out.truncate(limit);
    }

    /// Compute the matched character positions for `haystack` against
    /// `query`. Returns char indices into `haystack`, sorted and deduped.
    /// Empty query → empty vec. Used by the picker UI to bold matched chars
    /// in a row.
    pub fn match_indices(&mut self, haystack: &Utf32String, query: &str) -> Vec<u32> {
        if query.is_empty() {
            return Vec::new();
        }
        let pattern = self.pattern_for(query).clone();
        let mut indices: Vec<u32> = Vec::new();
        pattern.indices(haystack.slice(..), &mut self.matcher, &mut indices);
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scorer_rescore_indices_orders_by_quality() {
        let displays = ["src/main.rs", "src/foo/bar.rs", "Cargo.toml", "README.md"];
        let haystacks: Vec<Utf32String> = displays.iter().map(|s| Utf32String::from(*s)).collect();
        let mut scorer = Scorer::new();
        let mut out = Vec::new();
        scorer.rescore_indices(haystacks.iter().enumerate(), "main", 10, &mut out);
        assert!(!out.is_empty());
        assert!(
            displays[out[0].0].contains("main"),
            "expected main.rs first; got {:?}",
            out
        );
    }

    #[test]
    fn scorer_rescore_indices_empty_query_returns_input_order() {
        let displays = ["a", "b", "c"];
        let haystacks: Vec<Utf32String> = displays.iter().map(|s| Utf32String::from(*s)).collect();
        let mut scorer = Scorer::new();
        let mut out = Vec::new();
        scorer.rescore_indices(haystacks.iter().enumerate(), "", 10, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, 0);
    }

    #[test]
    fn scorer_rescore_indices_reuses_cached_pattern_across_calls() {
        // Same query on consecutive calls should hit the pattern cache
        // (see `Scorer::pattern_for`) and still produce identical results.
        let displays = ["alpha", "beta", "alphabet"];
        let haystacks: Vec<Utf32String> = displays.iter().map(|s| Utf32String::from(*s)).collect();
        let mut scorer = Scorer::new();
        let mut out = Vec::new();
        scorer.rescore_indices(haystacks.iter().enumerate(), "alpha", 10, &mut out);
        let first = out.clone();
        scorer.rescore_indices(haystacks.iter().enumerate(), "alpha", 10, &mut out);
        assert_eq!(first, out);
    }

    #[test]
    fn scorer_match_indices_marks_matched_chars() {
        let mut scorer = Scorer::new();
        let haystack = Utf32String::from("main.rs");
        let idx = scorer.match_indices(&haystack, "main");
        assert!(!idx.is_empty());
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
