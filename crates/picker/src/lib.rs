//! Fuzzy pickers: file finder, global grep. The TUI owns overlay rendering
//! and input routing; this crate provides the data-layer slice: candidate
//! scanning and nucleo-backed scoring.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcher;
use grep_searcher::{sinks::UTF8, Searcher};
use ignore::WalkBuilder;
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
        let Some(ft) = entry.file_type() else { continue };
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
pub fn grep(root: &Path, pattern: &str) -> anyhow::Result<Vec<GrepItem>> {
    let matcher = RegexMatcher::new(pattern)?;
    let mut out: Vec<GrepItem> = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let Some(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let mut searcher = Searcher::new();
        let _ = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_no, text| {
                let text = text.trim_end_matches('\n').to_string();
                let display = format!("{}:{}: {}", rel.display(), line_no, text);
                out.push(GrepItem {
                    path: path.clone(),
                    line: line_no,
                    text,
                    haystack: Utf32String::from(display),
                });
                Ok(true)
            }),
        );
    }
    Ok(out)
}

/// Score a corpus against `query` and return the top-N by score, descending.
/// Items with no match are filtered out. On empty query, returns the first
/// `limit` items in input order (score 0).
pub fn score<T: Clone>(
    items: &[(T, Utf32String)],
    query: &str,
    limit: usize,
) -> Vec<(T, u32)> {
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
        let items: Vec<(String, Utf32String)> = [
            "src/main.rs",
            "src/foo/bar.rs",
            "Cargo.toml",
            "README.md",
        ]
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
}
