//! A scripted transcript for screenshots and manual QA without a daemon
//! (debug builds, `TRAMA_DEV_DEMO=1`): a parent thread with folded
//! activity, a subagent, a patch, prose with comments on a passage and on
//! a thought, and the subagent's own transcript for the side pane
//! (`TRAMA_DEV_PANE=1`).

use serde_json::{Value, json};
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::subagent::{SubagentProfile, SubagentStatus};
use trama_protocol::{CommentSnapshot, SpawnedBy, SubagentSnapshot, ThreadKind, ThreadSnapshot};

use crate::conversation::chat::ChatEntry;
use crate::conversation::subagents::{AnnotationKind, AnnotationRow};
use crate::review::comments;

pub const PARENT: &str = "demo-parent";
pub const CHILD: &str = "demo-child";
const SPAWN: &str = "demo-spawn";

const ASK: &str = "Before I write code, please **accept ADR-5** and approve or correct the plan's acceptance criteria. No source code has changed; tests have not been run.";
const ANSWER: &str = "Confirmed\u{2014}Trama now shows ADR-5 as accepted.\n\nLUNA-4 is back to **planned**, and I'm treating \u{201c}procede\u{201d} as approval of the plan criteria. I've updated the plan with the accepted choices: no reason prompt, a non-renewable 60-minute expiry, and all target-authorized actions\u{2014}including account and role changes\u{2014}with the acknowledged elevation risk.\n\nOne configuration note for when the Entra app role is created: use `IMPERSONIFICATE` as its display name and `ROLE_IMPERSONIFICATE` as its token value. No application code has been changed yet; no tests have been run.";

fn user(text: &str, id: &str) -> ChatEntry {
    ChatEntry::User {
        text: text.into(),
        message_id: Some(MessageId::from(id)),
    }
}

fn answer(text: &str, id: &str) -> ChatEntry {
    ChatEntry::Assistant {
        markdown: text.into(),
        streaming: false,
        message_id: Some(MessageId::from(id)),
    }
}

fn thought(text: &str) -> ChatEntry {
    ChatEntry::Thinking { text: text.into() }
}

fn call(id: &str, name: &str, input: Value, output: Value) -> ChatEntry {
    ChatEntry::ToolCall {
        tool_use_id: id.into(),
        name: name.into(),
        input_json: input,
        finished: true,
        is_error: false,
        expanded: false,
        output: Some(output),
    }
}

/// The parent thread's transcript.
pub fn parent_entries() -> Vec<ChatEntry> {
    vec![
        user(
            "Plan user impersonation for cora-backend and cora-frontend. Check which users can be impersonated before writing any code.",
            "u1",
        ),
        thought("**Reading the plan**\n\nThe plan and ADR-5 say who can be a target."),
        call(
            "r1",
            "Read",
            json!({"file_path": "/demo/cora-backend/plans/user-impersonation.md"}),
            json!("# User impersonation"),
        ),
        thought("Check Trama for the open input requests."),
        call("s1", "Skill", json!({"skill": "trama"}), json!("loaded")),
        call(
            "b1",
            "Bash",
            json!({"command": "git status --short"}),
            json!(" M plans/user-impersonation.md"),
        ),
        call(
            "b2",
            "Bash",
            json!({"command": "rg -n ROLE_ADMIN src/main/java"}),
            json!(
                "src/main/java/cora/security/Roles.java:12:    ROLE_ADMIN,\nsrc/main/java/cora/web/AccountController.java:48:@PreAuthorize(\"hasRole('ROLE_ADMIN')\")"
            ),
        ),
        thought("ADR-5 is still proposed."),
        call(
            "m1",
            "mcp__trama-workspace__get_decision",
            json!({"key": "ADR-5"}),
            json!({"status": "proposed"}),
        ),
        call(
            SPAWN,
            "mcp__trama-workspace__spawn_subagent",
            json!({"title": "Validate target identity classification", "profile": "scout"}),
            json!({"thread_id": CHILD}),
        ),
        thought("**Patching approved plan**\n\nUpdate the status line and the criteria heading."),
        call(
            "e1",
            "Edit",
            json!({
                "file_path": "/demo/cora-backend/plans/user-impersonation.md",
                "old_string": "**Repos**: `cora-backend` + `cora-frontend`\n**Status**: Draft \u{2014} target eligibility and session controls are selected; proposed `ADR-5`\n\n## Acceptance criteria (draft \u{2014} confirm before code)",
                "new_string": "**Repos**: `cora-backend` + `cora-frontend`\n**Status**: Approved to proceed \u{2014} `ADR-5` and session policies are accepted\n\n## Acceptance criteria (approved to proceed)",
            }),
            json!("updated"),
        ),
        thought("Re-read the criteria."),
        call(
            "b3",
            "Bash",
            json!({"command": "cargo run -q -- lint plans/"}),
            json!("Exit code 0\nplans/user-impersonation.md: ok"),
        ),
        thought("Report back."),
        call(
            "b4",
            "Bash",
            json!({"command": "git diff --stat"}),
            json!(
                " plans/user-impersonation.md | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)"
            ),
        ),
        thought("Ask for the decision."),
        call(
            "m2",
            "mcp__trama-workspace__request_input",
            json!({"question": "Accept ADR-5?"}),
            json!("ok"),
        ),
        answer(ASK, "a1"),
        user("done \u{2014} procede", "u2"),
        answer(ANSWER, "a2"),
        ChatEntry::Annotation(AnnotationRow {
            message_id: MessageId::from("n1"),
            kind: AnnotationKind::SubagentFinished(SubagentStatus::Completed),
            thread_id: ThreadId::from(CHILD),
            title: "Validate target identity classification".into(),
            body: None,
        }),
    ]
}

