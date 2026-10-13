use super::*;

// -- subthreads -----------------------------------------------------------

/// A Trama subthread call: its title and, when known, the thread it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubthreadLink {
    pub title: String,
    pub thread: Option<ThreadId>,
    /// `send_to_thread`: a message to an existing subthread.
    pub message: bool,
}

/// Find the subthread from the call's input and result, against the
/// threads the sidebar knows (id first, then title).
pub fn subthread_link(
    name: &str,
    input: &Value,
    output: Option<&Value>,
    threads: &[(ThreadId, String)],
) -> SubthreadLink {
    let output_text = output.map(output_text).unwrap_or_default();
    let output_json: Option<Value> = output
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| serde_json::from_str(output_text.trim()).ok());
    let text_field = |value: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_string)
    };
    pub(super) const ID_KEYS: &[&str] =
        &["thread_id", "threadId", "target_thread_id", "subthread_id"];
    pub(super) const TITLE_KEYS: &[&str] = &["title", "name", "description"];
    let id = text_field(input, ID_KEYS)
        .or_else(|| {
            output_json
                .as_ref()
                .and_then(|json| text_field(json, ID_KEYS))
        })
        .or_else(|| {
            threads
                .iter()
                .find(|(id, _)| !id.0.is_empty() && output_text.contains(&id.0))
                .map(|(id, _)| id.0.clone())
        });
    let known = id
        .as_ref()
        .and_then(|id| threads.iter().find(|(known, _)| &known.0 == id));
    let title = text_field(input, TITLE_KEYS)
        .or_else(|| known.map(|(_, title)| title.clone()))
        .or_else(|| {
            output_json
                .as_ref()
                .and_then(|json| text_field(json, TITLE_KEYS))
        })
        .unwrap_or_else(|| "Untitled".into());
    let thread = known.map(|(id, _)| id.clone()).or_else(|| {
        threads
            .iter()
            .find(|(_, known)| known.trim().eq_ignore_ascii_case(title.trim()))
            .map(|(id, _)| id.clone())
    });
    SubthreadLink {
        title: crate::review::comments::one_line(&title, 80),
        thread,
        message: base_name(name) == "send_to_thread",
    }
}

// -- rendering ------------------------------------------------------------

/// Expansion state the tool views read, owned by the chat panel.
#[derive(Default)]
pub struct ToolViewState {
    /// Summary lines opened, by their first entry index.
    pub open_groups: HashSet<usize>,
    /// Thoughts opened, by entry index.
    pub open_thoughts: HashSet<usize>,
    /// Edit rows opened or closed by hand, by tool use id.
    pub edit_open: HashMap<String, bool>,
    /// Commands whose whole output is shown, by tool use id.
    pub full_output: HashSet<String>,
    pub timings: HashMap<String, ToolTiming>,
    /// Diffs built so far, by tool use id, with whether the call had
    /// finished (its result can move the diff to the real line numbers).
    edits: EditCache,
}

/// Built diffs by tool use id, with the `finished` flag they were built at.
pub(super) type EditCache = RefCell<HashMap<String, (bool, Option<Rc<FileEdit>>)>>;

impl ToolViewState {
    pub fn clear(&mut self) {
        self.open_groups.clear();
        self.open_thoughts.clear();
        self.edit_open.clear();
        self.full_output.clear();
        self.timings.clear();
        self.edits.borrow_mut().clear();
    }

    /// The call's diff, built once per state of the call.
    pub fn file_edit(&self, entry: &ChatEntry) -> Option<Rc<FileEdit>> {
        let ChatEntry::ToolCall {
            tool_use_id,
            name,
            input_json,
            finished,
            output,
            ..
        } = entry
        else {
            return None;
        };
        let mut cache = self.edits.borrow_mut();
        if let Some((seen, edit)) = cache.get(tool_use_id)
            && seen == finished
        {
            return edit.clone();
        }
        let edit = file_edit(name, input_json, output.as_ref()).map(Rc::new);
        cache.insert(tool_use_id.clone(), (*finished, edit.clone()));
        edit
    }
}

