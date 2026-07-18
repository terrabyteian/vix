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

/// How a grep pattern is interpreted: as a verbatim substring or as a regex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternKind {
    Literal,
    Regex,
}

/// Build a smart-case matcher for `pattern`. Both kinds follow vix's
/// editor-search convention: any uppercase char in the pattern makes
/// matching case-sensitive, otherwise it's case-insensitive. `Literal`
/// escapes nothing — `fixed_strings` already matches metacharacters
/// (`.`, `*`, `(`, etc.) verbatim. `Regex` interprets them as regex
/// syntax and errors on invalid patterns. Shared by every grep entry
/// point so their matching semantics never drift apart.
pub fn build_matcher(pattern: &str, kind: PatternKind) -> anyhow::Result<RegexMatcher> {
    Ok(RegexMatcherBuilder::new()
        .fixed_strings(matches!(kind, PatternKind::Literal))
        .case_smart(true)
        .build(pattern)?)
}

/// Recursively grep `root` for `pattern` as a literal smart-case substring
/// (not a regex — see [`build_matcher`]). Returns per-line matches sorted
/// by `(path, line)` ascending.
///
/// Uses `WalkParallel` and one `Searcher` per worker thread, the same
/// pattern ripgrep itself uses. Worker threads send hits over an mpsc
/// channel; the calling thread drains it once the walk completes.
pub fn grep(root: &Path, pattern: &str) -> anyhow::Result<Vec<GrepItem>> {
    grep_cancellable(
        root,
        pattern,
        PatternKind::Literal,
        &Arc::new(AtomicU64::new(0)),
        0,
    )
}

/// Streaming variant of [`grep_cancellable`]: hits are delivered to
/// `on_batch` in chunks (every `GREP_BATCH` hits or `GREP_FLUSH_MS` ms)
/// while the parallel walk is still running, instead of one Vec at the
/// end. Takes a prebuilt matcher (see [`build_matcher`]) so pattern
/// errors surface at build time, before any worker spawns. Batches arrive
/// in nondeterministic order by design — the parallel walk delivers hits
/// as workers find them; imposing an order is the consumer's job.
/// `on_batch` returns `false` to stop early; generation cancellation
/// works exactly as in [`grep_cancellable`].
pub fn grep_streaming(
    root: &Path,
    matcher: Arc<RegexMatcher>,
    current: &Arc<AtomicU64>,
    target_gen: u64,
    mut on_batch: impl FnMut(Vec<GrepItem>) -> bool,
) {
    const GREP_BATCH: usize = 200;
    const GREP_FLUSH_MS: u64 = 30;
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
}

