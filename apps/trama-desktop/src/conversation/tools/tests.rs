use super::*;
// `gpui_kit::*` brings GPUI's own `test` attribute into scope.
use core::prelude::v1::test;
use serde_json::json;

fn call(name: &str, input: Value) -> ChatEntry {
    ChatEntry::ToolCall {
        tool_use_id: format!("t-{name}"),
        name: name.into(),
        input_json: input,
        finished: true,
        is_error: false,
        expanded: false,
        output: None,
    }
}

fn thought() -> ChatEntry {
    ChatEntry::Thinking { text: "hmm".into() }
}

fn text(markdown: &str) -> ChatEntry {
    ChatEntry::Assistant {
        markdown: markdown.into(),
        streaming: false,
        message_id: None,
    }
}

fn kinds(lines: &[DiffLine]) -> String {
    lines
        .iter()
        .map(|line| match line.kind {
            LineKind::Added => '+',
            LineKind::Removed => '-',
            LineKind::Context => ' ',
            LineKind::NoNewline => '\\',
        })
        .collect()
}

#[test]
fn tools_are_classified_by_name() {
    assert_eq!(tool_kind("Edit"), ToolKind::Edit);
    assert_eq!(tool_kind("Write"), ToolKind::Edit);
    assert_eq!(tool_kind("Bash"), ToolKind::Command);
    assert_eq!(tool_kind("mcp__trama__spawn_subagent"), ToolKind::Subthread);
    assert_eq!(tool_kind("mcp__trama__send_to_thread"), ToolKind::Subthread);
    assert_eq!(
        tool_kind("mcp__trama-workspace__spawn_subagent"),
        ToolKind::Subthread
    );
    assert_eq!(
        tool_kind("mcp__trama-workspace__send_to_thread"),
        ToolKind::Subthread
    );
    assert_ne!(tool_kind("mcp__other__spawn_subagent"), ToolKind::Subthread);
    assert_ne!(
        tool_kind("mcp__trama-workspace__list_threads"),
        ToolKind::Subthread
    );
    assert_eq!(tool_kind("Grep"), ToolKind::Minor(Activity::Search));
    assert_eq!(tool_kind("Task"), ToolKind::Minor(Activity::Subagent));
    assert_eq!(
        tool_kind("mcp__github__get_issue"),
        ToolKind::Minor(Activity::Other)
    );
}

#[test]
fn runs_fold_into_groups_and_edit_blocks() {
    let entries = vec![
        thought(),
        call("Skill", json!({})),
        call("Grep", json!({})),
        text("Found it."),
        call("Read", json!({})),
        call("Edit", json!({})),
        call("Write", json!({})),
        call("Bash", json!({})),
        thought(),
        call("mcp__trama__spawn_subagent", json!({})),
    ];
    assert_eq!(
        layout_items(&entries),
        vec![
            Item::Group(0..3),
            Item::Single(3),
            Item::Single(4),
            Item::Edits(5..7),
            // A command folds with the thought after it.
            Item::Group(7..9),
            Item::Single(9),
        ]
    );
    // A lone thought keeps its own line ("Thinking: …").
    assert_eq!(
        layout_items(&[text("a"), thought(), text("b")]),
        vec![Item::Single(0), Item::Single(1), Item::Single(2)]
    );
}

