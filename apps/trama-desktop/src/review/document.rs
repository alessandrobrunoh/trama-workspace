//! The transcript as one document.
//!
//! Every block (a prompt, the agent's prose, a folded activity line, an
//! approval, a turn's comments) flows in one text column. Nothing marks
//! who wrote a block but a small round avatar in the left gutter, shown
//! only where an author's run starts: the person's initials, the agent's
//! icon, or a collaborator's initials in a shared thread. This module
//! decides the blocks, their authors and where avatars go; the chat panel
//! only draws them.

use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::Role;

use crate::conversation::chat::ChatEntry;
use crate::conversation::tools::Item;
use crate::conversation::turns::Turn;

/// Who wrote a block, as far as the gutter avatar is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AuthorKey {
    /// This machine's user.
    Me,
    /// Another participant of a shared thread.
    Person(String),
    /// The agent of this thread, running here.
    Agent,
    /// Another participant's agent (a mirrored answer).
    PersonAgent(String),
    /// Another thread's agent (a prompt it sent here).
    ThreadAgent(ThreadId),
}

/// What a document block holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockItem {
    /// Entries, laid out as [`crate::conversation::tools::layout_items`] folds them.
    Entries(Item),
    /// The comment threads on a turn, at its end (turn index).
    Comments(usize),
}

/// One block of the document stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocBlock {
    pub item: BlockItem,
    pub author: AuthorKey,
    /// Draw the author's avatar: the block starts the author's run.
    pub avatar: bool,
    /// The turn the block belongs to.
    pub turn: usize,
}

/// The author of one transcript entry. `remote` names the participant
/// behind a mirrored message (and its role); `received` the thread whose
/// agent sent a prompt.
pub fn entry_author(
    entry: &ChatEntry,
    remote: impl Fn(&MessageId) -> Option<(String, Role)>,
    received: impl Fn(&MessageId) -> Option<ThreadId>,
) -> AuthorKey {
    match entry {
        ChatEntry::User {
            message_id: Some(id),
            ..
        } => {
            if let Some(thread) = received(id) {
                AuthorKey::ThreadAgent(thread)
            } else if let Some((name, _)) = remote(id) {
                AuthorKey::Person(name)
            } else {
                AuthorKey::Me
            }
        }
        ChatEntry::User { .. } => AuthorKey::Me,
        ChatEntry::Assistant {
            message_id: Some(id),
            ..
        } => match remote(id) {
            Some((name, Role::Assistant)) => AuthorKey::PersonAgent(name),
            Some((name, _)) => AuthorKey::Person(name),
            None => AuthorKey::Agent,
        },
        _ => AuthorKey::Agent,
    }
}

/// Whether each block starts its author's run (gets the avatar).
pub fn avatar_flags(authors: &[AuthorKey]) -> Vec<bool> {
    authors
        .iter()
        .enumerate()
        .map(|(index, author)| index == 0 || &authors[index - 1] != author)
        .collect()
}

/// The turn holding entry `index`.
fn turn_of(turns: &[Turn], index: usize) -> usize {
    turns
        .iter()
        .rposition(|turn| turn.entries.start <= index)
        .unwrap_or(0)
}