/// Cancellable variant of [`grep`], with the pattern interpreted per
/// `kind` (see [`build_matcher`]). The walker checks `current` against
/// `target_gen` between files and between matched lines; when they
/// diverge the walk quits early and returns whatever's been collected so
/// far, sorted by `(path, line)` ascending. Used by the picker so a fresh
/// keystroke can supersede an in-flight grep without waiting for the old
/// one to finish.
///
/// `current` is shared across grep generations: the caller bumps it
/// whenever a newer query comes in. A worker holds the value its request
/// was issued with (`target_gen`) and bails when the live value moves
/// past it.
pub fn grep_cancellable(
    root: &Path,
    pattern: &str,
    kind: PatternKind,
    current: &Arc<AtomicU64>,
    target_gen: u64,
) -> anyhow::Result<Vec<GrepItem>> {
    let matcher = Arc::new(build_matcher(pattern, kind)?);
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
    // Workers race, so arrival order is nondeterministic; sort so callers
    // get the same order ripgrep-style output would have.
    let mut hits: Vec<GrepItem> = rx.iter().collect();
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(hits)
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
        out.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
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

        let got: Vec<(PathBuf, u64, &str)> = hits
            .iter()
            .map(|h| (h.path.clone(), h.line, h.text.as_str()))
            .collect();
        let expected = vec![
            (dir.join("a.txt"), 2, "beta NEEDLE here"),
            (dir.join("sub/c.txt"), 1, "first NEEDLE"),
            (dir.join("sub/c.txt"), 2, "second NEEDLE"),
        ];
        assert_eq!(got, expected);
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

        let full: Vec<(PathBuf, u64, String)> = grep(&dir, "NEEDLE")
            .unwrap()
            .into_iter()
            .map(|h| (h.path, h.line, h.text))
            .collect();
        let gen = Arc::new(AtomicU64::new(3));
        let matcher = Arc::new(build_matcher("NEEDLE", PatternKind::Literal).unwrap());
        let mut streamed: Vec<(PathBuf, u64, String)> = Vec::new();
        grep_streaming(&dir, matcher, &gen, 3, |batch| {
            streamed.extend(batch.into_iter().map(|h| (h.path, h.line, h.text)));
            true
        });
        let _ = fs::remove_dir_all(&dir);

        // `grep` promises (path, line)-sorted output; only the streamed side
        // arrives in nondeterministic order (by design) and needs sorting.
        let mut expected = full.clone();
        expected.sort();
        assert_eq!(full, expected, "grep output must be (path, line)-sorted");
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

    /// Companion to [`grep_matches_pattern_literally_not_as_regex`]: the same
    /// pattern `a.b` under `PatternKind::Regex` treats `.` as "any char" and
    /// hits both lines; under `Literal` it still hits only the literal one.
    #[test]
    fn regex_matcher_matches_metacharacters() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-rx-meta-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dot.txt"), "has a.b in it\n").unwrap();
        fs::write(dir.join("nodot.txt"), "has axb in it\n").unwrap();

        let gen = Arc::new(AtomicU64::new(0));
        let as_regex = grep_cancellable(&dir, "a.b", PatternKind::Regex, &gen, 0).unwrap();
        let as_literal = grep_cancellable(&dir, "a.b", PatternKind::Literal, &gen, 0).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            as_regex.len(),
            2,
            "regex `.` should match both a.b and axb; got {as_regex:?}"
        );
        assert_eq!(
            as_literal.len(),
            1,
            "literal a.b should match only the literal line; got {as_literal:?}"
        );
        assert!(as_literal[0].text.contains("a.b"));
    }

    /// Regex patterns are smart-case too: all-lowercase matches
    /// case-insensitively, any uppercase char makes it case-sensitive.
    #[test]
    fn regex_matcher_smart_case() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-rx-case-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("upper.txt"), "found NEEDLE here\n").unwrap();
        fs::write(dir.join("lower.txt"), "found needle here\n").unwrap();

        let gen = Arc::new(AtomicU64::new(0));
        let lo = grep_cancellable(&dir, "need.e", PatternKind::Regex, &gen, 0).unwrap();
        let hi = grep_cancellable(&dir, "Need.e", PatternKind::Regex, &gen, 0).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            lo.len(),
            2,
            "lowercase regex should match both cases; got {lo:?}"
        );
        assert!(
            hi.is_empty(),
            "uppercase-containing regex must be case-sensitive; got {hi:?}"
        );
    }

    /// Invalid regex syntax errors under `Regex` but is a perfectly good
    /// literal under `Literal` (fixed_strings never parses it as regex).
    #[test]
    fn build_matcher_invalid_regex_errors() {
        assert!(build_matcher("[", PatternKind::Regex).is_err());
        assert!(build_matcher("[", PatternKind::Literal).is_ok());
    }

    /// `grep` promises (path, line)-ascending output regardless of the
    /// nondeterministic order the parallel workers deliver hits in. Run it
    /// twice: both runs must produce the same, explicitly ordered result.
    #[test]
    fn grep_output_is_sorted_by_path_then_line() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("vix-grep-sorted-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("b.txt"), "NEEDLE one\nfiller\nNEEDLE two\n").unwrap();
        fs::write(dir.join("a.txt"), "NEEDLE three\n").unwrap();
        fs::write(dir.join("sub/c.txt"), "NEEDLE four\nNEEDLE five\n").unwrap();

        let first = grep(&dir, "NEEDLE").unwrap();
        let second = grep(&dir, "NEEDLE").unwrap();
        let _ = fs::remove_dir_all(&dir);

        let keys = |hits: &[GrepItem]| -> Vec<(PathBuf, u64)> {
            hits.iter().map(|h| (h.path.clone(), h.line)).collect()
        };
        let expected = vec![
            (dir.join("a.txt"), 1),
            (dir.join("b.txt"), 1),
            (dir.join("b.txt"), 3),
            (dir.join("sub/c.txt"), 1),
            (dir.join("sub/c.txt"), 2),
        ];
        assert_eq!(keys(&first), expected);
        assert_eq!(
            keys(&first),
            keys(&second),
            "grep output must be deterministic across runs"
        );
    }
}
