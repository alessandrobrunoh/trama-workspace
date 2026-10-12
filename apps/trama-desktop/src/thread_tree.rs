//! The sidebar's thread tree: children under their parent at any depth,
//! indentation capped at [`MAX_INDENT`] levels (deeper rows show their
//! depth instead), collapse and expand per node, and "Show N more" after
//! [`CHILD_LIMIT`] children, like Delta.

use std::collections::HashSet;

use trama_domain::ids::ThreadId;
use trama_protocol::ThreadSnapshot;

use crate::subthreads::{ThreadRow, VerdictBadge};

/// Indentation stops growing past this depth.
pub const MAX_INDENT: usize = 6;
/// Children shown before a "Show N more" row.
pub const CHILD_LIMIT: usize = 6;

/// One sidebar row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Thread {
        row: ThreadRow,
        /// Direct children (shown or not).
        children: usize,
        collapsed: bool,
    },
    /// The rest of `parent`'s children, folded.
    ShowMore {
        parent: ThreadId,
        depth: usize,
        hidden: usize,
    },
}

/// Collapsed nodes and nodes whose children are all shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeState {
    pub collapsed: HashSet<ThreadId>,
    pub expanded: HashSet<ThreadId>,
}

impl TreeState {
    pub fn toggle_collapsed(&mut self, id: &ThreadId) {
        if !self.collapsed.remove(id) {
            self.collapsed.insert(id.clone());
        }
    }

    pub fn show_all(&mut self, parent: &ThreadId) {
        self.expanded.insert(parent.clone());
    }

    /// Make `id` visible: expand its ancestors and show all children of
    /// any ancestor whose list would fold it away.
    pub fn reveal(&mut self, threads: &[ThreadSnapshot], id: &ThreadId) {
        let mut seen = HashSet::new();
        let mut current = id.clone();
        while seen.insert(current.clone()) {
            let Some(parent) = threads
                .iter()
                .find(|thread| thread.id == current)
                .and_then(|thread| thread.parent_thread_id.clone())
            else {
                break;
            };
            if !threads.iter().any(|thread| thread.id == parent) {
                break;
            }
            self.collapsed.remove(&parent);
            let position = threads
                .iter()
                .filter(|thread| thread.parent_thread_id.as_ref() == Some(&parent))
                .position(|thread| thread.id == current);
            if position.is_some_and(|position| position >= CHILD_LIMIT) {
                self.expanded.insert(parent.clone());
            }
            current = parent;
        }
    }
}

/// Visual indentation level for a depth.
pub fn indent_level(depth: usize) -> usize {
    depth.min(MAX_INDENT)
}

/// The depth label shown on rows deeper than the indentation goes.
pub fn depth_marker(depth: usize) -> Option<String> {
    (depth > MAX_INDENT).then(|| depth.to_string())
}

/// Rows in display order. A thread whose parent is not listed (or that
/// sits in a cycle) is a root; children keep the list's order.
pub fn tree_rows(threads: &[ThreadSnapshot], state: &TreeState) -> Vec<TreeRow> {
    let known = |id: &ThreadId| threads.iter().any(|thread| &thread.id == id);
    let mut out = Vec::with_capacity(threads.len());
    let mut placed = vec![false; threads.len()];
    for (index, thread) in threads.iter().enumerate() {
        let root = match &thread.parent_thread_id {
            None => true,
            Some(parent) => !known(parent),
        };
        if root {
            visit(threads, state, index, 0, &mut placed, &mut out);
        }
    }
    // Anything left is part of a cycle: show it flat rather than hide it.
    for index in 0..threads.len() {
        if !placed[index] {
            visit(threads, state, index, 0, &mut placed, &mut out);
        }
    }
    out
}

fn visit(
    threads: &[ThreadSnapshot],
    state: &TreeState,
    index: usize,
    depth: usize,
    placed: &mut [bool],
    out: &mut Vec<TreeRow>,
) {
    if placed[index] {
        return;
    }
    placed[index] = true;
    let thread = &threads[index];
    let children: Vec<usize> = threads
        .iter()
        .enumerate()
        .filter(|(child, candidate)| {
            !placed[*child] && candidate.parent_thread_id.as_ref() == Some(&thread.id)
        })
        .map(|(child, _)| child)
        .collect();
    let collapsed = !children.is_empty() && state.collapsed.contains(&thread.id);
    out.push(TreeRow::Thread {
        row: ThreadRow {
            index,
            depth,
            kind: thread.kind,
            verdict: VerdictBadge::from_verdict(thread.review_verdict.as_ref()),
        },
        children: children.len(),
        collapsed,
    });
    if collapsed {
        // Hidden, not orphaned: keep them out of the cycle fallback.
        for &child in &children {
            mark_subtree(threads, child, placed);
        }
        return;
    }
    let shown = if state.expanded.contains(&thread.id) {
        children.len()
    } else {
        children.len().min(CHILD_LIMIT)
    };
    for &child in &children[..shown] {
        visit(threads, state, child, depth + 1, placed, out);
    }
    if shown < children.len() {
        // Folded children stay unplaced here but must not resurface as
        // cycle roots.
        for &child in &children[shown..] {
            mark_subtree(threads, child, placed);
        }
        out.push(TreeRow::ShowMore {
            parent: thread.id.clone(),
            depth: depth + 1,
            hidden: children.len() - shown,
        });
    }
}

