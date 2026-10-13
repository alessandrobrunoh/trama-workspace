use super::*;
// `super::*` brings GPUI's `test` attribute; use the standard one.
use crate::review::diff::parse_unified_diff;
use core::prelude::v1::test;
use serde_json::{Value, json};

const TIME: &str = "2026-10-12T10:00:00Z";

/// Snapshots come from JSON so the tests read like the wire format.
fn snap(id: &str, parent: Option<&str>, target: Value, state: Value) -> CommentSnapshot {
    serde_json::from_value(json!({
        "id": id, "thread_id": "t", "parent_comment_id": parent,
        "author": {"kind": "human", "id": "user"},
        "target": target, "body": format!("body {id}"), "excerpt": null,
        "created_at": TIME, "state": state, "anchor_status": null,
    }))
    .unwrap()
}

fn on_message(id: &str, start: usize, end: usize) -> Value {
    json!({"target": "message_range", "message_id": id, "start_byte": start, "end_byte": end})
}

fn on_code(path: &str, start: u32, end: u32) -> Value {
    json!({"target": "code", "anchor": {"repository_id": "r", "path": path,
        "revision": 0, "start_line": start, "end_line": end,
        "text_hash": null, "context_hash": null}})
}

fn on_hunk(path: &str, header: &str) -> Value {
    json!({"target": "diff_hunk", "repository_id": "r", "path": path,
        "base_revision": "abc", "hunk_header": header})
}

fn pending() -> Value {
    json!({"kind": "pending"})
}

fn delivered(turn: u32, answer: Option<&str>) -> Value {
    json!({"kind": "delivered", "message_id": "u1", "turn": turn,
        "delivered_at": TIME, "answer_message_id": answer})
}

fn resolved() -> Value {
    json!({"kind": "resolved", "resolved_at": TIME, "delivery": null})
}

#[test]
fn selection_maps_to_message_bytes_with_base_and_trimming() {
    let source = "  Hello world  ";
    assert_eq!(selection_to_message_range(source, 0, 0..15), Some(2..13));
    assert_eq!(
        selection_to_message_range(source, 100, 2..7),
        Some(102..107),
        "offset by the hidden comment block"
    );
    assert_eq!(selection_to_message_range(source, 0, 0..2), None);
    assert_eq!(selection_to_message_range(source, 0, 5..5), None);
    assert_eq!(selection_to_message_range(source, 0, 9..3), None);
    assert_eq!(
        selection_to_message_range(source, 0, 8..400),
        Some(8..13),
        "clamped to the source"
    );
}

#[test]
fn selection_mapping_is_utf8_safe() {
    // "é" is 2 bytes, "🦀" 4 bytes, "日本" 3 bytes each.
    let source = "café 🦀 日本語";
    let crab = source.find('🦀').unwrap();
    assert_eq!(
        selection_to_message_range(source, 0, crab..crab + 4),
        Some(crab..crab + 4)
    );
    // Ends inside a character snap outward to whole characters.
    assert_eq!(
        selection_to_message_range(source, 0, crab + 1..crab + 3),
        Some(crab..crab + 4)
    );
    let e = source.find('é').unwrap();
    let range = selection_to_message_range(source, 7, e + 1..e + 2).unwrap();
    assert_eq!(range, 7 + e..7 + e + 2);
    let ja = source.find('日').unwrap();
    let range = selection_to_message_range(source, 0, ja..source.len()).unwrap();
    assert_eq!(&source[range], "日本語");
    // The mapped range always slices the message text cleanly.
    let message = format!("{}{source}", "x".repeat(7));
    for start in 0..source.len() {
        for end in start..=source.len() {
            if let Some(range) = selection_to_message_range(source, 7, start..end) {
                assert!(message.is_char_boundary(range.start));
                assert!(message.is_char_boundary(range.end));
                assert!(!message[range].trim().is_empty());
            }
        }
    }
}

const BLOCK: &str = "<review-comments>\n\
The user left comments on specific passages. Each quotes the passage it is about; address every comment.\n\
\n\
[1] On this passage of your reply:\n\
> use a map\n\
Comment: why not a vec?\n\
\n\
[2] On src/lib.rs line 3:\n\
```rust\n\
let x = 1;\n\
```\n\
Comment: rename\n\
Reply: and document it\n\
</review-comments>";