/// Diffs up to this many lines start open.
pub(super) const OPEN_EDIT_LINES: usize = 24;
/// Lines drawn for one expanded diff or output.
pub(super) const MAX_RENDERED_LINES: usize = 600;

pub fn status_icon(finished: bool, is_error: bool, p: &Palette) -> AnyElement {
    if is_error {
        Icon::new(IconName::CircleX)
            .with_size(px(13.))
            .text_color(p.danger)
            .into_any_element()
    } else if finished {
        Icon::new(IconName::Check)
            .with_size(px(13.))
            .text_color(p.success)
            .into_any_element()
    } else {
        Spinner::new().xsmall().color(p.accent).into_any_element()
    }
}

pub(super) fn chevron(open: bool, p: &Palette) -> Icon {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .with_size(px(12.))
    .text_color(p.text_faint)
}

pub(super) fn counts(additions: u32, deletions: u32, p: &Palette) -> Div {
    h_flex()
        .flex_shrink_0()
        .gap(theme::sp(1.5))
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .when(additions > 0, |this| {
            this.child(
                div()
                    .text_color(p.diff_add_text)
                    .child(format!("+{additions}")),
            )
        })
        .when(deletions > 0, |this| {
            this.child(
                div()
                    .text_color(p.diff_del_text)
                    .child(format!("\u{2212}{deletions}")),
            )
        })
}

// -- document lines ---------------------------------------------------------

/// What the tool views need from the transcript while drawing.
pub struct DocCtx<'a> {
    pub p: &'a Palette,
    pub state: &'a ToolViewState,
    pub texts: &'a mut BlockTexts,
    /// Comments on this turn's blocks, to highlight and place.
    pub marks: &'a [Mark],
    pub turn: usize,
}

impl DocCtx<'_> {
    /// Selectable plain text.
    pub(super) fn text(
        &mut self,
        key: &str,
        plain: &str,
        kind: &'static str,
        cx: &mut Context<ChatPanel>,
    ) -> AnyElement {
        self.texts.view(
            key,
            &escape_markdown(plain),
            plain,
            kind,
            self.turn,
            self.marks,
            cx,
        )
    }

    /// Selectable Markdown.
    pub(super) fn markdown(
        &mut self,
        key: &str,
        markdown: &str,
        kind: &'static str,
        cx: &mut Context<ChatPanel>,
    ) -> AnyElement {
        self.texts
            .view(key, markdown, markdown, kind, self.turn, self.marks, cx)
    }
}

/// What a click on a document line does.
pub type LineClick = Rc<dyn Fn(&mut ChatPanel, &ClickEvent, &mut Window, &mut Context<ChatPanel>)>;

/// A muted document line: a label, its selectable text, what trails it,
/// and a chevron when it opens. A click acts unless it ended a selection
/// in the line's text (`text_key`).
#[allow(clippy::too_many_arguments)]
pub(super) fn line(
    id: impl Into<ElementId>,
    label: Option<String>,
    text: AnyElement,
    text_key: String,
    trailing: Vec<AnyElement>,
    open: Option<bool>,
    click: Option<LineClick>,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let row = h_flex()
        .id(id.into())
        .w_full()
        .min_h(px(24.))
        .gap(theme::sp(1.5))
        .items_start()
        .text_size(theme::text_ui())
        .line_height(px(22.))
        .text_color(p.text_faint)
        .children(label.map(|label| div().flex_shrink_0().text_color(p.text_muted).child(label)))
        .child(div().min_w_0().flex_shrink(1.).child(text))
        .children(
            trailing
                .into_iter()
                .map(|element| div().flex_shrink_0().pt(px(4.)).child(element)),
        )
        .children(open.map(|open| div().flex_shrink_0().pt(px(5.)).child(chevron(open, p))));
    match click {
        Some(click) => row
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if this.text_selected(&text_key, cx) {
                    return;
                }
                click(this, event, window, cx);
            }))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The first line of a thought, as plain text ("Thinking: <line>").
