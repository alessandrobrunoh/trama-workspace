use super::*;

/// Diff lines picked with the mouse in one hunk of an edit shown in the
/// transcript. Typing while lines are picked starts a comment on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPick {
    pub tool_use_id: String,
    pub hunk: usize,
    pub anchor: usize,
    pub cursor: usize,
}

impl PatchPick {
    pub fn lines(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.min(self.cursor)..=self.anchor.max(self.cursor)
    }

    pub fn contains(&self, tool_use_id: &str, hunk: usize, line: usize) -> bool {
        self.tool_use_id == tool_use_id && self.hunk == hunk && self.lines().contains(&line)
    }
}

/// One diff line as the comment quotes it: `+ text`, `- text`, `  text`.
pub fn quoted_line(line: &DiffLine) -> String {
    let sign = match line.kind {
        LineKind::Added => '+',
        LineKind::Removed => '-',
        _ => ' ',
    };
    format!("{sign} {}", line.text)
}

/// "Apply patch <path> ⌄" and, open, the edit's diff. Lines can be picked
/// (click, drag, shift-click) and commented on; `marked` holds the
/// (hunk, line) of lines with comments. `None` when the input is not an
/// edit this view can read.
pub fn render_patch(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    pick: Option<&PatchPick>,
    marked: &HashSet<(usize, usize)>,
    cx: &mut Context<ChatPanel>,
) -> Option<AnyElement> {
    let ChatEntry::ToolCall {
        tool_use_id,
        finished,
        is_error,
        output,
        ..
    } = entry
    else {
        return None;
    };
    let edit = ctx.state.file_edit(entry)?;
    let p = ctx.p;
    let lines = edit.line_count();
    let open = ctx
        .state
        .edit_open
        .get(tool_use_id)
        .copied()
        .unwrap_or(lines <= OPEN_EDIT_LINES && !is_error);
    let key = format!("patch-{tool_use_id}");
    let path = display_path(&edit.path);
    let text = ctx.text(&key, &path, comments::KIND_PATCH, cx);
    let mut trailing = vec![counts(edit.additions, edit.deletions, p).into_any_element()];
    if !*finished || *is_error {
        trailing.push(status_icon(*finished, *is_error, p));
    }
    let id = tool_use_id.clone();
    let header = line(
        ("patch", ix),
        Some("Apply patch".into()),
        text,
        key,
        trailing,
        (lines > 0).then_some(open),
        Some(Rc::new(move |this, _, _, cx| {
            this.toggle_edit(&id, open, cx)
        })),
        p,
        cx,
    );
    let error = is_error
        .then(|| output.as_ref().map(output_text))
        .flatten()
        .map(|text| comments::one_line(&strip_tags(&text), 160));
    let body = (open && lines > 0).then(|| {
        let mut column = v_flex()
            .flex_shrink_0()
            .min_w_full()
            .py(theme::sp(1.5))
            .font_family(theme::MONO_FONT)
            .text_size(theme::text_xs())
            .line_height(relative(1.6));
        let mut drawn = 0;
        for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
            if hunk_ix > 0 {
                column = column.child(div().w_full().h(theme::sp(3.)));
            }
            for (line_ix, diff_line) in hunk.lines.iter().enumerate() {
                if drawn == MAX_RENDERED_LINES {
                    break;
                }
                drawn += 1;
                let picked = pick.is_some_and(|pick| pick.contains(tool_use_id, hunk_ix, line_ix));
                let commented = marked.contains(&(hunk_ix, line_ix));
                let at = PatchLine {
                    tool_use_id: tool_use_id.clone(),
                    hunk: hunk_ix,
                    line: line_ix,
                };
                let drag_at = at.clone();
                column = column.child(
                    diff_row(diff_line, picked, commented, p)
                        .id(SharedString::from(format!(
                            "patch-line-{tool_use_id}-{hunk_ix}-{line_ix}"
                        )))
                        .cursor_text()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.press_patch_line(
                                    at.clone(),
                                    event.modifiers.shift,
                                    window,
                                    cx,
                                );
                            }),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.drag_patch_line(drag_at.clone(), cx);
                            }
                        })),
                );
            }
        }
        if lines > drawn {
            column = column.child(
                div()
                    .px(theme::sp(3.))
                    .py(theme::sp(1.))
                    .font_family(theme::UI_FONT)
                    .text_color(p.text_faint)
                    .child(format!("{} more lines not shown", lines - drawn)),
            );
        }
        h_flex()
            .id(("patch-body", ix))
            .w_full()
            .rounded(theme::radius())
            .border_1()
            .border_color(if *is_error {
                p.danger.opacity(0.4)
            } else {
                p.border
            })
            .bg(p.panel)
            .overflow_x_scroll()
            .child(column)
    });
    Some(
        v_flex()
            .w_full()
            .gap(theme::sp(1.))
            .child(header)
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .pl(theme::sp(3.))
                        .text_size(theme::text_xs())
                        .text_color(p.danger)
                        .child(error),
                )
            })
            .children(body)
            .into_any_element(),
    )
}