#[test]
fn prompt_block_splits_off_the_delivered_comments() {
    let text = format!("{BLOCK}\n\nPlease continue.");
    let block = split_prompt_block(&text).unwrap();
    assert_eq!(block.entries, 2);
    assert_eq!(&text[block.block.clone()], BLOCK);
    assert_eq!(&text[block.body_start..], "Please continue.");
    assert_eq!(prompt_body(&text), "Please continue.");

    // Comments only (`SendComments`): no body.
    let only = split_prompt_block(BLOCK).unwrap();
    assert_eq!(only.body_start, BLOCK.len());
    assert_eq!(prompt_body(BLOCK), "");

    assert_eq!(split_prompt_block("plain prompt"), None);
    assert_eq!(prompt_body("plain prompt"), "plain prompt");
    assert_eq!(
        split_prompt_block("<review-comments>\nunterminated"),
        None,
        "an unclosed block is ordinary text"
    );
}

#[test]
fn prompt_block_ignores_a_closing_tag_quoted_inside_a_comment() {
    let tricky = "<review-comments>\nintro\n\n[1] On this passage:\n> x\nComment: the tag\n</review-comments> appears here\n[2] On y:\nComment: z\n</review-comments>\n\nbody text";
    let block = split_prompt_block(tricky).unwrap();
    assert_eq!(block.entries, 2);
    assert_eq!(&tricky[block.body_start..], "body text");
}