pub fn thinking_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '#' | '`') || c.is_whitespace())
        .trim_end_matches(':');
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// A tool call as one line: what it did and on what ("Read src/a.rs",
/// "Ran cargo test", "Apply patch src/a.rs", "Called get_issue").
pub fn call_line(name: &str, input: &Value) -> (String, String) {
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| input.get(*key).and_then(Value::as_str))
            .map(|text| text.lines().next().unwrap_or_default().trim().to_string())
    };
    let summary = || crate::conversation::chat::tool_summary(name, input);
    let (label, detail) = match base_name(name) {
        "Bash" => ("Ran", field(&["command"]).unwrap_or_else(summary)),
        "Read" | "NotebookRead" => (
            "Read",
            field(&["file_path", "notebook_path", "path"])
                .map(|path| display_path(&path))
                .unwrap_or_else(summary),
        ),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => (
            "Apply patch",
            field(&["file_path", "notebook_path"])
                .map(|path| display_path(&path))
                .unwrap_or_else(summary),
        ),
        "Grep" | "Glob" => ("Searched", summary()),
        "LS" => ("Listed", field(&["path"]).unwrap_or_else(summary)),
        "ToolSearch" => ("Searched tools for", summary()),
        "WebFetch" => ("Fetched", field(&["url"]).unwrap_or_else(summary)),
        "WebSearch" => (
            "Searched the web for",
            field(&["query"]).unwrap_or_else(summary),
        ),
        "Skill" => (
            "Loaded skill",
            field(&["skill", "name", "command"]).unwrap_or_else(summary),
        ),
        "Task" | "Agent" => (
            "Ran subagent",
            field(&["description"]).unwrap_or_else(summary),
        ),
        "TodoWrite" => ("Updated the plan", summary()),
        other => ("Called", other.to_string()),
    };
    (label.to_string(), detail)
}

/// "<title> (scout · Opus 4.1)": what an "Agent spawned:" line names.
pub fn agent_detail(title: &str, profile: Option<&str>, model: Option<&str>) -> String {
    let title = if title.trim().is_empty() {
        "Untitled"
    } else {
        title.trim()
    };
    let model = model.map(crate::conversation::turns::model_label);
    match (profile, model) {
        (Some(profile), Some(model)) => format!("{title} ({profile} \u{b7} {model})"),
        (Some(profile), None) => format!("{title} ({profile})"),
        (None, Some(model)) => format!("{title} ({model})"),
        (None, None) => title.to_string(),
    }
}

/// The label of a subagent's lifecycle line.
pub fn agent_label(
    status: Option<trama_domain::subagent::SubagentStatus>,
    spawned: bool,
) -> &'static str {
    use trama_domain::subagent::SubagentStatus;
    match (spawned, status) {
        (true, _) => "Agent spawned:",
        (false, Some(SubagentStatus::Failed)) => "Agent failed:",
        (false, _) => "Agent completed:",
    }
}

