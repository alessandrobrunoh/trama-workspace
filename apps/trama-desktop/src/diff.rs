//! Small parser for `git diff` unified output.
//!
//! It understands the git extended headers the Changes drawer needs (new,
//! deleted, renamed, binary) and plain `---`/`+++` diffs without a
//! `diff --git` line. It never runs git: callers hand it text.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl FileStatus {
    /// One-letter badge, as in `git status --short`.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`
    NoNewline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Text after the closing `@@`, usually the enclosing function.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    /// Set for renames (and copies): the path before the change.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
}

impl DiffFile {
    fn new(path: String) -> Self {
        Self {
            path,
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
        }
    }

    /// Stable hash of this file's change, so UI state such as "seen" can be
    /// dropped when the same path changes again.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.path.hash(&mut hasher);
        self.old_path.hash(&mut hasher);
        self.binary.hash(&mut hasher);
        for hunk in &self.hunks {
            (hunk.old_start, hunk.new_start).hash(&mut hasher);
            for line in &hunk.lines {
                (line.kind as u8).hash(&mut hasher);
                line.text.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

/// Totals across files: (files, additions, deletions).
pub fn totals(files: &[DiffFile]) -> (usize, u32, u32) {
    files.iter().fold((0, 0, 0), |(n, add, del), file| {
        (n + 1, add + file.additions, del + file.deletions)
    })
}

/// Parse unified diff text into files. Unknown lines are ignored.
pub fn parse_unified_diff(text: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current: Option<DiffFile> = None;
    // Line counters inside the active hunk.
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut in_hunk = false;
    // Whether the current file started with `diff --git` (so `---` lines
    // belong to it rather than starting a new file).
    let mut git_header = false;

    let mut lines = text.lines().peekable();
    while let Some(raw) = lines.next() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            let (a, b) = split_git_paths(rest);
            let mut file = DiffFile::new(b.clone());
            if a != b {
                file.old_path = Some(a);
            }
            current = Some(file);
            in_hunk = false;
            git_header = true;
            continue;
        }

        if in_hunk && let Some(file) = current.as_mut() {
            let hunk_open = file
                .hunks
                .last()
                .map(|hunk| hunk_has_room(hunk, old_no, new_no))
                .unwrap_or(false);
            if hunk_open || line.starts_with('\\') {
                if let Some(parsed) = parse_hunk_line(line, &mut old_no, &mut new_no) {
                    match parsed.kind {
                        LineKind::Added => file.additions += 1,
                        LineKind::Removed => file.deletions += 1,
                        _ => {}
                    }
                    if let Some(hunk) = file.hunks.last_mut() {
                        hunk.lines.push(parsed);
                    }
                    continue;
                }
            }
            in_hunk = false;
        }

        if let Some(header) = line.strip_prefix("@@ ") {
            if let (Some(file), Some(hunk)) = (current.as_mut(), parse_hunk_header(header)) {
                old_no = hunk.old_start;
                new_no = hunk.new_start;
                file.hunks.push(hunk);
                in_hunk = true;
            }
            continue;
        }

        if line.starts_with("--- ") && lines.peek().is_some_and(|next| next.starts_with("+++ ")) {
            let old = clean_path(&line[4..]);
            let new_line = lines.next().unwrap_or_default();
            let new = clean_path(&new_line[4..]);
            if !git_header || current.is_none() {
                if let Some(file) = current.take() {
                    files.push(file);
                }
                let path = new.clone().or_else(|| old.clone()).unwrap_or_default();
                current = Some(DiffFile::new(path));
            }
            git_header = false;
            if let Some(file) = current.as_mut() {
                match (&old, &new) {
                    (None, Some(new)) => {
                        file.status = FileStatus::Added;
                        file.path = new.clone();
                    }
                    (Some(old), None) => {
                        file.status = FileStatus::Deleted;
                        file.path = old.clone();
                    }
                    (Some(old), Some(new)) => {
                        file.path = new.clone();
                        if old != new {
                            file.old_path = Some(old.clone());
                            file.status = FileStatus::Renamed;
                        }
                    }
                    (None, None) => {}
                }
            }
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
            file.old_path = None;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
            file.old_path = None;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.old_path = Some(from.to_string());
            file.status = FileStatus::Renamed;
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_string();
            file.status = FileStatus::Renamed;
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    if let Some(file) = current.take() {
        files.push(file);
    }
    for file in &mut files {
        if file.status == FileStatus::Modified && file.old_path.is_some() {
            file.status = FileStatus::Renamed;
        }
    }
    files
}

fn hunk_has_room(hunk: &Hunk, old_no: u32, new_no: u32) -> bool {
    old_no < hunk.old_start + hunk.old_len || new_no < hunk.new_start + hunk.new_len
}

fn parse_hunk_line(line: &str, old_no: &mut u32, new_no: &mut u32) -> Option<DiffLine> {
    let (kind, text) = match line.chars().next() {
        Some('+') => (LineKind::Added, &line[1..]),
        Some('-') => (LineKind::Removed, &line[1..]),
        Some(' ') => (LineKind::Context, &line[1..]),
        // Some tools strip the space on empty context lines.
        None => (LineKind::Context, ""),
        Some('\\') => (LineKind::NoNewline, line.trim_start_matches('\\').trim()),
        Some(_) => return None,
    };
    let (old_line, new_line) = match kind {
        LineKind::Context => {
            let numbers = (Some(*old_no), Some(*new_no));
            *old_no += 1;
            *new_no += 1;
            numbers
        }
        LineKind::Added => {
            let number = Some(*new_no);
            *new_no += 1;
            (None, number)
        }
        LineKind::Removed => {
            let number = Some(*old_no);
            *old_no += 1;
            (number, None)
        }
        LineKind::NoNewline => (None, None),
    };
    Some(DiffLine {
        kind,
        old_line,
        new_line,
        text: text.to_string(),
    })
}

/// `-12,7 +12,9 @@ fn main()` → hunk with an empty line list.
fn parse_hunk_header(header: &str) -> Option<Hunk> {
    let end = header.find(" @@")?;
    let ranges = &header[..end];
    let section = header[end + 3..].trim().to_string();
    let mut parts = ranges.split_whitespace();
    let (old_start, old_len) = parse_range(parts.next()?.strip_prefix('-')?)?;
    let (new_start, new_len) = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section,
        lines: Vec::new(),
    })
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// `a/src/x.rs b/src/y.rs` → (`src/x.rs`, `src/y.rs`). Handles quoted paths
/// and paths with spaces when both sides are equal.
fn split_git_paths(rest: &str) -> (String, String) {
    let rest = rest.trim();
    if rest.starts_with('"') {
        let mut parts = rest.split('"').filter(|part| !part.trim().is_empty());
        let a = parts.next().unwrap_or_default();
        let b = parts.next().unwrap_or(a);
        return (strip_side(a), strip_side(b));
    }
    // With identical sides the split point is the middle " b/".
    if let Some(index) = rest.find(" b/") {
        let a = &rest[..index];
        let b = &rest[index + 1..];
        return (strip_side(a), strip_side(b));
    }
    let mut parts = rest.splitn(2, ' ');
    let a = parts.next().unwrap_or_default();
    let b = parts.next().unwrap_or(a);
    (strip_side(a), strip_side(b))
}

fn strip_side(path: &str) -> String {
    let path = path.trim().trim_matches('"');
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// `a/src/x.rs\t2024-…` → `Some("src/x.rs")`; `/dev/null` → `None`.
fn clean_path(raw: &str) -> Option<String> {
    let path = raw.split('\t').next().unwrap_or(raw).trim();
    if path == "/dev/null" {
        return None;
    }
    Some(strip_side(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFIED: &str = "\
diff --git a/src/auth.rs b/src/auth.rs
index 3b18e51..a9c2f07 100644
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,7 +10,8 @@ fn login(user: &User) -> Session {
     let token = issue();
-    todo!()
+    let session = Session::new(token);
+    session
 }

 fn logout() {}
@@ -40,3 +41,3 @@ impl Session {
-    old
+    new
     tail
";

    #[test]
    fn parses_modified_file_with_two_hunks_and_line_numbers() {
        let files = parse_unified_diff(MODIFIED);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "src/auth.rs");
        assert_eq!(file.old_path, None);
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!((file.additions, file.deletions), (3, 2));
        assert_eq!(file.hunks.len(), 2);

        let hunk = &file.hunks[0];
        assert_eq!(
            (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
            (10, 7, 10, 8)
        );
        assert_eq!(hunk.section, "fn login(user: &User) -> Session {");
        let kinds: Vec<_> = hunk.lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context,
                LineKind::Context,
                LineKind::Context,
            ]
        );
        assert_eq!(hunk.lines[0].old_line, Some(10));
        assert_eq!(hunk.lines[0].new_line, Some(10));
        assert_eq!(hunk.lines[1].old_line, Some(11));
        assert_eq!(hunk.lines[1].new_line, None);
        assert_eq!(hunk.lines[2].new_line, Some(11));
        assert_eq!(hunk.lines[3].new_line, Some(12));
        assert_eq!(hunk.lines[4].old_line, Some(12));
        assert_eq!(hunk.lines[4].new_line, Some(13));
        // The empty context line kept its numbers.
        assert_eq!(hunk.lines[5].text, "");
        assert_eq!(hunk.lines[5].old_line, Some(13));

        let second = &file.hunks[1];
        assert_eq!(second.lines[0].old_line, Some(40));
        assert_eq!(second.lines[1].new_line, Some(41));
        assert_eq!(second.lines[2].new_line, Some(42));
    }

    #[test]
    fn parses_added_deleted_renamed_and_binary_files() {
        let text = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..e69de29
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index e69de29..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
\\ No newline at end of file
diff --git a/old name.rs b/new name.rs
similarity index 90%
rename from old name.rs
rename to new name.rs
index 1..2 100644
--- a/old name.rs
+++ b/new name.rs
@@ -1 +1 @@
-a
+b
diff --git a/logo.png b/logo.png
index 1..2 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse_unified_diff(text);
        assert_eq!(files.len(), 4);

        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!((files[0].additions, files[0].deletions), (2, 0));

        assert_eq!(files[1].path, "gone.txt");
        assert_eq!(files[1].status, FileStatus::Deleted);
        assert_eq!(files[1].deletions, 1);
        let last = files[1].hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::NoNewline);
        assert_eq!(last.old_line, None);

        assert_eq!(files[2].path, "new name.rs");
        assert_eq!(files[2].old_path.as_deref(), Some("old name.rs"));
        assert_eq!(files[2].status, FileStatus::Renamed);
        assert_eq!((files[2].additions, files[2].deletions), (1, 1));

        assert_eq!(files[3].path, "logo.png");
        assert!(files[3].binary);
        assert!(files[3].hunks.is_empty());
        assert_eq!(totals(&files), (4, 3, 2));
    }

    #[test]
    fn parses_plain_unified_diff_without_git_header() {
        let text = "\
--- a/readme.md\t2026-01-01
+++ b/readme.md\t2026-01-02
@@ -1,2 +1,2 @@
 # Title
-old line
+new line
--- a/other.md
+++ b/other.md
@@ -3 +3,2 @@
 keep
+added
";
        let files = parse_unified_diff(text);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "readme.md");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!(files[1].path, "other.md");
        assert_eq!(files[1].hunks[0].old_len, 1);
        assert_eq!(files[1].hunks[0].lines[1].new_line, Some(4));
    }

    #[test]
    fn hunk_body_lines_that_look_like_headers_stay_in_the_hunk() {
        // A removed line whose text is "-- x" renders as "--- x" in the diff.
        let text = "\
diff --git a/sql.txt b/sql.txt
--- a/sql.txt
+++ b/sql.txt
@@ -1,2 +1,1 @@
--- x
 keep
";
        let files = parse_unified_diff(text);
        assert_eq!(files.len(), 1);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, LineKind::Removed);
        assert_eq!(lines[0].text, "-- x");
        assert_eq!(files[0].deletions, 1);
    }

    #[test]
    fn fingerprint_changes_with_content_and_empty_input_is_empty() {
        assert!(parse_unified_diff("").is_empty());
        assert!(parse_unified_diff("not a diff\n").is_empty());
        let a = parse_unified_diff(MODIFIED);
        let b = parse_unified_diff(&MODIFIED.replace("+    new", "+    newer"));
        assert_eq!(
            a[0].fingerprint(),
            parse_unified_diff(MODIFIED)[0].fingerprint()
        );
        assert_ne!(a[0].fingerprint(), b[0].fingerprint());
    }
}
