//! Right drawer and its Changes view.
//!
//! There is no daemon API for diffs yet. The panel starts empty and says so.
//! Integration calls [`ChangesPanel::set_diff`] with `git diff` text once the
//! daemon can provide it; nothing here invents data.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _, checkbox::Checkbox, h_flex, scroll::ScrollableElement as _, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::diff::{DiffFile, DiffLine, FileStatus, LineKind, parse_unified_diff, totals};
use crate::theme::{self, Palette};

struct ChangeSet {
    repo_label: String,
    files: Vec<DiffFile>,
}

pub struct ChangesPanel {
    change_set: Option<ChangeSet>,
    collapsed: HashSet<String>,
    /// Path → fingerprint of the change the user marked as seen.
    seen: HashMap<String, u64>,
}

impl ChangesPanel {
    pub fn new() -> Self {
        Self {
            change_set: None,
            collapsed: HashSet::new(),
            seen: HashMap::new(),
        }
    }

    /// Show `unified_diff` (output of `git diff`) for one repository.
    ///
    /// "Seen" marks survive a refresh only for files whose change is
    /// byte-identical; a file that changed again comes back unseen.
    pub fn set_diff(
        &mut self,
        repo_label: impl Into<String>,
        unified_diff: &str,
        cx: &mut Context<Self>,
    ) {
        let files = parse_unified_diff(unified_diff);
        retain_seen(&mut self.seen, &files);
        let paths: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        self.collapsed.retain(|path| paths.contains(path.as_str()));
        self.change_set = Some(ChangeSet {
            repo_label: repo_label.into(),
            files,
        });
        cx.notify();
    }

    /// Back to the "no data" state, e.g. when the selected thread changes.
    #[allow(dead_code)]
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.change_set = None;
        self.collapsed.clear();
        self.seen.clear();
        cx.notify();
    }

    fn is_seen(&self, file: &DiffFile) -> bool {
        self.seen.get(&file.path) == Some(&file.fingerprint())
    }

    fn set_seen(&mut self, index: usize, seen: bool, cx: &mut Context<Self>) {
        let Some(file) = self
            .change_set
            .as_ref()
            .and_then(|set| set.files.get(index))
        else {
            return;
        };
        let path = file.path.clone();
        if seen {
            self.seen.insert(path.clone(), file.fingerprint());
            self.collapsed.insert(path);
        } else {
            self.seen.remove(&path);
            self.collapsed.remove(&path);
        }
        cx.notify();
    }

    fn toggle_collapsed(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        cx.notify();
    }
}

impl Default for ChangesPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Keep only seen marks whose path still has the identical change.
fn retain_seen(seen: &mut HashMap<String, u64>, files: &[DiffFile]) {
    let current: HashMap<&str, u64> = files
        .iter()
        .map(|file| (file.path.as_str(), file.fingerprint()))
        .collect();
    seen.retain(|path, fingerprint| current.get(path.as_str()) == Some(fingerprint));
}

impl Render for ChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let Some(set) = &self.change_set else {
            return empty_state(
                &p,
                "No changes to show",
                "Changes will appear when the daemon provides them.",
            )
            .into_any_element();
        };
        if set.files.is_empty() {
            return empty_state(
                &p,
                "Working tree clean",
                format!("{} has no uncommitted changes.", set.repo_label),
            )
            .into_any_element();
        }

        let (count, additions, deletions) = totals(&set.files);
        let seen_count = set.files.iter().filter(|file| self.is_seen(file)).count();
        let files: Vec<AnyElement> = set
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let collapsed = self.collapsed.contains(&file.path);
                let seen = self.is_seen(file);
                render_file(index, file, collapsed, seen, &p, cx).into_any_element()
            })
            .collect();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(theme::sp(3.))
                    .py(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .border_b_1()
                    .border_color(p.border)
                    .text_size(theme::text_sm())
                    .child(
                        Icon::new(IconName::FolderGit2)
                            .with_size(px(13.))
                            .text_color(p.text_muted),
                    )
                    .child(
                        div()
                            .text_color(p.text)
                            .font_weight(FontWeight::MEDIUM)
                            .child(set.repo_label.clone()),
                    )
                    .child(
                        div()
                            .text_color(p.text_faint)
                            .child(format!("{count} file{}", if count == 1 { "" } else { "s" })),
                    )
                    .child(counts(additions, deletions, &p))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(p.text_faint)
                            .child(format!("{seen_count}/{count} seen")),
                    ),
            )
            .child(
                v_flex()
                    .id("changes-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .children(files),
            )
            .into_any_element()
    }
}