/// Thinking and tool calls: one line, or a folded run as one summary line
/// ("Thought 3 times, ran 2 commands, read 3 files ›") opening to a line
/// per step.
pub fn render_activity(
    range: Range<usize>,
    entries: &[ChatEntry],
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    if range.len() == 1 {
        return render_step(range.start, &entries[range.start], ctx, cx);
    }
    let p = ctx.p;
    let slice = &entries[range.clone()];
    let running = slice.iter().any(|entry| {
        matches!(
            entry,
            ChatEntry::ToolCall {
                finished: false,
                ..
            }
        )
    });
    let failed = slice
        .iter()
        .filter(|entry| matches!(entry, ChatEntry::ToolCall { is_error: true, .. }))
        .count();
    let start = range.start;
    let open = ctx.state.open_groups.contains(&start);
    let key = format!("group-{start}");
    let summary = group_summary(slice);
    let text = ctx.text(&key, &summary, comments::KIND_ACTIVITY, cx);
    let mut trailing = Vec::new();
    if failed > 0 {
        trailing.push(
            div()
                .text_size(theme::text_xs())
                .text_color(p.danger)
                .child(format!("{failed} failed"))
                .into_any_element(),
        );
    }
    if running {
        trailing.push(Spinner::new().xsmall().color(p.accent).into_any_element());
    }
    let header = line(
        ("activity-group", start),
        None,
        text,
        key,
        trailing,
        Some(open),
        Some(Rc::new(move |this, _, _, cx| this.toggle_group(start, cx))),
        p,
        cx,
    );
    let body = open.then(|| {
        let mut list = v_flex()
            .w_full()
            .pl(theme::sp(3.))
            .ml(theme::sp(1.))
            .gap(theme::sp(0.5))
            .border_l_1()
            .border_color(p.border);
        for (offset, entry) in slice.iter().enumerate() {
            list = list.child(render_step(start + offset, entry, ctx, cx));
        }
        list
    });
    v_flex()
        .w_full()
        .gap(theme::sp(0.5))
        .child(header)
        .children(body)
        .into_any_element()
}

/// One step: "Thinking: <first line> ›", "Ran <command> ›", "Read <path> ›";
/// opening shows the thought, the command and its output, or the call's
/// input and output.
pub(super) fn render_step(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let p = ctx.p;
    match entry {
        ChatEntry::Thinking { text } => {
            let open = ctx.state.open_thoughts.contains(&ix);
            let key = format!("thinking-line-{ix}");
            let first = thinking_line(text);
            let detail = ctx.text(&key, &first, comments::KIND_THINKING, cx);
            let header = line(
                ("thinking", ix),
                Some("Thinking:".into()),
                detail,
                key,
                Vec::new(),
                Some(open),
                Some(Rc::new(move |this, _, _, cx| this.toggle_thought(ix, cx))),
                p,
                cx,
            );
            let body = open.then(|| {
                let text = ctx.markdown(
                    &format!("thinking-{ix}"),
                    text.trim(),
                    comments::KIND_THINKING,
                    cx,
                );
                div()
                    .w_full()
                    .pl(theme::sp(3.))
                    .pb(theme::sp(1.))
                    .text_size(theme::text_ui())
                    .line_height(relative(1.55))
                    .text_color(p.text_muted)
                    .child(text)
            });
            v_flex()
                .w_full()
                .child(header)
                .children(body)
                .into_any_element()
        }
        ChatEntry::ToolCall {
            tool_use_id,
            name,
            input_json,
            finished,
            is_error,
            expanded,
            ..
        } => {
            let command = tool_kind(name) == ToolKind::Command;
            let (label, detail) = call_line(name, input_json);
            let key = format!("step-{tool_use_id}");
            let text = ctx.text(
                &key,
                &detail,
                if command {
                    comments::KIND_COMMAND
                } else {
                    comments::KIND_ACTIVITY
                },
                cx,
            );
            let mut trailing = Vec::new();
            if !*finished || *is_error {
                trailing.push(status_icon(*finished, *is_error, p));
            }
            let header = line(
                ("step", ix),
                Some(label),
                text,
                key,
                trailing,
                Some(*expanded),
                Some(Rc::new(move |this, _, _, cx| this.toggle_tool(ix, cx))),
                p,
                cx,
            );
            let body = expanded.then(|| {
                if command {
                    render_command_body(ix, entry, ctx, cx)
                } else {
                    render_call_body(entry, ctx, cx)
                }
            });
            v_flex()
                .w_full()
                .child(header)
                .children(body)
                .into_any_element()
        }
        _ => div().into_any_element(),
    }
}