#[test]
fn activity_lines_read_like_delta() {
    assert_eq!(
        group_summary(&[
            thought(),
            call("Bash", json!({"command": "ls"})),
            call("Bash", json!({"command": "pwd"})),
            call("Read", json!({"file_path": "a.rs"})),
            thought(),
        ]),
        "Thought 2 times, ran 2 commands, read a file"
    );
    assert_eq!(
        thinking_line("\n**Patching approved plan**\n\nThen more."),
        "Patching approved plan"
    );
    assert_eq!(thinking_line("## Plan:\nx"), "Plan");
    assert!(thinking_line(&"word ".repeat(40)).ends_with('…'));
    assert_eq!(
        call_line("Bash", &json!({"command": "cargo test\n--all"})),
        ("Ran".to_string(), "cargo test".to_string())
    );
    assert_eq!(
        call_line("Read", &json!({"file_path": "src/main.rs"})),
        ("Read".to_string(), "src/main.rs".to_string())
    );
    assert_eq!(
        call_line("Edit", &json!({"file_path": "plans/user.md"})),
        ("Apply patch".to_string(), "plans/user.md".to_string())
    );
    assert_eq!(
        call_line("Skill", &json!({"skill": "trama"})),
        ("Loaded skill".to_string(), "trama".to_string())
    );
    assert_eq!(
        call_line("mcp__github__get_issue", &json!({"number": 3})),
        ("Called".to_string(), "get_issue".to_string())
    );
    assert_eq!(
        agent_detail(
            "Validate target identity",
            Some("scout"),
            Some("claude-opus-4-1")
        ),
        "Validate target identity (scout \u{b7} Opus 4.1)"
    );
    assert_eq!(agent_detail(" ", None, None), "Untitled");
    assert_eq!(agent_detail("T", Some("worker"), None), "T (worker)");
    use trama_domain::subagent::SubagentStatus;
    assert_eq!(agent_label(None, true), "Agent spawned:");
    assert_eq!(
        agent_label(Some(SubagentStatus::Completed), false),
        "Agent completed:"
    );
    assert_eq!(
        agent_label(Some(SubagentStatus::Failed), false),
        "Agent failed:"
    );
}

#[test]
fn comments_find_their_lines_in_an_edit() {
    let edit = file_edit(
        "Edit",
        &json!({"file_path": "/repo/a.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc"}),
        None,
    )
    .unwrap();
    // " a", "-b", "+B", " c"
    assert_eq!(new_lines(&edit), vec![1, 2, 3]);
    let on_b: HashSet<(usize, usize)> = patch_lines_in(&edit, 2..=2);
    assert_eq!(on_b, HashSet::from([(0, 2)]));
    let quoted = patch_lines_matching(&edit, &["- b", "+ B"]);
    assert_eq!(quoted, HashSet::from([(0, 1), (0, 2)]));
    assert!(patch_lines_matching(&edit, &["- b", "+ nope"]).is_empty());
    assert!(patch_lines_matching(&edit, &[]).is_empty());
}

#[test]
fn patch_picks_span_lines_of_one_hunk() {
    let pick = PatchPick {
        tool_use_id: "e1".into(),
        hunk: 0,
        anchor: 4,
        cursor: 2,
    };
    assert_eq!(pick.lines(), 2..=4);
    assert!(pick.contains("e1", 0, 3));
    assert!(!pick.contains("e1", 1, 3));
    assert!(!pick.contains("e2", 0, 3));
    let line = DiffLine {
        kind: LineKind::Removed,
        old_line: Some(3),
        new_line: None,
        text: "old".into(),
    };
    assert_eq!(quoted_line(&line), "- old");
}

#[test]
fn group_summary_counts_in_first_seen_order() {
    let entries = vec![
        thought(),
        call("Skill", json!({})),
        thought(),
        call("Grep", json!({})),
        call("Glob", json!({})),
        call("Read", json!({})),
        call("Read", json!({})),
        thought(),
        call("Grep", json!({})),
        call("WebFetch", json!({})),
        call("Task", json!({})),
        call("mcp__x__y", json!({})),
    ];
    assert_eq!(
        group_summary(&entries),
        "Thought 3 times, loaded a skill, ran 3 searches, read 2 files, browsed the web, ran a subagent, called a tool"
    );
    assert_eq!(group_summary(&[thought()]), "Thought once");
    assert_eq!(group_summary(&[]), "");
}