fn mark_subtree(threads: &[ThreadSnapshot], index: usize, placed: &mut [bool]) {
    if placed[index] {
        return;
    }
    placed[index] = true;
    let id = threads[index].id.clone();
    for child in 0..threads.len() {
        if threads[child].parent_thread_id.as_ref() == Some(&id) {
            mark_subtree(threads, child, placed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_protocol::ThreadKind;

    fn snapshot(id: &str, parent: Option<&str>) -> ThreadSnapshot {
        ThreadSnapshot {
            id: ThreadId::from(id),
            title: id.into(),
            repo_count: 1,
            parent_thread_id: parent.map(ThreadId::from),
            kind: if parent.is_some() {
                ThreadKind::Sub
            } else {
                ThreadKind::Top
            },
            review_verdict: None,
            model: None,
            depth: 0,
            subagent: None,
            archived: false,
        }
    }

    fn render(rows: &[TreeRow], threads: &[ThreadSnapshot]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                TreeRow::Thread {
                    row,
                    children,
                    collapsed,
                } => format!(
                    "{}{}{}",
                    "  ".repeat(row.depth),
                    threads[row.index].id,
                    match (children, collapsed) {
                        (0, _) => String::new(),
                        (n, true) => format!(" [+{n}]"),
                        (n, false) => format!(" [-{n}]"),
                    }
                ),
                TreeRow::ShowMore { depth, hidden, .. } => {
                    format!("{}… {hidden} more", "  ".repeat(*depth))
                }
            })
            .collect()
    }

    /// A chain root → d1 → … → d9.
    fn chain() -> Vec<ThreadSnapshot> {
        let mut threads = vec![snapshot("root", None)];
        for depth in 1..=9 {
            let parent = if depth == 1 {
                "root".to_string()
            } else {
                format!("d{}", depth - 1)
            };
            threads.push(snapshot(&format!("d{depth}"), Some(&parent)));
        }
        threads
    }

    #[test]
    fn deep_chains_nest_with_capped_indentation() {
        let threads = chain();
        let rows = tree_rows(&threads, &TreeState::default());
        assert_eq!(rows.len(), 10);
        let depths: Vec<usize> = rows
            .iter()
            .map(|row| match row {
                TreeRow::Thread { row, .. } => row.depth,
                TreeRow::ShowMore { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(depths, (0..10).collect::<Vec<_>>());
        assert_eq!(indent_level(3), 3);
        assert_eq!(indent_level(9), MAX_INDENT);
        assert_eq!(depth_marker(6), None);
        assert_eq!(depth_marker(9).as_deref(), Some("9"));
    }

    #[test]
    fn collapsing_hides_the_subtree_and_reveal_opens_it() {
        let threads = chain();
        let mut state = TreeState::default();
        state.toggle_collapsed(&ThreadId::from("d2"));
        let rows = tree_rows(&threads, &state);
        assert_eq!(
            render(&rows, &threads),
            vec!["root [-1]", "  d1 [-1]", "    d2 [+1]"]
        );
        state.reveal(&threads, &ThreadId::from("d5"));
        assert_eq!(tree_rows(&threads, &state).len(), 10);
        // A leaf has nothing to collapse.
        state.toggle_collapsed(&ThreadId::from("d9"));
        assert_eq!(tree_rows(&threads, &state).len(), 10);
    }

    #[test]
    fn many_children_fold_after_the_limit() {
        let mut threads = vec![snapshot("p", None)];
        for n in 0..9 {
            threads.push(snapshot(&format!("c{n}"), Some("p")));
        }
        threads.push(snapshot("g", Some("c8")));
        threads.push(snapshot("other", None));
        let mut state = TreeState::default();
        let rows = tree_rows(&threads, &state);
        assert_eq!(
            render(&rows, &threads),
            vec![
                "p [-9]",
                "  c0",
                "  c1",
                "  c2",
                "  c3",
                "  c4",
                "  c5",
                "  … 3 more",
                "other"
            ]
        );
        state.show_all(&ThreadId::from("p"));
        let rows = tree_rows(&threads, &state);
        assert_eq!(rows.len(), 12);
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, TreeRow::ShowMore { .. }))
        );

        // Selecting a folded grandchild unfolds the list that hid it.
        let mut state = TreeState::default();
        state.reveal(&threads, &ThreadId::from("g"));
        assert!(state.expanded.contains(&ThreadId::from("p")));
        assert!(render(&tree_rows(&threads, &state), &threads).contains(&"    g".to_string()));
    }

    #[test]
    fn cycles_and_orphans_stay_visible() {
        let threads = vec![
            snapshot("a", Some("b")),
            snapshot("b", Some("a")),
            snapshot("orphan", Some("missing")),
        ];
        let rows = tree_rows(&threads, &TreeState::default());
        assert_eq!(rows.len(), 3);
    }
}