/// An opened command: the command and the tail of its output as one
/// selectable block, with "Show N more lines" above it.
pub(super) fn render_command_body(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        tool_use_id,
        input_json,
        finished,
        is_error,
        output,
        ..
    } = entry
    else {
        return div().into_any_element();
    };
    let p = ctx.p;
    let command = input_json
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = output.as_ref().map(output_text).unwrap_or_default();
    let (status, body) = command_result(*finished, *is_error, &text);
    let lines: Vec<String> = parse_ansi(&body).iter().map(line_text).collect();
    let show_all = ctx.state.full_output.contains(tool_use_id);
    let (first, hidden) = visible_tail(lines.len(), OUTPUT_TAIL, show_all);
    let end = lines.len().min(first + MAX_RENDERED_LINES);
    let mut block = format!("$ {command}");
    for line in &lines[first..end] {
        block.push('\n');
        block.push_str(line);
    }
    if end < lines.len() {
        block.push_str(&format!("\n… {} more lines not shown", lines.len() - end));
    }
    let duration = ctx
        .state
        .timings
        .get(tool_use_id)
        .and_then(ToolTiming::duration_ms)
        .map(format_duration);
    let status_text = match status {
        ExitStatus::Running => Some(("Running…".to_string(), p.accent_text)),
        ExitStatus::Success => None,
        ExitStatus::Code(code) => Some((format!("Exit code {code}"), p.danger)),
        ExitStatus::Failed => Some(("Failed".to_string(), p.danger)),
        ExitStatus::Interrupted => Some(("Interrupted".to_string(), p.warning)),
    };
    let id = tool_use_id.clone();
    let toggle = (hidden > 0 || show_all && lines.len() > OUTPUT_TAIL).then(|| {
        h_flex()
            .id(("command-more", ix))
            .gap(theme::sp(1.))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_xs())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_output(&id, cx)))
            .child(
                Icon::new(if show_all {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronsUpDown
                })
                .with_size(px(11.)),
            )
            .child(if show_all {
                "Show less".to_string()
            } else {
                format!(
                    "Show {}",
                    comments::plural(hidden, "more line", "more lines")
                )
            })
    });
    let code = ctx.markdown(
        &format!("command-{tool_use_id}"),
        &code_block(&block, ""),
        comments::KIND_COMMAND,
        cx,
    );
    v_flex()
        .w_full()
        .pl(theme::sp(3.))
        .py(theme::sp(1.))
        .gap(theme::sp(1.))
        .children(toggle)
        .child(div().w_full().text_size(theme::text_xs()).child(code))
        .when(status_text.is_some() || duration.is_some(), |this| {
            let (label, color) = status_text.unwrap_or_else(|| ("Done".into(), p.text_faint));
            this.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .text_size(theme::text_xs())
                    .child(div().text_color(color).child(label))
                    .children(duration.map(|d| div().text_color(p.text_faint).child(d))),
            )
        })
        .into_any_element()
}

/// An opened call: its input and output as one selectable block.
pub(super) fn render_call_body(
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        tool_use_id,
        input_json,
        is_error,
        output,
        ..
    } = entry
    else {
        return div().into_any_element();
    };
    let mut markdown = format!("Input\n\n{}", code_block(&pretty_json(input_json), "json"));
    if let Some(output) = output {
        let label = if *is_error { "Error" } else { "Output" };
        let text = output_text(output);
        let text = if text.len() > OUTPUT_LIMIT {
            let mut end = OUTPUT_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
        } else {
            text
        };
        markdown.push_str(&format!("\n\n{label}\n\n{}", code_block(&text, "")));
    }
    let view = ctx.markdown(
        &format!("call-{tool_use_id}"),
        &markdown,
        comments::KIND_TOOL,
        cx,
    );
    div()
        .w_full()
        .pl(theme::sp(3.))
        .py(theme::sp(1.))
        .text_size(theme::text_xs())
        .text_color(ctx.p.text_muted)
        .child(view)
        .into_any_element()
}

/// Bytes of a call's input or output shown when opened.
pub(super) const OUTPUT_LIMIT: usize = 4000;

/// A tool input or result as readable text: strings as they are, other
/// JSON pretty-printed, capped.
pub fn pretty_json(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    if text.len() > OUTPUT_LIMIT {
        let mut end = OUTPUT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
    } else {
        text
    }
}
