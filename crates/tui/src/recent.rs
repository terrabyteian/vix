//! Recent-files persistence: a small dep-free store recording, per project
//! directory, which files were opened and when. Used to seed a "recent
//! files" picker. All I/O here is best-effort — a missing or unwritable
//! store degrades to "no recent files" rather than surfacing an error.
//!
//! File format: UTF-8 text, one entry per line, tab-separated:
//! `<unix_epoch_secs>\t<project_abs_dir>\t<rel_path>`. Lines starting with
//! `#` and malformed lines (wrong field count, non-numeric epoch) are
//! skipped on read rather than treated as errors, so a hand-edited or
//! partially-written file degrades gracefully instead of losing everything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Max entries retained per project. Older entries for that project are
/// dropped first when a write would exceed it.
pub(crate) const RECENT_PER_PROJECT_CAP: usize = 50;
/// Max entries retained in the file overall, across all projects. Bounds
/// the file's size regardless of how many projects have been visited.
pub(crate) const RECENT_GLOBAL_CAP: usize = 1000;

struct Entry {
    epoch: u64,
    project: String,
    rel: String,
}

/// Resolve where the recent-files store lives, preferring a more specific
/// override at each step: `$VIX_DATA_DIR/recent`, then
/// `$XDG_DATA_HOME/vix/recent`, then `$HOME/.local/share/vix/recent`. `None`
/// means none of those are set — persistence is disabled for this run
/// rather than guessing at a location.
pub(crate) fn default_data_file() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("VIX_DATA_DIR") {
        return Some(PathBuf::from(dir).join("recent"));
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return Some(PathBuf::from(dir).join("vix").join("recent"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home).join(".local/share/vix/recent"));
    }
    None
}

/// Parse `content` tolerantly: any line that isn't exactly
/// `epoch\tproject\trel` with a numeric epoch is dropped silently. Never
/// errors — a corrupt file just yields fewer entries.
fn parse_entries(content: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in content.lines() {
        if line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 3 {
            continue;
        }
        let Ok(epoch) = fields[0].parse::<u64>() else {
            continue;
        };
        out.push(Entry {
            epoch,
            project: fields[1].to_string(),
            rel: fields[2].to_string(),
        });
    }
    out
}

fn render_entries(entries: &[Entry]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str(&e.epoch.to_string());
        s.push('\t');
        s.push_str(&e.project);
        s.push('\t');
        s.push_str(&e.rel);
        s.push('\n');
    }
    s
}

/// Load recent entries for `project` from `file`, most-recently-recorded
/// first, deduped by `rel` (a file recorded twice keeps only its newest
/// timestamp), capped at [`RECENT_PER_PROJECT_CAP`]. A missing or
/// unreadable file yields an empty list rather than an error.
///
/// Consumed by `Editor::open_omnibox`, which turns the result into the omni
/// picker's `recent_rank` ranking bonus and `recent_items` empty-query
/// recents view (see `Picker::rescore_omni`).
pub(crate) fn load_recent_from(file: &Path, project: &Path) -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let project_str = project.to_string_lossy();
    let mut entries: Vec<Entry> = parse_entries(&content)
        .into_iter()
        .filter(|e| e.project == project_str)
        .collect();
    // Stable: entries were already written newest-first, so ties (possible
    // when several records land in the same second) keep that order.
    entries.sort_by(|a, b| b.epoch.cmp(&a.epoch));

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(RECENT_PER_PROJECT_CAP);
    for e in entries {
        if seen.insert(e.rel.clone()) {
            out.push(PathBuf::from(e.rel));
            if out.len() >= RECENT_PER_PROJECT_CAP {
                break;
            }
        }
    }
    out
}