fn empty_state(p: &Palette, title: &str, body: impl Into<SharedString>) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(theme::sp(2.))
        .px(theme::sp(8.))
        .child(
            div()
                .size(px(36.))
                .rounded(theme::radius_lg())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::FileDiff)
                        .with_size(px(16.))
                        .text_color(p.text_muted),
                ),
        )
        .child(
            div()
                .text_size(theme::text_ui())
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(theme::text_sm())
                .text_color(p.text_muted)
                .text_center()
                .child(body.into()),
        )
}

fn counts(additions: u32, deletions: u32, p: &Palette) -> impl IntoElement {
    h_flex()
        .gap(theme::sp(1.5))
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .child(
            div()
                .text_color(p.diff_add_text)
                .child(format!("+{additions}")),
        )
        .child(
            div()
                .text_color(p.diff_del_text)
                .child(format!("\u{2212}{deletions}")),
        )
}

fn status_color(status: FileStatus, p: &Palette) -> Hsla {
    match status {
        FileStatus::Added => p.success,
        FileStatus::Deleted => p.danger,
        FileStatus::Modified => p.warning,
        FileStatus::Renamed => p.accent_text,
    }
}

fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn render_file(
    index: usize,
    file: &DiffFile,
    collapsed: bool,
    seen: bool,
    p: &Palette,
    cx: &mut Context<ChangesPanel>,
) -> impl IntoElement {
    let (dir, name) = split_path(&file.path);
    let path = file.path.clone();
    let header = h_flex()
        .id(("diff-file", index))
        .w_full()
        .gap(theme::sp(2.))
        .px(theme::sp(2.))
        .py(theme::sp(1.5))
        .items_center()
        .cursor_pointer()
        .hover(|style| style.bg(p.hover))
        .when(!collapsed, |this| this.border_b_1().border_color(p.border))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(path.clone(), cx)))
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .with_size(px(12.))
            .text_color(p.text_faint),
        )
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .font_weight(FontWeight::BOLD)
                .text_color(status_color(file.status, p))
                .child(file.status.letter()),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_size(theme::text_sm())
                .child(
                    div()
                        .flex_shrink(1.)
                        .min_w_0()
                        .truncate()
                        .text_color(p.text_faint)
                        .child(dir),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(if seen { p.text_muted } else { p.text })
                        .child(name),
                )
                .when_some(file.old_path.clone(), |this, old| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .pl(theme::sp(1.5))
                            .text_color(p.text_faint)
                            .child(format!("\u{2190} {old}")),
                    )
                }),
        )
        .child(counts(file.additions, file.deletions, p))
        .child(
            Checkbox::new(("seen", index))
                .label("Seen")
                .checked(seen)
                .small()
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_seen(index, *checked, cx);
                })),
        );

    let body = (!collapsed).then(|| {
        if file.binary {
            return div()
                .px(theme::sp(3.))
                .py(theme::sp(2.))
                .text_size(theme::text_sm())
                .text_color(p.text_muted)
                .child("Binary file not shown.")
                .into_any_element();
        }
        if file.hunks.is_empty() {
            return div()
                .px(theme::sp(3.))
                .py(theme::sp(2.))
                .text_size(theme::text_sm())
                .text_color(p.text_muted)
                .child("No content changes (mode or rename only).")
                .into_any_element();
        }
        v_flex()
            .w_full()
            .font_family(theme::MONO_FONT)
            .text_size(theme::text_xs())
            .children(file.hunks.iter().flat_map(|hunk| {
                let header = div()
                    .w_full()
                    .px(theme::sp(2.))
                    .py(theme::sp(0.5))
                    .bg(p.diff_hunk_bg)
                    .text_color(p.text_faint)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(format!(
                        "@@ -{},{} +{},{} @@ {}",
                        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len, hunk.section
                    ))
                    .into_any_element();
                std::iter::once(header).chain(
                    hunk.lines
                        .iter()
                        .map(|line| render_line(line, p).into_any_element()),
                )
            }))
            .into_any_element()
    });

    v_flex()
        .w_full()
        .flex_shrink_0()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.bg)
        .overflow_hidden()
        .child(header)
        .children(body)
}