#[test]
fn edit_input_becomes_a_numbered_unified_diff() {
    let input = json!({
        "file_path": "/repo/src/auth.rs",
        "old_string": "fn login() {\n    todo!()\n}",
        "new_string": "fn login() {\n    let token = issue();\n    Session::new(token)\n}",
    });
    let output = json!(
        "The file /repo/src/auth.rs has been updated. Here's the result of running `cat -n` on a snippet of the edited file:\n    40→}\n    41→\n    42→fn login() {\n    43→    let token = issue();"
    );
    let edit = file_edit("Edit", &input, Some(&output)).unwrap();
    assert_eq!(edit.verb, "Edit");
    assert_eq!(edit.path, "/repo/src/auth.rs");
    assert_eq!((edit.additions, edit.deletions, edit.edits), (2, 1, 1));
    assert_eq!(edit.hunks.len(), 1);
    let hunk = &edit.hunks[0];
    assert_eq!(hunk.header, "@@ -42,3 +42,4 @@");
    assert_eq!(kinds(&hunk.lines), " -++ ");
    assert_eq!(hunk.lines[1].old_line, Some(43));
    assert_eq!(hunk.lines[2].new_line, Some(43));
    assert_eq!(hunk.lines[3].text, "    Session::new(token)");
    assert_eq!(hunk.lines[4].old_line, Some(44));
    assert_eq!(hunk.lines[4].new_line, Some(45));

    // Without a snippet the diff counts from line 1.
    let bare = file_edit("Edit", &input, None).unwrap();
    assert_eq!(bare.hunks[0].header, "@@ -1,3 +1,4 @@");
}

#[test]
fn multi_edit_write_and_notebook_inputs() {
    let multi = file_edit(
        "MultiEdit",
        &json!({"file_path": "a.rs", "edits": [
            {"old_string": "a", "new_string": "b"},
            {"old_string": "c\nd", "new_string": "c"},
        ]}),
        None,
    )
    .unwrap();
    assert_eq!(multi.edits, 2);
    assert_eq!(multi.hunks.len(), 2);
    assert_eq!((multi.additions, multi.deletions), (1, 2));

    let write = file_edit(
        "Write",
        &json!({"file_path": "new.md", "content": "# Title\n\nBody\n"}),
        None,
    )
    .unwrap();
    assert_eq!(write.verb, "Write");
    assert_eq!((write.additions, write.deletions), (3, 0));
    assert_eq!(write.hunks[0].header, "@@ -0,0 +1,3 @@");

    let notebook = file_edit(
        "NotebookEdit",
        &json!({"notebook_path": "n.ipynb", "new_source": "x = 1", "edit_mode": "insert"}),
        None,
    )
    .unwrap();
    assert_eq!(notebook.verb, "Edit notebook");
    assert_eq!(notebook.additions, 1);
    assert!(file_edit("Edit", &json!({}), None).is_none());
    assert_eq!(multi.path, "a.rs");
}

#[test]
fn long_unchanged_runs_split_into_hunks() {
    let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    let new = old
        .replace("line 2\n", "line two\n")
        .replace("line 18\n", "line eighteen\n");
    let lines = line_diff(&old, &new, 1, 1);
    let hunks = build_hunks(&lines, 3);
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0].header, "@@ -1,5 +1,5 @@");
    assert_eq!(hunks[1].header, "@@ -15,6 +15,6 @@");
    assert!(build_hunks(&line_diff("same", "same", 1, 1), 3).is_empty());
}