/// New-side line numbers an edit's diff shows.
pub fn new_lines(edit: &FileEdit) -> Vec<u32> {
    edit.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| line.new_line)
        .collect()
}

/// (hunk, line) of the diff lines showing new-side lines `range`.
pub fn patch_lines_in(
    edit: &FileEdit,
    range: std::ops::RangeInclusive<u32>,
) -> HashSet<(usize, usize)> {
    let mut out = HashSet::new();
    for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
        for (line_ix, line) in hunk.lines.iter().enumerate() {
            if line.new_line.is_some_and(|number| range.contains(&number)) {
                out.insert((hunk_ix, line_ix));
            }
        }
    }
    out
}

/// (hunk, line) of the diff lines a comment quoted (`+ text`, `- text`),
/// when every quoted line is in one hunk of the edit; empty otherwise.
pub fn patch_lines_matching(edit: &FileEdit, quoted: &[&str]) -> HashSet<(usize, usize)> {
    if quoted.is_empty() {
        return HashSet::new();
    }
    for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
        let shown: Vec<String> = hunk.lines.iter().map(quoted_line).collect();
        let found: Option<HashSet<(usize, usize)>> = quoted
            .iter()
            .map(|wanted| {
                shown
                    .iter()
                    .position(|line| line.trim_end() == wanted.trim_end())
                    .map(|line_ix| (hunk_ix, line_ix))
            })
            .collect();
        if let Some(found) = found {
            return found;
        }
    }
    HashSet::new()
}

/// Where a pressed diff line is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub tool_use_id: String,
    pub hunk: usize,
    pub line: usize,
}

pub(super) fn diff_row(line: &DiffLine, picked: bool, commented: bool, p: &Palette) -> Div {
    let (bg, sign, sign_color, text_color) = match line.kind {
        LineKind::Added => (Some(p.diff_add_bg), "+", p.diff_add_text, p.text),
        LineKind::Removed => (Some(p.diff_del_bg), "-", p.diff_del_text, p.text),
        LineKind::Context => (None, " ", p.text_faint, p.text_muted),
        LineKind::NoNewline => (None, " ", p.text_faint, p.text_faint),
    };
    let bg = if picked {
        Some(p.accent.opacity(0.24))
    } else if commented {
        Some(comments::mark_color(p).opacity(0.2))
    } else {
        bg
    };
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .when_some(bg, |this, bg| this.bg(bg))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_color(sign_color)
                .child(sign),
        )
        .child(
            div()
                .flex_shrink_0()
                .pr(theme::sp(3.))
                .whitespace_nowrap()
                .text_color(text_color)
                .child(line.text.replace('\t', "    ")),
        )
}

/// A subagent line: "Agent spawned: <title> (scout · model) ›", "Agent
/// completed: <title> ›", "Message to agent: <title> ›". A click opens the
/// subagent next to this transcript (⌘-click: as the selected thread).
#[allow(clippy::too_many_arguments)]
pub fn render_agent(
    id: impl Into<ElementId>,
    label: &str,
    detail: &str,
    key: String,
    thread: Option<ThreadId>,
    trailing: Vec<AnyElement>,
    tooltip: Option<String>,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let p = ctx.p;
    let text = ctx.text(&key, detail, comments::KIND_SUBAGENT, cx);
    let known = thread.is_some();
    let click: Option<LineClick> = thread.map(|thread| {
        Rc::new(
            move |this: &mut ChatPanel,
                  event: &ClickEvent,
                  _: &mut Window,
                  cx: &mut Context<ChatPanel>| {
                this.open_agent(thread.clone(), event.modifiers().platform, cx);
            },
        ) as LineClick
    });
    let tooltip: Option<SharedString> = tooltip.map(Into::into);
    div()
        .id(SharedString::from(format!("{key}-box")))
        .w_full()
        .px(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .when(known, |this| this.hover(|style| style.bg(p.hover)))
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
        })
        .child(line(
            id,
            Some(label.to_string()),
            text,
            key,
            trailing,
            known.then_some(false),
            click,
            p,
            cx,
        ))
        .into_any_element()
}

/// Drop `<tool_use_error>`-style wrappers from an error message.
pub(super) fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let tag = &rest[open + 1..open + close];
        out.push_str(&rest[..open]);
        if tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '/')
        {
            rest = &rest[open + close + 1..];
        } else {
            out.push('<');
            rest = &rest[open + 1..];
        }
    }
    out.push_str(rest);
    out
}