fn render_line(line: &DiffLine, p: &Palette) -> impl IntoElement {
    let (bg, sign, text_color) = match line.kind {
        LineKind::Added => (Some(p.diff_add_bg), "+", p.text),
        LineKind::Removed => (Some(p.diff_del_bg), "-", p.text),
        LineKind::Context => (None, " ", p.text_muted),
        LineKind::NoNewline => (None, " ", p.text_faint),
    };
    let sign_color = match line.kind {
        LineKind::Added => p.diff_add_text,
        LineKind::Removed => p.diff_del_text,
        _ => p.text_faint,
    };
    let number = |n: Option<u32>| {
        div()
            .w(px(34.))
            .flex_shrink_0()
            .pr(theme::sp(1.5))
            .text_right()
            .text_color(p.text_faint)
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    h_flex()
        .w_full()
        .when_some(bg, |this, bg| this.bg(bg))
        .child(number(line.old_line))
        .child(number(line.new_line))
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .text_color(sign_color)
                .child(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_color(text_color)
                .child(line.text.replace('\t', "    ")),
        )
}

/// Tabs of the right drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawerTab {
    #[default]
    Changes,
    Copies,
}

impl DrawerTab {
    fn label(self) -> &'static str {
        match self {
            DrawerTab::Changes => "Changes",
            DrawerTab::Copies => "Copies",
        }
    }
}

/// Drawer chrome: tab strip and close button around the active view.
pub fn render_drawer(
    active: DrawerTab,
    body: AnyView,
    on_tab: impl Fn(DrawerTab, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    use gpui_kit::component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
    };
    let p = theme::palette(cx);
    let on_tab = std::rc::Rc::new(on_tab);
    let tab = |tab: DrawerTab| {
        let is_active = tab == active;
        let on_tab = on_tab.clone();
        div()
            .id(SharedString::from(format!("drawer-tab-{}", tab.label())))
            .px(theme::sp(2.5))
            .py(theme::sp(1.))
            .rounded(theme::radius())
            .text_size(theme::text_sm())
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(is_active, |this| this.bg(p.selected).text_color(p.text))
            .when(!is_active, |this| {
                this.text_color(p.text_faint)
                    .hover(|style| style.bg(p.hover))
            })
            .on_click(move |_, window, cx| on_tab(tab, window, cx))
            .child(tab.label())
    };
    v_flex()
        .w(theme::drawer_width())
        .h_full()
        .flex_shrink_0()
        .bg(p.panel)
        .border_l_1()
        .border_color(p.border)
        .child(
            h_flex()
                .flex_shrink_0()
                .h(px(40.))
                .px(theme::sp(2.))
                .gap(theme::sp(1.))
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .child(tab(DrawerTab::Changes))
                .child(tab(DrawerTab::Copies))
                .child(div().flex_1())
                .child(
                    Button::new("drawer-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close drawer (\u{2318}R)")
                        .disabled(false)
                        .on_click(on_close),
                ),
        )
        .child(div().flex_1().min_h_0().child(body))
}

#[cfg(test)]
mod tests {
    use super::{HashMap, parse_unified_diff, retain_seen, split_path};

    const DIFF: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-x
+y
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-x
+z
";

    #[test]
    fn seen_marks_survive_only_identical_changes() {
        let files = parse_unified_diff(DIFF);
        let mut seen: HashMap<String, u64> = files
            .iter()
            .map(|file| (file.path.clone(), file.fingerprint()))
            .collect();
        seen.insert("deleted.rs".into(), 1);

        let refreshed = parse_unified_diff(&DIFF.replace("+z", "+zz"));
        retain_seen(&mut seen, &refreshed);
        assert!(seen.contains_key("a.rs"), "unchanged file stays seen");
        assert!(!seen.contains_key("b.rs"), "changed file is unseen again");
        assert!(!seen.contains_key("deleted.rs"), "gone file is dropped");
    }

    #[test]
    fn split_path_separates_directory_and_name() {
        assert_eq!(
            split_path("src/ui/app.rs"),
            ("src/ui/".to_string(), "app.rs".to_string())
        );
        assert_eq!(split_path("README"), (String::new(), "README".to_string()));
    }
}