/// The document stream: one block per layout item, each turn's comment
/// threads (when `has_comments` says it has some) right after its last
/// block, and the avatar on every block that starts an author's run.
/// Comments are written by people, so their block is `Me`'s.
pub fn document(
    items: &[Item],
    turns: &[Turn],
    has_comments: impl Fn(usize) -> bool,
    author: impl Fn(&Item) -> AuthorKey,
) -> Vec<DocBlock> {
    let mut blocks: Vec<DocBlock> = Vec::with_capacity(items.len() + turns.len());
    let close_turn = |blocks: &mut Vec<DocBlock>, turn: usize| {
        if has_comments(turn) {
            blocks.push(DocBlock {
                item: BlockItem::Comments(turn),
                author: AuthorKey::Me,
                avatar: false,
                turn,
            });
        }
    };
    let mut current: Option<usize> = None;
    for item in items {
        let turn = turn_of(turns, item.start());
        if let Some(previous) = current
            && previous != turn
        {
            close_turn(&mut blocks, previous);
        }
        current = Some(turn);
        blocks.push(DocBlock {
            item: BlockItem::Entries(item.clone()),
            author: author(item),
            avatar: false,
            turn,
        });
    }
    if let Some(last) = current {
        close_turn(&mut blocks, last);
    }
    let authors: Vec<AuthorKey> = blocks.iter().map(|block| block.author.clone()).collect();
    for (block, avatar) in blocks.iter_mut().zip(avatar_flags(&authors)) {
        block.avatar = avatar;
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::tools::layout_items;
    use crate::conversation::turns::segment_turns;
    use serde_json::json;

    fn user(text: &str, id: &str) -> ChatEntry {
        ChatEntry::User {
            text: text.into(),
            message_id: Some(MessageId::from(id)),
        }
    }

    fn answer(text: &str, id: Option<&str>) -> ChatEntry {
        ChatEntry::Assistant {
            markdown: text.into(),
            streaming: false,
            message_id: id.map(MessageId::from),
        }
    }

    fn call(id: &str, name: &str) -> ChatEntry {
        ChatEntry::ToolCall {
            tool_use_id: id.into(),
            name: name.into(),
            input_json: json!({"file_path": "src/a.rs", "command": "ls"}),
            finished: true,
            is_error: false,
            expanded: false,
            output: None,
        }
    }

    fn thinking(text: &str) -> ChatEntry {
        ChatEntry::Thinking { text: text.into() }
    }

    fn authors_of(entries: &[ChatEntry]) -> impl Fn(&Item) -> AuthorKey + '_ {
        move |item: &Item| {
            entry_author(
                &entries[item.start()],
                |id| match id.0.as_str() {
                    "r1" => Some(("alice".to_string(), Role::User)),
                    "r2" => Some(("alice".to_string(), Role::Assistant)),
                    _ => None,
                },
                |id| (id.0.as_str() == "x1").then(|| ThreadId::from("child")),
            )
        }
    }

    #[test]
    fn blocks_flow_in_one_stream_with_an_avatar_per_author_run() {
        let entries = vec![
            user("Fix the bug", "u1"),
            thinking("plan"),
            call("t1", "Read"),
            call("t2", "Bash"),
            answer("Fixed.", Some("a1")),
            user("Thanks", "u2"),
            answer("You're welcome.", Some("a2")),
        ];
        let items = layout_items(&entries);
        let turns = segment_turns(&entries);
        let blocks = document(&items, &turns, |_| false, authors_of(&entries));
        let shape: Vec<(BlockItem, AuthorKey, bool, usize)> = blocks
            .into_iter()
            .map(|block| (block.item, block.author, block.avatar, block.turn))
            .collect();
        use AuthorKey::*;
        use BlockItem::Entries;
        assert_eq!(
            shape,
            vec![
                (Entries(Item::Single(0)), Me, true, 0),
                // Thinking, a read and a command fold into one line.
                (Entries(Item::Group(1..4)), Agent, true, 0),
                (Entries(Item::Single(4)), Agent, false, 0),
                (Entries(Item::Single(5)), Me, true, 1),
                (Entries(Item::Single(6)), Agent, true, 1),
            ]
        );
    }

    #[test]
    fn comment_blocks_close_their_turn_and_belong_to_the_person() {
        let entries = vec![
            user("Q1", "u1"),
            answer("A1", Some("a1")),
            user("Q2", "u2"),
            answer("A2", Some("a2")),
        ];
        let items = layout_items(&entries);
        let turns = segment_turns(&entries);
        let blocks = document(&items, &turns, |turn| turn == 0, authors_of(&entries));
        let shape: Vec<(BlockItem, bool)> = blocks
            .iter()
            .map(|block| (block.item.clone(), block.avatar))
            .collect();
        assert_eq!(
            shape,
            vec![
                (BlockItem::Entries(Item::Single(0)), true),
                (BlockItem::Entries(Item::Single(1)), true),
                (BlockItem::Comments(0), true),
                // The next prompt continues the person's run: no avatar.
                (BlockItem::Entries(Item::Single(2)), false),
                (BlockItem::Entries(Item::Single(3)), true),
            ]
        );
        // Comments on the last turn come after its last block.
        let blocks = document(&items, &turns, |turn| turn == 1, authors_of(&entries));
        assert_eq!(blocks.last().unwrap().item, BlockItem::Comments(1));
        assert_eq!(blocks.last().unwrap().turn, 1);
    }

    #[test]
    fn collaborators_and_other_agents_start_their_own_runs() {
        let entries = vec![
            user("Mine", "u1"),
            user("Theirs", "r1"),
            answer("Their Claude", Some("r2")),
            answer("Local Claude", None),
            user("From the parent agent", "x1"),
        ];
        let items = layout_items(&entries);
        let turns = segment_turns(&entries);
        let blocks = document(&items, &turns, |_| false, authors_of(&entries));
        let authors: Vec<(AuthorKey, bool)> = blocks
            .into_iter()
            .map(|block| (block.author, block.avatar))
            .collect();
        assert_eq!(
            authors,
            vec![
                (AuthorKey::Me, true),
                (AuthorKey::Person("alice".into()), true),
                (AuthorKey::PersonAgent("alice".into()), true),
                (AuthorKey::Agent, true),
                (AuthorKey::ThreadAgent(ThreadId::from("child")), true),
            ]
        );
        assert_eq!(
            avatar_flags(&[AuthorKey::Agent, AuthorKey::Agent, AuthorKey::Me]),
            vec![true, false, true]
        );
        assert!(avatar_flags(&[]).is_empty());
    }
}