#[test]
fn bash_results_read_exit_codes() {
    assert_eq!(
        command_result(true, true, "Exit code 2\nerror: no such file"),
        (ExitStatus::Code(2), "error: no such file".into())
    );
    assert_eq!(
        command_result(true, false, "ok\n"),
        (ExitStatus::Success, "ok\n".into())
    );
    assert_eq!(
        command_result(true, true, "permission denied"),
        (ExitStatus::Failed, "permission denied".into())
    );
    assert_eq!(
        command_result(true, true, "[Request interrupted by user for tool use]").0,
        ExitStatus::Interrupted
    );
    assert_eq!(command_result(false, false, "").0, ExitStatus::Running);
    assert_eq!(
        output_text(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
        "a\nb"
    );
    assert_eq!(output_text(&json!({"stdout": "out", "stderr": ""})), "out");
}

#[test]
fn ansi_colors_are_kept_and_other_escapes_dropped() {
    let lines = parse_ansi(
        "\u{1b}[1;32mok\u{1b}[0m done\n\u{1b}]0;title\u{7}\u{1b}[2Kprogress 10%\rprogress 100%\n\u{1b}[38;5;196mred\u{1b}[39m\tx\n\n",
    );
    assert_eq!(lines.len(), 3, "trailing blank lines are dropped");
    assert_eq!(
        lines[0],
        vec![
            AnsiSpan {
                text: "ok".into(),
                style: AnsiStyle {
                    fg: Some(AnsiColor::Basic(2)),
                    bold: true,
                    dim: false
                }
            },
            AnsiSpan {
                text: " done".into(),
                style: AnsiStyle::default()
            },
        ]
    );
    assert_eq!(line_text(&lines[1]), "progress 100%");
    assert_eq!(lines[2][0].style.fg, Some(AnsiColor::Rgb(255, 0, 0)));
    assert_eq!(line_text(&lines[2]), "red    x");
}

#[test]
fn output_shows_the_tail_until_expanded() {
    assert_eq!(visible_tail(40, 12, false), (28, 28));
    assert_eq!(visible_tail(40, 12, true), (0, 0));
    assert_eq!(visible_tail(12, 12, false), (0, 0));
    assert_eq!(format_duration(420), "420ms");
    assert_eq!(format_duration(2_340), "2.3s");
    assert_eq!(format_duration(64_000), "1m 04s");
    let timing = ToolTiming {
        started_ms: 1_000,
        finished_ms: Some(3_500),
    };
    assert_eq!(timing.duration_ms(), Some(2_500));
}

#[test]
fn subthread_links_resolve_by_id_then_title() {
    let threads = vec![
        (ThreadId::from("01ABC"), "Fix the parser".to_string()),
        (ThreadId::from("01DEF"), "Write docs".to_string()),
    ];
    let by_output = subthread_link(
        "mcp__trama__spawn_subagent",
        &json!({"title": "Fix the parser", "prompt": "…"}),
        Some(&json!([{"type": "text", "text": "{\"thread_id\": \"01ABC\"}"}])),
        &threads,
    );
    assert_eq!(by_output.title, "Fix the parser");
    assert_eq!(by_output.thread, Some(ThreadId::from("01ABC")));
    assert!(!by_output.message);

    let by_title = subthread_link(
        "mcp__trama__spawn_subagent",
        &json!({"title": "write docs"}),
        None,
        &threads,
    );
    assert_eq!(by_title.thread, Some(ThreadId::from("01DEF")));

    let message = subthread_link(
        "mcp__trama__send_to_thread",
        &json!({"thread_id": "01DEF", "text": "go on"}),
        None,
        &threads,
    );
    assert!(message.message);
    assert_eq!(message.title, "Write docs");

    let unknown = subthread_link(
        "mcp__trama__spawn_subagent",
        &json!({"title": "Elsewhere"}),
        Some(&json!("Started thread 01ZZZ")),
        &threads,
    );
    assert_eq!(unknown.thread, None);
    assert_eq!(unknown.title, "Elsewhere");

    // The daemon's own MCP server (`trama-workspace`) answers in prose.
    let daemon = subthread_link(
        "mcp__trama-workspace__spawn_subagent",
        &json!({"title": "Scout the parser", "profile": "scout"}),
        Some(
            &json!([{"type": "text", "text": "Spawned scout subagent thread 01ABC (Scout the parser). It runs on its own."}]),
        ),
        &threads,
    );
    assert_eq!(daemon.thread, Some(ThreadId::from("01ABC")));
    assert!(!daemon.message);
}

#[test]
fn error_tags_are_stripped() {
    assert_eq!(
        strip_tags("<tool_use_error>String to replace not found</tool_use_error>"),
        "String to replace not found"
    );
    assert_eq!(strip_tags("a < b > c"), "a < b > c");
}