/// Record that `rel` (relative to `project`) was opened, in `file`.
/// Best-effort: every I/O failure is swallowed and this simply does
/// nothing. Refuses (silently) if `project` or `rel` contain a tab or
/// newline, since those would corrupt the line-oriented format.
///
/// Read-modify-write: parses the existing file tolerantly, upserts this
/// entry (deduped by `(project, rel)`, timestamped with the current time),
/// enforces the per-project and global caps, then writes to a sibling
/// `recent.tmp` and renames it over `file` — so a crash mid-write can't
/// leave a truncated store behind.
pub(crate) fn record_recent_in(file: &Path, project: &Path, rel: &Path) {
    let project_str = project.to_string_lossy().into_owned();
    let rel_str = rel.to_string_lossy().into_owned();
    if project_str.contains('\t')
        || project_str.contains('\n')
        || rel_str.contains('\t')
        || rel_str.contains('\n')
    {
        return;
    }
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let existing = std::fs::read_to_string(file).unwrap_or_default();
    // Stored (and parsed) newest-first; flip to oldest-first so appending
    // the new entry at the end keeps the vec in chronological order —
    // needed so same-second ties break by recency, not file position.
    let mut entries = parse_entries(&existing);
    entries.reverse();
    entries.retain(|e| !(e.project == project_str && e.rel == rel_str));
    entries.push(Entry {
        epoch,
        project: project_str.clone(),
        rel: rel_str,
    });

    // Sort newest-first: primary key epoch descending, tie-broken by
    // chronological (append) order descending.
    let mut indexed: Vec<(usize, Entry)> = entries.into_iter().enumerate().collect();
    indexed.sort_by(|a, b| b.1.epoch.cmp(&a.1.epoch).then(b.0.cmp(&a.0)));
    let mut entries: Vec<Entry> = indexed.into_iter().map(|(_, e)| e).collect();

    // Per-project cap: keep each project's newest RECENT_PER_PROJECT_CAP
    // entries, dropping its oldest first.
    let mut per_project: HashMap<String, usize> = HashMap::new();
    entries.retain(|e| {
        let count = per_project.entry(e.project.clone()).or_insert(0);
        *count += 1;
        *count <= RECENT_PER_PROJECT_CAP
    });

    // Global cap: list is newest-first, so truncating drops the oldest.
    entries.truncate(RECENT_GLOBAL_CAP);

    let Some(parent) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = parent.join("recent.tmp");
    if std::fs::write(&tmp, render_entries(&entries)).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, file);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir per test so parallel test runs (same pid) never
    /// collide, mirroring `vix_picker`'s temp-dir tests.
    fn test_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("vix-recent-test-{}-{name}-{n}", std::process::id()))
    }

    #[test]
    fn roundtrip_record_then_load() {
        let dir = test_dir("roundtrip");
        let file = dir.join("recent");
        let project = dir.join("proj");

        record_recent_in(&file, &project, Path::new("src/main.rs"));
        let got = load_recent_from(&file, &project);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn per_project_scoping_does_not_leak() {
        let dir = test_dir("scoping");
        let file = dir.join("recent");
        let proj_a = dir.join("a");
        let proj_b = dir.join("b");

        record_recent_in(&file, &proj_a, Path::new("a.rs"));
        record_recent_in(&file, &proj_b, Path::new("b.rs"));
        let got_a = load_recent_from(&file, &proj_a);
        let got_b = load_recent_from(&file, &proj_b);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got_a, vec![PathBuf::from("a.rs")]);
        assert_eq!(got_b, vec![PathBuf::from("b.rs")]);
    }

    #[test]
    fn most_recent_first_ordering() {
        let dir = test_dir("order");
        let file = dir.join("recent");
        let project = dir.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let proj_s = project.to_string_lossy();
        let content =
            format!("100\t{proj_s}\told.rs\n200\t{proj_s}\tmid.rs\n300\t{proj_s}\tnew.rs\n");
        std::fs::write(&file, content).unwrap();

        let got = load_recent_from(&file, &project);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            got,
            vec![
                PathBuf::from("new.rs"),
                PathBuf::from("mid.rs"),
                PathBuf::from("old.rs"),
            ]
        );
    }

    #[test]
    fn recording_same_file_twice_dedupes_to_newest() {
        let dir = test_dir("dedupe");
        let file = dir.join("recent");
        let project = dir.join("proj");

        record_recent_in(&file, &project, Path::new("other.rs"));
        record_recent_in(&file, &project, Path::new("new.rs"));
        record_recent_in(&file, &project, Path::new("new.rs"));
        let got = load_recent_from(&file, &project);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            got.iter()
                .filter(|p| *p == &PathBuf::from("new.rs"))
                .count(),
            1,
            "expected exactly one entry for the twice-recorded file; got {got:?}"
        );
        assert_eq!(
            got[0],
            PathBuf::from("new.rs"),
            "newest record should sort first"
        );
    }

    #[test]
    fn per_project_cap_is_enforced() {
        let dir = test_dir("cap");
        let file = dir.join("recent");
        let project = dir.join("proj");

        for i in 0..(RECENT_PER_PROJECT_CAP + 10) {
            record_recent_in(&file, &project, Path::new(&format!("f{i}.rs")));
        }
        let got = load_recent_from(&file, &project);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got.len(), RECENT_PER_PROJECT_CAP);
        // The cap drops the oldest, so the earliest-recorded files must be gone
        // and the most recently recorded one must have survived.
        assert!(!got.contains(&PathBuf::from("f0.rs")));
        assert!(got.contains(&PathBuf::from(format!(
            "f{}.rs",
            RECENT_PER_PROJECT_CAP + 9
        ))));
    }

    #[test]
    fn corrupt_lines_are_skipped_without_error() {
        let dir = test_dir("corrupt");
        let file = dir.join("recent");
        let project = dir.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let proj_s = project.to_string_lossy();
        let content = format!(
            "# a comment line\n\
             garbage text with no tabs\n\
             1\t2\t3\t4\n\
             not-a-number\t{proj_s}\tbad.rs\n\
             \n\
             500\t{proj_s}\tgood.rs\n"
        );
        std::fs::write(&file, content).unwrap();

        let got = load_recent_from(&file, &project);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got, vec![PathBuf::from("good.rs")]);
    }

    #[test]
    fn missing_file_yields_empty_vec() {
        let dir = test_dir("missing");
        let file = dir.join("does-not-exist");
        let project = dir.join("proj");

        let got = load_recent_from(&file, &project);

        assert!(got.is_empty());
    }
}