/// The subagent's transcript, for the side pane.
pub fn child_entries() -> Vec<ChatEntry> {
    vec![
        user(
            "Inspect the cora-backend identity model and Microsoft Entra/Graph integration to determine whether the confirmed impersonation target rule can reliably exclude service accounts with existing data. Do not edit files or change Trama.",
            "cu1",
        ),
        thought("**Listing backend root**"),
        answer(
            "I'll trace the persisted user model, synchronization path, identity adapters, and Graph mapping for an explicit service-account distinction, then report the evidence and the minimum safe way to add one if it's absent.",
            "ca0",
        ),
        call(
            "c1",
            "LS",
            json!({"path": "/demo/cora-backend"}),
            json!("src\nplans"),
        ),
        thought("Look at the user entity."),
        call(
            "c2",
            "Grep",
            json!({"pattern": "serviceAccount|userType", "path": "src"}),
            json!(""),
        ),
        call(
            "c3",
            "Read",
            json!({"file_path": "/demo/cora-backend/src/main/java/cora/domain/User.java"}),
            json!("class User"),
        ),
        thought("No marker anywhere."),
        answer(
            "### Conclusion\n\n**No reliable server-side distinction between human users and service accounts exists in the backend today.** There is no impersonation target filter in the backend source to assess, and the current user model cannot safely exclude existing service accounts.\n\n### What the code records\n\n- `User` stores profile fields, `activated`, and authorities, but no account type or service-account marker.\n- `AuthenticatedIdentity` has no identity-kind field.",
            "ca1",
        ),
    ]
}

/// The prompt the parent's agent sent the subagent.
pub fn child_received() -> Vec<(MessageId, AnnotationRow)> {
    vec![(
        MessageId::from("cu1"),
        AnnotationRow {
            message_id: MessageId::from("cu1"),
            kind: AnnotationKind::Received,
            thread_id: ThreadId::from(PARENT),
            title: "Add Entra ID user impersonation permission".into(),
            body: None,
        },
    )]
}

/// Both threads as the sidebar would list them.
pub fn threads() -> Vec<ThreadSnapshot> {
    vec![
        ThreadSnapshot {
            id: ThreadId::from(PARENT),
            title: "Add Entra ID user impersonation permission".into(),
            repo_count: 2,
            parent_thread_id: None,
            kind: ThreadKind::Top,
            review_verdict: None,
            model: None,
            depth: 0,
            archived: false,
            subagent: None,
        },
        ThreadSnapshot {
            id: ThreadId::from(CHILD),
            title: "Validate target identity classification".into(),
            repo_count: 2,
            parent_thread_id: Some(ThreadId::from(PARENT)),
            kind: ThreadKind::Sub,
            review_verdict: None,
            model: Some("claude-opus-4-1".into()),
            depth: 1,
            archived: false,
            subagent: Some(SubagentSnapshot {
                spawned_by: SpawnedBy {
                    parent_thread_id: ThreadId::from(PARENT),
                    tool_use_id: Some(SPAWN.into()),
                    profile: SubagentProfile::Scout,
                },
                status: SubagentStatus::Completed,
                detail: None,
            }),
        },
    ]
}

fn snapshot(
    id: &str,
    message: &str,
    text: &str,
    passage: &str,
    body: &str,
    state: Value,
) -> CommentSnapshot {
    let start = text.find(passage).unwrap_or(0);
    serde_json::from_value(json!({
        "id": id, "thread_id": PARENT, "parent_comment_id": null,
        "author": {"kind": "human", "id": "user"},
        "target": {"target": "message_range", "message_id": message,
            "start_byte": start, "end_byte": start + passage.len()},
        "body": body, "excerpt": passage,
        "created_at": "2026-10-12T10:00:00Z", "state": state, "anchor_status": null,
    }))
    .expect("demo comment")
}

/// Comments on the parent: one delivered and answered, one pending on a
/// passage, one pending on a thought.
pub fn parent_comments() -> Vec<CommentSnapshot> {
    let delivered = json!({"kind": "delivered", "message_id": "u2", "turn": 2,
        "delivered_at": "2026-10-12T10:01:00Z", "answer_message_id": "a2"});
    let first_line = ASK.lines().next().unwrap_or_default();
    vec![
        snapshot("c1", "a1", ASK, "accept ADR-5", "done", delivered),
        snapshot(
            "c2",
            "a2",
            ANSWER,
            "`IMPERSONIFICATE` as its display name and `ROLE_IMPERSONIFICATE` as its",
            "idk if i like this thing",
            json!({"kind": "pending"}),
        ),
        snapshot(
            "c3",
            "a1",
            ASK,
            first_line,
            &comments::proxy_body(
                comments::KIND_THINKING,
                "Patching approved plan",
                "Why patch the plan before ADR-5 is accepted?",
            ),
            json!({"kind": "pending"}),
        ),
    ]
}
