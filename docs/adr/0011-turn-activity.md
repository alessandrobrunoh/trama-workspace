# ADR 0011: Store each turn's tool activity

- Status: Accepted

## Context

The daemon stored a turn's prompt and its final answer only. Tool calls,
thinking and subagent events reached the desktop as live agent events, and
the desktop kept them in memory (per prompt) to put them back when a thread
was reopened. After a restart, diffs, command blocks and summary rows were
gone.

## Decision

- The daemon accumulates the normalized events of the turn in flight
  (`ToolCallStarted` / `ToolCallFinished`, `ReasoningDelta`,
  `SubagentStarted` / `SubagentFinished`) into `ActivityItem`s
  (`trama_domain::activity`), in stream order, under the same lock that
  broadcasts them.
- When the turn ends, the items are stored with the message that ends it:
  the answer, or the system notice of a failed or cut-off turn. One row per
  turn in `turn_activity` (migration `0009_turn_activity.sql`, only adds a
  table) and a `turn.activity_recorded` event, in one transaction.
- Bounds: each tool input or output keeps at most 32 KiB of JSON (long
  strings lose their middle, so a command's output keeps head and tail), a
  thinking block 16 KiB, a turn 1000 items; once a turn stored 1 MiB, later
  values keep 1 KiB. Cut values are flagged `truncated`.
- Protocol 10 adds `DaemonMessage::TurnActivity { thread_id, message_id,
  items }`, sent on subscribe only: right before the `MessageAppended` of
  the message that ends each stored turn (inherited history included), and
  with `message_id: None` for the turn in flight, after the history.
  History and the in-flight snapshot are sent under the lock that registers
  the subscriber, so each step arrives exactly once (replayed or live).
- The desktop rebuilds its tool entries from the items (unfinished calls of
  a stored turn show as cut off) and drops its in-memory stash.

## Consequences

- Text streamed between tool calls is still stored only as part of the
  final answer, so a replayed turn shows its tool rows before the whole
  answer.
- Turns stored before protocol 10 have no activity.
- A daemon crash mid-turn loses that turn's activity with its answer.
- Turns of other participants in a shared thread (ADR 0009) are not stored
  with activity.