#[test]
fn groups_put_replies_under_roots_and_rank_phases() {
    let comments = vec![
        snap("a", None, on_message("m1", 0, 3), delivered(2, Some("m9"))),
        snap("b", None, on_code("src/a.rs", 3, 3), pending()),
        snap("a1", Some("a"), on_message("m1", 0, 3), pending()),
        snap("a2", Some("a1"), on_message("m1", 0, 3), delivered(2, None)),
        snap("c", None, on_message("m2", 0, 1), resolved()),
        snap("c1", Some("c"), on_message("m2", 0, 1), pending()),
        snap(
            "x1",
            Some("missing"),
            on_message("m3", 0, 1),
            delivered(1, None),
        ),
    ];
    let groups = group_comments(&comments);
    let ids: Vec<&str> = groups.iter().map(|g| g.id().0.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c", "x1"], "roots in creation order");
    let replies: Vec<&str> = groups[0].replies.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(replies, ["a1", "a2"], "a reply to a reply joins its root");

    assert_eq!(groups[0].phase(), Phase::Pending, "a pending reply reopens");
    assert!(groups[0].open_by_default());
    assert_eq!(groups[1].phase(), Phase::Pending);
    assert_eq!(groups[2].phase(), Phase::Resolved(None));
    assert_eq!(
        groups[2].pending_count(),
        0,
        "resolved threads are never sent"
    );
    assert!(!groups[2].open_by_default());
    match groups[3].phase() {
        Phase::Delivered(delivery) => {
            assert_eq!(delivery.turn, 1);
            assert_eq!(delivery.answer_message_id, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(pending_total(&groups), 2);

    let answered = group_comments(&[snap(
        "d",
        None,
        on_message("m1", 0, 1),
        delivered(4, Some("m7")),
    )]);
    assert!(matches!(
        answered[0].phase(),
        Phase::Delivered(CommentDelivery { turn: 4, .. })
    ));
    assert!(!answered[0].open_by_default());
}

#[test]
fn message_anchors_are_relative_to_the_rendered_source() {
    let text = "<review-comments>\n[1] x\n</review-comments>\n\nnaïve plan here";
    let base = split_prompt_block(text).unwrap().body_start;
    let source = &text[base..];
    let plan = base + source.find("plan").unwrap();
    let comments = vec![
        snap("late", None, on_message("m1", plan, plan + 4), pending()),
        snap("early", None, on_message("m1", base, base + 6), pending()),
        snap("in-block", None, on_message("m1", 1, 5), pending()),
        snap("other", None, on_message("m2", 0, 4), pending()),
        snap(
            "past-end",
            None,
            on_message("m1", base, text.len() + 3),
            pending(),
        ),
        snap("code", None, on_code("a.rs", 1, 1), pending()),
    ];
    let groups = group_comments(&comments);
    let (threads, anchors) = message_anchors(&groups, &MessageId::from("m1"), base, source);
    assert_eq!(
        threads,
        vec![0, 1, 2, 4],
        "every thread on the message is listed"
    );
    assert_eq!(
        anchors,
        vec![
            MessageAnchor {
                group: 1,
                range: 0..6
            },
            MessageAnchor {
                group: 0,
                range: plan - base..plan - base + 4
            },
        ],
        "sorted, and only ranges inside the rendered source"
    );
    assert_eq!(&source[anchors[0].range.clone()], "naïve");
}

const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 one
-two
+TWO
+three
 four
@@ -20 +21 @@ fn tail()
-old
+new
";

#[test]
fn diff_slots_place_code_and_hunk_comments() {
    let files = parse_unified_diff(DIFF);
    let hunks = &files[0].hunks;
    let slot = |snapshot: CommentSnapshot| {
        diff_slot(&group_comments(&[snapshot])[0], "r", "src/a.rs", hunks)
    };

    // Lines 2-3 (new side) end on "+three": hunk 0, line 3.
    assert_eq!(
        slot(snap("c", None, on_code("src/a.rs", 2, 3), pending())),
        Some(DiffSlot::Line { hunk: 0, line: 3 })
    );
    // Relocated by the daemon: placed at the new line, badge "moved".
    let mut moved = snap("m", None, on_code("src/a.rs", 7, 7), pending());
    moved.anchor_status = Some(AnchorStatus::Resolved { line: 21 });
    let group = &group_comments(&[moved.clone()])[0];
    assert_eq!(AnchorBadge::for_group(group), Some(AnchorBadge::Moved(21)));
    assert_eq!(slot(moved), Some(DiffSlot::Line { hunk: 1, line: 1 }));
    // Orphaned anchors are never attached to a line.
    let mut orphan = snap("o", None, on_code("src/a.rs", 2, 2), pending());
    orphan.anchor_status = Some(AnchorStatus::Orphaned);
    assert_eq!(
        slot(orphan.clone()),
        Some(DiffSlot::Detached(Detached::Orphaned))
    );
    assert_eq!(
        AnchorBadge::for_group(&group_comments(&[orphan])[0]),
        Some(AnchorBadge::Detached(Detached::Orphaned))
    );
    // Fuzzy stays at the original lines with a warning badge.
    let mut fuzzy = snap("f", None, on_code("src/a.rs", 1, 1), pending());
    fuzzy.anchor_status = Some(AnchorStatus::Fuzzy);
    assert_eq!(
        AnchorBadge::for_group(&group_comments(&[fuzzy.clone()])[0]),
        Some(AnchorBadge::Fuzzy)
    );
    assert_eq!(slot(fuzzy), Some(DiffSlot::Line { hunk: 0, line: 0 }));
    assert_eq!(
        slot(snap("far", None, on_code("src/a.rs", 90, 90), pending())),
        Some(DiffSlot::Detached(Detached::OutsideDiff))
    );

    assert_eq!(
        slot(snap(
            "h",
            None,
            on_hunk("src/a.rs", "@@ -20 +21 @@"),
            pending()
        )),
        Some(DiffSlot::Hunk(1))
    );
    assert_eq!(
        slot(snap(
            "h2",
            None,
            on_hunk("src/a.rs", "@@ -20 +21 @@ fn tail()"),
            pending()
        )),
        Some(DiffSlot::Hunk(1)),
        "a header with its section text still matches"
    );
    assert_eq!(
        slot(snap(
            "gone",
            None,
            on_hunk("src/a.rs", "@@ -5,2 +5,2 @@"),
            pending()
        )),
        Some(DiffSlot::Detached(Detached::Outdated))
    );
    assert_eq!(
        slot(snap("x", None, on_code("src/b.rs", 1, 1), pending())),
        None
    );
    assert_eq!(
        slot(snap("y", None, on_message("m", 0, 1), pending())),
        None
    );
}

#[test]
fn comment_messages_upsert_replace_and_delete() {
    let thread = ThreadId::from("t");
    let mut comments = Vec::new();
    let a = snap("a", None, on_message("m1", 0, 1), pending());
    assert!(apply_comment_message(
        &mut comments,
        &thread,
        &DaemonMessage::CommentCreated(a.clone())
    ));
    let mut updated = a.clone();
    updated.state = serde_json::from_value(delivered(1, None)).unwrap();
    assert!(apply_comment_message(
        &mut comments,
        &thread,
        &DaemonMessage::CommentUpdated(updated.clone())
    ));
    assert_eq!(comments, vec![updated]);

    let mut foreign = snap("z", None, on_message("m1", 0, 1), pending());
    foreign.thread_id = ThreadId::from("other");
    assert!(!apply_comment_message(
        &mut comments,
        &thread,
        &DaemonMessage::CommentCreated(foreign)
    ));
    assert_eq!(comments.len(), 1);

    let b = snap("b", None, on_message("m1", 0, 1), pending());
    assert!(apply_comment_message(
        &mut comments,
        &thread,
        &DaemonMessage::Comments {
            thread_id: thread.clone(),
            comments: vec![a.clone(), b.clone()],
        }
    ));
    assert_eq!(comments.len(), 2);
    assert!(apply_comment_message(
        &mut comments,
        &thread,
        &DaemonMessage::CommentDeleted {
            thread_id: thread.clone(),
            comment_ids: vec![CommentId::from("a")],
        }
    ));
    assert_eq!(comments, vec![b]);
}

#[test]
fn block_end_splits_after_the_anchored_paragraph() {
    let source = "First paragraph, café.\nStill first.\n\nSecond 🦀 one.\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\n\nTail";
    let second = source.find("Second").unwrap();
    let first_end = block_end(source, 3);
    assert_eq!(
        &source[..first_end],
        "First paragraph, café.\nStill first.\n"
    );
    assert!(
        source[first_end..].starts_with('\n'),
        "the blank line opens the next part"
    );
    let crab = source.find('🦀').unwrap();
    let end = block_end(source, crab + 4);
    assert_eq!(&source[second..end], "Second 🦀 one.\n");
    // A blank line inside fenced code does not split the block.
    let code = source.find("let a").unwrap();
    let end = block_end(source, code);
    assert!(source[..end].ends_with("```\n"));
    assert_eq!(block_end(source, source.len() - 1), source.len());
    assert_eq!(block_end("one line", 0), "one line".len());
    // Every cut is a character boundary.
    for offset in 0..=source.len() {
        assert!(source.is_char_boundary(block_end(source, offset)));
        assert!(block_end(source, offset) >= offset.min(source.len()));
    }
}

#[test]
fn typing_a_printable_key_starts_a_comment() {
    let key = |text: &str| typed_text(&Keystroke::parse(text).unwrap());
    assert_eq!(key("a"), Some("a".into()));
    assert_eq!(key("shift-a"), Some("A".into()));
    assert_eq!(key("space"), Some(" ".into()));
    assert_eq!(key("alt-e->é"), Some("é".into()));
    assert_eq!(key("cmd-c"), None, "copy stays copy");
    assert_eq!(key("ctrl-a"), None);
    assert_eq!(key("escape"), None);
    assert_eq!(key("enter"), None);
    assert_eq!(key("up"), None);
    assert_eq!(key("f5"), None);
}

#[test]
fn picked_diff_lines_become_code_or_hunk_targets() {
    let files = parse_unified_diff(DIFF);
    let hunk = &files[0].hunks[0];
    match diff_target("r", "src/a.rs", "abc", hunk, 1..=3) {
        CommentTarget::Code { anchor } => {
            assert_eq!((anchor.start_line, anchor.end_line), (2, 3));
            assert_eq!(anchor.path, std::path::PathBuf::from("src/a.rs"));
            assert_eq!(anchor.repository_id.0, "r");
        }
        other => panic!("expected code, got {other:?}"),
    }
    assert_eq!(
        diff_target("r", "src/a.rs", "abc", hunk, 1..=1),
        CommentTarget::DiffHunk {
            repository_id: "r".into(),
            path: "src/a.rs".into(),
            base_revision: "abc".into(),
            hunk_header: "@@ -1,3 +1,4 @@".into(),
        },
        "a removed line has no new-side line: the hunk is the target"
    );
}

#[test]
fn block_comments_quote_their_passage_and_read_back() {
    let body = proxy_body(KIND_THINKING, " line one\n\nline three ", " fix this ");
    assert_eq!(body, "> Thinking: line one\n> \n> line three\n\nfix this");
    assert_eq!(
        parse_proxy(&body),
        Some(Proxy {
            kind: KIND_THINKING,
            excerpt: "line one\n\nline three".into(),
            comment: "fix this".into(),
        })
    );
    assert_eq!(display_body(&body), "fix this");
    assert_eq!(display_body("plain comment"), "plain comment");
    assert_eq!(parse_proxy("plain comment"), None);
    assert_eq!(parse_proxy("> Unknown: x\n\ny"), None);
    assert_eq!(parse_proxy("> Thinking: no blank line"), None);
    let patch = proxy_body(KIND_PATCH, "- old\n+ new", "why?");
    assert_eq!(parse_proxy(&patch).unwrap().excerpt, "- old\n+ new");
    let long = proxy_body(KIND_COMMAND, &"x".repeat(700), "c");
    assert!(parse_proxy(&long).unwrap().excerpt.ends_with('…'));
}

#[test]
fn block_comments_anchor_to_the_turn_message_but_not_its_text() {
    // Stored on the first line of the answer...
    let text = "\n\n  Hello world\nmore";
    let range = proxy_anchor(text).unwrap();
    assert_eq!(&text[range], "Hello world");
    assert_eq!(proxy_anchor(" \n "), None);
    let wide = "é".repeat(150);
    let range = proxy_anchor(&wide).unwrap();
    assert!(range.end <= 200 && wide.is_char_boundary(range.end));
    // ...but neither highlighted nor placed there.
    let mut on_block = snap("b", None, on_message("m", 0, 5), pending());
    on_block.body = proxy_body(KIND_THINKING, "plan", "hmm");
    let groups = group_comments(&[on_block, snap("a", None, on_message("m", 6, 9), pending())]);
    let (threads, anchors) = message_anchors(&groups, &MessageId::from("m"), 0, "Hello world");
    assert_eq!(threads, vec![1]);
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].group, 1);
}

#[test]
fn transcript_diff_lines_anchor_to_code_in_a_known_checkout() {
    // The fixture paths (`/w/...`) are only absolute on Unix.
    if cfg!(windows) {
        return;
    }
    let checkouts = vec![
        ("r1".to_string(), std::path::PathBuf::from("/w/app")),
        (
            "r2".to_string(),
            std::path::PathBuf::from("/w/app/vendor/lib"),
        ),
    ];
    assert_eq!(
        repo_relative(&checkouts, "/w/app/vendor/lib/x.rs"),
        Some(("r2".into(), "x.rs".into()))
    );
    assert_eq!(
        repo_relative(&checkouts, "/w/app/src/y.rs"),
        Some(("r1".into(), "src/y.rs".into()))
    );
    assert_eq!(repo_relative(&checkouts, "/elsewhere/z.rs"), None);
    assert_eq!(repo_relative(&checkouts, "src/a.rs"), None, "ambiguous");
    assert_eq!(
        repo_relative(&checkouts[..1], "src/a.rs"),
        Some(("r1".into(), "src/a.rs".into()))
    );

    let files = parse_unified_diff(DIFF);
    let hunk = &files[0].hunks[0];
    match patch_target(&checkouts, "/w/app/src/a.rs", hunk, 1..=3) {
        Some(CommentTarget::Code { anchor }) => {
            assert_eq!(anchor.repository_id.0, "r1");
            assert_eq!(anchor.path, std::path::PathBuf::from("src/a.rs"));
            assert_eq!((anchor.start_line, anchor.end_line), (2, 3));
            assert!(code_on_edit(&anchor, "/w/app/src/a.rs", &[1, 2]));
            assert!(!code_on_edit(&anchor, "/w/app/src/a.rs", &[7]));
            assert!(!code_on_edit(&anchor, "/w/app/src/b.rs", &[2]));
            assert!(!code_on_edit(&anchor, "/w/app/xsrc/a.rs", &[2]));
        }
        other => panic!("expected code, got {other:?}"),
    }
    assert_eq!(
        patch_target(&checkouts, "/w/app/src/a.rs", hunk, 1..=1),
        None,
        "removed lines only: a block comment quoting them"
    );
    assert_eq!(patch_target(&checkouts, "/tmp/a.rs", hunk, 1..=3), None);
}

#[test]
fn labels_are_short_and_human() {
    assert_eq!(relative_time(100, 110), "just now");
    assert_eq!(relative_time(0, 300), "5m ago");
    assert_eq!(relative_time(0, 7_200), "2h ago");
    assert_eq!(relative_time(0, 3 * 86_400), "3d ago");
    assert_eq!(relative_time(0, 15 * 86_400), "2w ago");
    assert_eq!(relative_time(500, 100), "just now", "clock skew");
    assert_eq!(plural(1, "comment", "comments"), "1 comment");
    assert_eq!(plural(3, "comment", "comments"), "3 comments");
    assert_eq!(one_line("first\nsecond", 80), "first…");
    let own = snap("a", None, on_message("m", 0, 1), pending());
    assert!(can_delete(&own));
    let mut sent = own.clone();
    sent.state = serde_json::from_value(delivered(1, None)).unwrap();
    assert!(!can_delete(&sent));
}
