use super::*;

// -- edits → unified diff -------------------------------------------------

/// One edit call's change to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    /// "Edit", "Write" or "Edit notebook".
    pub verb: &'static str,
    pub path: String,
    /// Replacements in this call (MultiEdit carries several).
    pub edits: usize,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileEdit {
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }
}

pub(super) const CONTEXT: usize = 3;

/// The diff an edit call makes, from its input. `output` (the tool result)
/// places a single Edit at its real line when it quotes the edited lines.
pub fn file_edit(name: &str, input: &Value, output: Option<&Value>) -> Option<FileEdit> {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let (verb, path, pairs): (&'static str, String, Vec<(String, String)>) = match name {
        "Edit" => (
            "Edit",
            field("file_path")?.to_string(),
            vec![(
                field("old_string").unwrap_or_default().to_string(),
                field("new_string").unwrap_or_default().to_string(),
            )],
        ),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array);
            let pairs = edits
                .map(|edits| {
                    edits
                        .iter()
                        .map(|edit| {
                            let text = |key: &str| {
                                edit.get(key)
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            };
                            (text("old_string"), text("new_string"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ("Edit", field("file_path")?.to_string(), pairs)
        }
        "Write" => (
            "Write",
            field("file_path")?.to_string(),
            vec![(
                String::new(),
                field("content").unwrap_or_default().to_string(),
            )],
        ),
        "NotebookEdit" => {
            let source = field("new_source").unwrap_or_default().to_string();
            let pair = if field("edit_mode") == Some("delete") {
                (source, String::new())
            } else {
                (String::new(), source)
            };
            (
                "Edit notebook",
                field("notebook_path")?.to_string(),
                vec![pair],
            )
        }
        _ => return None,
    };
    let snippet = output.map(output_text).unwrap_or_default();
    let single = pairs.len() == 1;
    let mut hunks = Vec::new();
    for (old, new) in &pairs {
        let start = if name == "Write" {
            1
        } else if single {
            locate_in_snippet(&snippet, new).unwrap_or(1)
        } else {
            1
        };
        let lines = line_diff(old, new, start, start);
        hunks.extend(build_hunks(&lines, CONTEXT));
    }
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .fold((0, 0), |(add, del), line| match line.kind {
                LineKind::Added => (add + 1, del),
                LineKind::Removed => (add, del + 1),
                _ => (add, del),
            });
    Some(FileEdit {
        verb,
        path,
        edits: pairs.len(),
        hunks,
        additions,
        deletions,
    })
}

/// The line where `new` starts, read from a `cat -n` style snippet in the
/// tool result ("   12→text" or "   12\ttext").
pub(super) fn locate_in_snippet(snippet: &str, new: &str) -> Option<u32> {
    let first = new.lines().find(|line| !line.trim().is_empty())?;
    let offset = new.lines().position(|line| line == first)? as u32;
    snippet.lines().find_map(|line| {
        let trimmed = line.trim_start();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let rest = &trimmed[digits..];
        let text = rest.strip_prefix('→').or_else(|| rest.strip_prefix('\t'))?;
        (text.trim_end() == first.trim_end())
            .then(|| trimmed[..digits].parse::<u32>().ok())
            .flatten()
            .map(|number| number.saturating_sub(offset).max(1))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Equal,
    Delete,
    Insert,
}

/// Line operations turning `old` into `new` (longest common subsequence
/// after trimming the shared head and tail).
pub(super) fn diff_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(Op, &'a str)> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[head..old.len() - tail];
    let b = &new[head..new.len() - tail];
    let mut ops: Vec<(Op, &str)> = old[..head].iter().map(|line| (Op::Equal, *line)).collect();
    if a.len().saturating_mul(b.len()) > 1_000_000 {
        ops.extend(a.iter().map(|line| (Op::Delete, *line)));
        ops.extend(b.iter().map(|line| (Op::Insert, *line)));
    } else {
        // lcs[i][j]: common lines of a[i..] and b[j..].
        let width = b.len() + 1;
        let mut lcs = vec![0u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * width + j] = if a[i] == b[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                ops.push((Op::Equal, a[i]));
                i += 1;
                j += 1;
            } else if j < b.len()
                && (i == a.len() || lcs[i * width + j + 1] >= lcs[(i + 1) * width + j])
            {
                ops.push((Op::Insert, b[j]));
                j += 1;
            } else {
                ops.push((Op::Delete, a[i]));
                i += 1;
            }
        }
        // Show removals before additions within each change.
        let mut start = 0;
        while start < ops.len() {
            if ops[start].0 == Op::Equal {
                start += 1;
                continue;
            }
            let end = ops[start..]
                .iter()
                .position(|(op, _)| *op == Op::Equal)
                .map_or(ops.len(), |offset| start + offset);
            ops[start..end].sort_by_key(|(op, _)| *op == Op::Insert);
            start = end;
        }
    }
    ops.extend(
        old[old.len() - tail..]
            .iter()
            .map(|line| (Op::Equal, *line)),
    );
    ops
}

/// Numbered diff lines for `old` → `new`, both starting at the given line.
pub fn line_diff(old: &str, new: &str, old_start: u32, new_start: u32) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (mut old_no, mut new_no) = (old_start, new_start);
    diff_ops(&old_lines, &new_lines)
        .into_iter()
        .map(|(op, text)| {
            let (kind, old_line, new_line) = match op {
                Op::Equal => {
                    let numbers = (LineKind::Context, Some(old_no), Some(new_no));
                    old_no += 1;
                    new_no += 1;
                    numbers
                }
                Op::Delete => {
                    old_no += 1;
                    (LineKind::Removed, Some(old_no - 1), None)
                }
                Op::Insert => {
                    new_no += 1;
                    (LineKind::Added, None, Some(new_no - 1))
                }
            };
            DiffLine {
                kind,
                old_line,
                new_line,
                text: text.to_string(),
            }
        })
        .collect()
}

/// Group changed lines into hunks with `context` lines around them.
pub fn build_hunks(lines: &[DiffLine], context: usize) -> Vec<Hunk> {
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != LineKind::Context)
        .map(|(index, _)| index)
        .collect();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &index in &changed {
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(lines.len());
        match ranges.last_mut() {
            Some(last) if start <= last.end => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
        .into_iter()
        .map(|range| {
            let slice = &lines[range];
            let old_len = slice.iter().filter(|l| l.old_line.is_some()).count() as u32;
            let new_len = slice.iter().filter(|l| l.new_line.is_some()).count() as u32;
            let old_start = slice
                .iter()
                .find_map(|line| line.old_line)
                .unwrap_or_else(|| slice[0].new_line.unwrap_or(1).saturating_sub(1));
            let new_start = slice
                .iter()
                .find_map(|line| line.new_line)
                .unwrap_or_else(|| slice[0].old_line.unwrap_or(1).saturating_sub(1));
            Hunk {
                header: format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@"),
                old_start,
                old_len,
                new_start,
                new_len,
                section: String::new(),
                lines: slice.to_vec(),
            }
        })
        .collect()
}

/// `/Users/me/src/app/main.rs` → `~/src/app/main.rs`.
pub fn display_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}
