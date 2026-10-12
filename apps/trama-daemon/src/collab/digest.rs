//! The shared-thread digest: what other participants did since this
//! daemon's last turn in a shared thread, given to the local agent before
//! the participant's prompt (ADR 0013). Bounded; the most recent entries win.
//!
//! Format (the block is omitted when nothing happened):
//!
//! ```text
//! <shared-thread-digest>
//! Other participants worked in this shared thread since your last turn
//! here. Their turns ran on their own machines; their file changes are not
//! in your checkout. Oldest first:
//! [alice] asked: <prompt>
//! [alice's agent] answered: <answer>
//!   tools: Edit src/lib.rs; Bash cargo test
//! [alice] noted: <system row, e.g. a land result>
//! [bob] commented on "<excerpt>": <comment>
//! </shared-thread-digest>
//! ```

use super::REMOTE_ACTOR_PREFIX;
use super::truncate;
use trama_db::TramaDb;
use trama_domain::activity::ActivityItem;
use trama_domain::comment::CommentTarget;
use trama_domain::ids::ThreadId;
use trama_domain::message::{Actor, Role};

const MAX_DIGEST_BYTES: usize = 12 * 1024;
const MAX_ENTRY_BYTES: usize = 1500;
const MAX_TOOLS: usize = 8;

pub(crate) const OPEN: &str = "<shared-thread-digest>";
pub(crate) const CLOSE: &str = "</shared-thread-digest>";

fn remote_name(actor: &Actor) -> Option<&str> {
    match actor {
        Actor::Human { id } => id.strip_prefix(REMOTE_ACTOR_PREFIX),
        _ => None,
    }
}

fn tool_label(item: &ActivityItem) -> Option<String> {
    let ActivityItem::ToolCall { name, input, .. } = item else {
        return None;
    };
    let target = ["file_path", "path", "command", "pattern", "url"]
        .iter()
        .find_map(|key| input.get(*key).and_then(|v| v.as_str()))
        .map(|value| truncate(value.lines().next().unwrap_or_default(), 80));
    Some(match target {
        Some(target) => format!("{name} {target}"),
        None => name.clone(),
    })
}

/// The digest for `thread_id`, or `None` when no other participant did
/// anything since this daemon's last local turn there.
pub(crate) async fn build(db: &TramaDb, thread_id: &ThreadId) -> Option<String> {
    let messages = db.list_messages_by_thread(thread_id).await.ok()?;
    let origins = db.collab_message_origins(thread_id).await.ok()?;
    // This daemon's last turn: the last local prompt or answer.
    let start = messages
        .iter()
        .rposition(|message| {
            matches!(message.role, Role::User | Role::Assistant)
                && !origins.get(&message.id).is_some_and(|(_, remote)| *remote)
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let since = start
        .checked_sub(1)
        .and_then(|index| messages.get(index))
        .map(|message| message.created_at);
    let mut entries: Vec<String> = Vec::new();
    for message in &messages[start..] {
        if !origins.get(&message.id).is_some_and(|(_, remote)| *remote) {
            continue;
        }
        let Some(name) = remote_name(&message.actor) else {
            continue;
        };
        let text = truncate(message.text().trim(), MAX_ENTRY_BYTES);
        let mut entry = match message.role {
            Role::User => format!("[{name}] asked: {text}"),
            Role::Assistant => format!("[{name}'s agent] answered: {text}"),
            _ => format!("[{name}] noted: {text}"),
        };
        if message.role == Role::Assistant {
            if let Ok(Some(items)) = db.turn_activity_for_message(&message.id).await {
                let tools: Vec<String> = items.iter().filter_map(tool_label).collect();
                if !tools.is_empty() {
                    let shown = tools.len().min(MAX_TOOLS);
                    entry.push_str("\n  tools: ");
                    entry.push_str(&tools[..shown].join("; "));
                    if tools.len() > shown {
                        entry.push_str(&format!("; and {} more", tools.len() - shown));
                    }
                }
            }
        }
        entries.push(entry);
    }
    if let Ok(comments) = db.list_comments(thread_id).await {
        for record in comments {
            let comment = record.comment;
            let Some(name) = remote_name(&comment.author) else {
                continue;
            };
            if since.is_some_and(|since| comment.created_at <= since) {
                continue;
            }
            let on = match (&comment.target, comment.excerpt.as_deref()) {
                (_, Some(excerpt)) if !excerpt.trim().is_empty() => {
                    format!(
                        " on \"{}\"",
                        truncate(excerpt.trim(), 200).replace('\n', " ")
                    )
                }
                (CommentTarget::Code { anchor }, _) => {
                    format!(" on {}", anchor.path.display())
                }
                _ => String::new(),
            };
            let verb = if comment.parent_id.is_some() {
                "replied"
            } else {
                "commented"
            };
            entries.push(format!(
                "[{name}] {verb}{on}: {}",
                truncate(comment.body.trim(), MAX_ENTRY_BYTES)
            ));
        }
    }
    if entries.is_empty() {
        return None;
    }
    // Keep the most recent entries within the budget.
    let mut kept: Vec<String> = Vec::new();
    let mut size = 0usize;
    for entry in entries.iter().rev() {
        if size + entry.len() + 1 > MAX_DIGEST_BYTES {
            break;
        }
        size += entry.len() + 1;
        kept.push(entry.clone());
    }
    kept.reverse();
    let omitted = entries.len() - kept.len();
    let mut out = format!(
        "{OPEN}\nOther participants worked in this shared thread since your last turn here. \
         Their turns ran on their own machines; their file changes are not in your checkout. \
         Oldest first:\n"
    );
    if omitted > 0 {
        out.push_str(&format!("({omitted} earlier entries omitted)\n"));
    }
    out.push_str(&kept.join("\n"));
    out.push('\n');
    out.push_str(CLOSE);
    Some(out)
}
