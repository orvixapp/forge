//! Git awareness for the editor (ARCHITECTURE.md §16, the Phase 3 slice
//! of it): which repository and branch a file belongs to, the file's
//! content at `HEAD`, and a line diff against the buffer for the gutter.
//! Staging, blame and history arrive with Phase 6.

use imara_diff::{Algorithm, Diff, InternedInput, sources::lines};
use std::path::{Path, PathBuf};

/// Where a file lives in git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Working tree root.
    pub root: PathBuf,
    /// Short branch name, or the abbreviated commit id when detached.
    pub branch: String,
    /// The file's path relative to `root`.
    pub relative: PathBuf,
}

/// Finds the repository containing `file`.
#[must_use]
pub fn discover(file: &Path) -> Option<RepoInfo> {
    let directory = file.parent()?;
    let repo = gix::discover(directory).ok()?;
    let root = repo.workdir()?.to_path_buf();
    let branch = match repo.head_name().ok().flatten() {
        Some(name) => name.shorten().to_string(),
        None => repo
            .head_id()
            .ok()
            .map_or_else(|| "HEAD".into(), |id| id.to_hex_with_len(8).to_string()),
    };
    let relative = file.strip_prefix(&root).ok()?.to_path_buf();
    Some(RepoInfo {
        root,
        branch,
        relative,
    })
}

/// The file's content in the `HEAD` commit: `Ok(None)` when it is not
/// tracked there (a new file).
///
/// # Errors
///
/// Repository or object store failures, as text.
pub fn head_text(info: &RepoInfo) -> Result<Option<String>, String> {
    let repo = gix::open(&info.root).map_err(|error| error.to_string())?;
    // An unborn branch has no HEAD commit: every file is new.
    let Ok(commit) = repo.head_commit() else {
        return Ok(None);
    };
    let tree = commit.tree().map_err(|error| error.to_string())?;
    let Some(entry) = tree
        .lookup_entry_by_path(&info.relative)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let object = entry.object().map_err(|error| error.to_string())?;
    Ok(Some(String::from_utf8_lossy(&object.data).into_owned()))
}

/// Kind of change a gutter line carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterMark {
    Added,
    Modified,
    /// Lines were removed right before this line.
    Deleted,
}

/// Per-line marks of `new` against `old`, as `(line, mark)` sorted by
/// line, plus the counts VS Code shows in the status bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineDiff {
    pub marks: Vec<(usize, GutterMark)>,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

impl LineDiff {
    /// Mark of buffer line `line`, if any.
    #[must_use]
    pub fn mark(&self, line: usize) -> Option<GutterMark> {
        self.marks
            .binary_search_by_key(&line, |(at, _)| *at)
            .ok()
            .map(|index| self.marks[index].1)
    }

    /// Lines of the hunk containing `line` — for "next/previous change".
    #[must_use]
    pub fn hunk_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut previous: Option<usize> = None;
        for (line, _) in &self.marks {
            if previous.is_none_or(|last| *line != last + 1) {
                starts.push(*line);
            }
            previous = Some(*line);
        }
        starts
    }
}

/// Line diff of `new` (the buffer) against `old` (`HEAD`); a file absent
/// from `HEAD` is all additions.
#[must_use]
pub fn line_diff(old: Option<&str>, new: &str) -> LineDiff {
    let new_lines = new.lines().count();
    let Some(old) = old else {
        let marks = (0..new_lines)
            .map(|line| (line, GutterMark::Added))
            .collect();
        return LineDiff {
            marks,
            added: new_lines,
            modified: 0,
            deleted: 0,
        };
    };
    let input = InternedInput::new(lines(old), lines(new));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut result = LineDiff::default();
    for hunk in diff.hunks() {
        let after = hunk.after.start as usize..hunk.after.end as usize;
        let before_len = (hunk.before.end - hunk.before.start) as usize;
        if hunk.is_pure_removal() {
            result.deleted += before_len;
            let at = after.start.min(new_lines.saturating_sub(1));
            if !result.marks.iter().any(|(line, _)| *line == at) {
                result.marks.push((at, GutterMark::Deleted));
            }
            continue;
        }
        let modified = after.len().min(before_len);
        result.modified += modified;
        result.added += after.len() - modified;
        if before_len > after.len() {
            result.deleted += before_len - after.len();
        }
        for (offset, line) in after.enumerate() {
            let mark = if offset < modified {
                GutterMark::Modified
            } else {
                GutterMark::Added
            };
            result.marks.push((line, mark));
        }
    }
    result.marks.sort_by_key(|(line, _)| *line);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_diff_classifies_added_modified_and_deleted() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let diff = line_diff(Some(old), new);
        assert_eq!(diff.mark(1), Some(GutterMark::Modified));
        assert_eq!(diff.mark(4), Some(GutterMark::Added));
        assert_eq!(diff.mark(0), None);
        assert_eq!((diff.added, diff.modified, diff.deleted), (1, 1, 0));
        let removed = line_diff(Some("a\nb\nc\n"), "a\nc\n");
        assert_eq!(removed.mark(1), Some(GutterMark::Deleted));
        assert_eq!(removed.deleted, 1);
        assert_eq!(removed.hunk_starts(), [1]);
        let fresh = line_diff(None, "x\ny\n");
        assert_eq!(fresh.added, 2);
        assert_eq!(fresh.hunk_starts(), [0]);
        assert_eq!(line_diff(Some("same\n"), "same\n"), LineDiff::default());
    }

    #[test]
    fn discovers_this_repository_and_reads_head_blobs() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let Some(info) = discover(&file) else {
            eprintln!("not inside a git checkout; skipping");
            return;
        };
        assert!(info.root.join(".git").exists());
        assert_eq!(info.relative, Path::new("crates/forge-git/Cargo.toml"));
        assert!(!info.branch.is_empty());
        // A committed file has HEAD content; a made-up one has none.
        let readme = RepoInfo {
            relative: PathBuf::from("README.md"),
            ..info.clone()
        };
        assert!(
            head_text(&readme)
                .unwrap()
                .is_some_and(|text| text.contains("Forge"))
        );
        let missing = RepoInfo {
            relative: PathBuf::from("no/such/file.rs"),
            ..info
        };
        assert_eq!(head_text(&missing).unwrap(), None);
    }
}
