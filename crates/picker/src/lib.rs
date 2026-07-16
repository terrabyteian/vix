//! Fuzzy pickers: file finder, global grep. The TUI owns overlay rendering
//! and input routing; this crate provides the data-layer slice: candidate
//! scanning and nucleo-backed scoring.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
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

/// Streaming variant of [`scan_files`]: walks `root` and hands out batches
/// of files as they're discovered instead of one big Vec at the end, so a
/// UI can show results immediately on huge repos. Batches flush every
/// `SCAN_BATCH` entries or `SCAN_FLUSH_MS` ms, whichever comes first.
///
/// `on_batch` returns `false` to stop the walk (e.g. the consumer hung up).
/// The walk also stops when `current` moves past `target_gen` — the same
/// shared-generation cancellation used by [`grep_cancellable`].
pub fn scan_files_streaming(
    root: &Path,
    current: &Arc<AtomicU64>,
    target_gen: u64,
    mut on_batch: impl FnMut(Vec<FileItem>) -> bool,
) {
    const SCAN_BATCH: usize = 500;
    const SCAN_FLUSH_MS: u64 = 25;
    let mut batch: Vec<FileItem> = Vec::with_capacity(SCAN_BATCH);
    let mut last_flush = std::time::Instant::now();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        if current.load(Ordering::Relaxed) != target_gen {
            return;
        }
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
        batch.push(FileItem {
            rel_path: rel,
            haystack,
        });
        if batch.len() >= SCAN_BATCH
            || (!batch.is_empty() && last_flush.elapsed().as_millis() as u64 >= SCAN_FLUSH_MS)
        {
            if !on_batch(std::mem::take(&mut batch)) {
                return;
            }
            batch.reserve(SCAN_BATCH);
            last_flush = std::time::Instant::now();
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
}

/// Build a literal smart-case substring matcher for `pattern`: no regex
/// metacharacters are special (`.`, `*`, `(`, etc. all match themselves),
/// and case sensitivity follows the query — all-lowercase matches
/// case-insensitively, any uppercase char makes it case-sensitive. Shared by
/// every grep entry point so their matching semantics never drift apart.
fn literal_matcher(pattern: &str) -> anyhow::Result<RegexMatcher> {
    Ok(RegexMatcherBuilder::new()
        .fixed_strings(true)
        .case_smart(true)
        .build(pattern)?)
}

/// Recursively grep `root` for `pattern` as a literal smart-case substring
/// (not a regex — see [`literal_matcher`]). Returns per-line matches.
///
/// Uses `WalkParallel` and one `Searcher` per worker thread, the same
/// pattern ripgrep itself uses. Worker threads send hits over an mpsc
/// channel; the calling thread drains it once the walk completes.
pub fn grep(root: &Path, pattern: &str) -> anyhow::Result<Vec<GrepItem>> {
    grep_cancellable(root, pattern, &Arc::new(AtomicU64::new(0)), 0)
}

/// Streaming variant of [`grep_cancellable`]: hits are delivered to
/// `on_batch` in chunks (every `GREP_BATCH` hits or `GREP_FLUSH_MS` ms)
/// while the parallel walk is still running, instead of one Vec at the
/// end. `on_batch` returns `false` to stop early; generation cancellation
/// works exactly as in [`grep_cancellable`].
pub fn grep_streaming(
    root: &Path,
    pattern: &str,
    current: &Arc<AtomicU64>,
    target_gen: u64,
    mut on_batch: impl FnMut(Vec<GrepItem>) -> bool,
) -> anyhow::Result<()> {
    const GREP_BATCH: usize = 200;
    const GREP_FLUSH_MS: u64 = 30;
    let matcher = Arc::new(literal_matcher(pattern)?);
    let root: Arc<Path> = Arc::from(root.to_path_buf().into_boxed_path());
    let (tx, rx) = mpsc::channel::<GrepItem>();

    let walker = WalkBuilder::new(&*root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build_parallel();

    std::thread::scope(|scope| {
        // The walk runs on a scoped thread; this thread drains per-hit
        // messages into batches as they arrive. Dropping `rx` (on early
        // stop) makes worker sends fail, which stops their per-file search.
        let current_walk = Arc::clone(current);
        let matcher = Arc::clone(&matcher);
        let walk_root = Arc::clone(&root);
        scope.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                let matcher = Arc::clone(&matcher);
                let root = Arc::clone(&walk_root);
                let current = Arc::clone(&current_walk);
                let mut searcher = Searcher::new();
                Box::new(move |result| {
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
            // Workers' sender clones drop as `run` returns; the original was
            // moved into the closure factory and drops with it, so the drain
            // loop below sees Disconnected once the walk is done.
        });

        let mut batch: Vec<GrepItem> = Vec::with_capacity(GREP_BATCH);
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(GREP_FLUSH_MS)) {
                Ok(item) => {
                    batch.push(item);
                    if batch.len() >= GREP_BATCH && !on_batch(std::mem::take(&mut batch)) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !batch.is_empty() && !on_batch(std::mem::take(&mut batch)) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !batch.is_empty() {
                        on_batch(batch);
                    }
                    return;
                }
            }
        }
    });
    Ok(())
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
    let matcher = Arc::new(literal_matcher(pattern)?);
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

    #[test]
    fn scan_files_streaming_delivers_same_set_as_scan_files() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-scan-stream-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        for i in 0..12 {
            fs::write(dir.join(format!("f{i}.txt")), "x\n").unwrap();
        }
        fs::write(dir.join("sub/nested.txt"), "x\n").unwrap();

        let full: Vec<_> = scan_files(&dir).into_iter().map(|f| f.rel_path).collect();
        let gen = Arc::new(AtomicU64::new(7));
        let mut streamed: Vec<std::path::PathBuf> = Vec::new();
        scan_files_streaming(&dir, &gen, 7, |batch| {
            streamed.extend(batch.into_iter().map(|f| f.rel_path));
            true
        });
        let _ = fs::remove_dir_all(&dir);

        let mut a = full;
        let mut b = streamed;
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn scan_files_streaming_stops_on_generation_bump() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-scan-cancel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            fs::write(dir.join(format!("f{i}.txt")), "x\n").unwrap();
        }
        // Generation already moved past the target: nothing is delivered.
        let gen = Arc::new(AtomicU64::new(2));
        let mut called = false;
        scan_files_streaming(&dir, &gen, 1, |_batch| {
            called = true;
            true
        });
        let _ = fs::remove_dir_all(&dir);
        assert!(!called, "cancelled scan must not deliver batches");
    }

    #[test]
    fn grep_streaming_delivers_same_hits_as_grep() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-stream-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "alpha\nbeta NEEDLE here\ngamma\n").unwrap();
        fs::write(dir.join("sub/c.txt"), "first NEEDLE\nsecond NEEDLE\n").unwrap();

        let mut full: Vec<String> = grep(&dir, "NEEDLE")
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        let gen = Arc::new(AtomicU64::new(3));
        let mut streamed: Vec<String> = Vec::new();
        grep_streaming(&dir, "NEEDLE", &gen, 3, |batch| {
            streamed.extend(batch.into_iter().map(|h| h.text));
            true
        })
        .unwrap();
        let _ = fs::remove_dir_all(&dir);

        full.sort();
        streamed.sort();
        assert_eq!(full, streamed);
    }

    /// A pattern containing a regex metacharacter must match only the
    /// literal text, not the metacharacter's regex meaning — `a.b` should
    /// hit a line with literal `a.b` and not a line with `axb` (which a
    /// real regex `.` would also match).
    #[test]
    fn grep_matches_pattern_literally_not_as_regex() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-literal-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dot.txt"), "has a.b in it\n").unwrap();
        fs::write(dir.join("nodot.txt"), "has axb in it\n").unwrap();

        let hits = grep(&dir, "a.b").unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            hits.len(),
            1,
            "expected only the literal a.b line; got {hits:?}"
        );
        assert!(hits[0].text.contains("a.b"));
    }

    /// Smart-case: an all-lowercase pattern matches case-insensitively.
    #[test]
    fn grep_lowercase_pattern_matches_case_insensitively() {
        use std::fs;
        let dir =
            std::env::temp_dir().join(format!("vix-grep-smartcase-lo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "found NEEDLE here\n").unwrap();

        let hits = grep(&dir, "needle").unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            hits.len(),
            1,
            "lowercase query should match uppercase text; got {hits:?}"
        );
    }

    /// Smart-case: a pattern with an uppercase char matches case-sensitively,
    /// so it must not match a line that only has the lowercase spelling.
    #[test]
    fn grep_mixed_case_pattern_matches_case_sensitively() {
        use std::fs;
        let dir =
            std::env::temp_dir().join(format!("vix-grep-smartcase-hi-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "found needle here\n").unwrap();

        let hits = grep(&dir, "Needle").unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(
            hits.is_empty(),
            "uppercase-containing query must not match lowercase-only text; got {hits:?}"
        );
    }
}
