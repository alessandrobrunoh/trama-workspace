use super::*;

pub(super) fn stop_message(
    running: bool,
    selected_thread: Option<&ThreadId>,
) -> Option<ClientMessage> {
    if !running {
        return None;
    }
    selected_thread.map(|thread_id| ClientMessage::Interrupt {
        thread_id: thread_id.clone(),
    })
}

pub(super) fn stop_transition(
    lifecycle: TurnLifecycle,
    selected_thread: Option<&ThreadId>,
) -> (TurnLifecycle, Option<ClientMessage>) {
    match lifecycle {
        TurnLifecycle::Running => (TurnLifecycle::Stopping, stop_message(true, selected_thread)),
        TurnLifecycle::Stopping => (TurnLifecycle::Stopping, None),
        TurnLifecycle::AwaitingAssistant => (TurnLifecycle::AwaitingAssistant, None),
        TurnLifecycle::Idle => (TurnLifecycle::Idle, None),
    }
}

pub(super) fn accepts_agent_events(lifecycle: TurnLifecycle) -> bool {
    // The protocol has no turn ID, so events are fenced by the current
    // turn's terminal/reconciliation boundary and rely on daemon ordering.
    matches!(lifecycle, TurnLifecycle::Running | TurnLifecycle::Stopping)
}

pub(super) fn can_submit(lifecycle: TurnLifecycle) -> bool {
    lifecycle == TurnLifecycle::Idle
}

pub(super) fn terminal_transition(
    uses_daemon: bool,
    final_assistant_persisted: bool,
) -> TurnLifecycle {
    if uses_daemon && !final_assistant_persisted {
        TurnLifecycle::AwaitingAssistant
    } else {
        TurnLifecycle::Idle
    }
}

pub(super) fn unscoped_error_transition(
    lifecycle: TurnLifecycle,
    message: String,
) -> (TurnLifecycle, String) {
    (lifecycle, message)
}

pub(super) fn stored_assistant_transition(lifecycle: TurnLifecycle) -> TurnLifecycle {
    if lifecycle == TurnLifecycle::AwaitingAssistant {
        TurnLifecycle::Idle
    } else {
        lifecycle
    }
}

pub(super) fn record_message_id(seen: &mut HashSet<MessageId>, id: MessageId) -> bool {
    seen.insert(id)
}

pub(super) fn last_unpersisted_assistant(entries: &[ChatEntry]) -> Option<usize> {
    let last_user = entries
        .iter()
        .rposition(|entry| matches!(entry, ChatEntry::User { .. }))
        .unwrap_or(0);
    entries
        .iter()
        .enumerate()
        .skip(last_user + 1)
        .rev()
        .find_map(|(index, entry)| match entry {
            ChatEntry::Assistant {
                message_id: None, ..
            } => Some(index),
            _ => None,
        })
}

pub(super) fn reconcile_stored_assistant(
    entries: &mut [ChatEntry],
    lifecycle: TurnLifecycle,
    message: &Message,
) -> bool {
    if !matches!(
        lifecycle,
        TurnLifecycle::Running | TurnLifecycle::Stopping | TurnLifecycle::AwaitingAssistant
    ) {
        return false;
    }
    let text = message.text();
    if let Some(index) = last_unpersisted_assistant(entries) {
        if let ChatEntry::Assistant {
            markdown,
            streaming,
            message_id,
        } = &mut entries[index]
        {
            if markdown.is_empty() || (!text.is_empty() && *markdown != text) {
                *markdown = text;
            }
            *streaming = false;
            *message_id = Some(message.id.clone());
            return true;
        }
    }
    false
}

/// Add a stored user message, or adopt the optimistic entry the composer
/// pushed for it. The stored text may start with the `<review-comments>`
/// block of the comments delivered with the prompt, so the typed prompt is
/// compared with the text after it.
pub(super) fn append_stored_user(
    entries: &mut Vec<ChatEntry>,
    pending_prompt: &mut Option<String>,
    message: &Message,
) {
    let text = message.text();
    let body = comments::prompt_body(&text);
    if pending_prompt.as_deref() == Some(body) {
        *pending_prompt = None;
    }
    if let Some(ChatEntry::User {
        text: existing,
        message_id: message_id @ None,
    }) = entries.last_mut()
        && (existing.as_str() == body || existing == &text)
    {
        *existing = text;
        *message_id = Some(message.id.clone());
        return;
    }
    entries.push(ChatEntry::User {
        text,
        message_id: Some(message.id.clone()),
    });
}

pub(super) fn append_stored_assistant(
    entries: &mut Vec<ChatEntry>,
    lifecycle: TurnLifecycle,
    message: &Message,
) {
    if !reconcile_stored_assistant(entries, lifecycle, message) {
        let text = message.text();
        if !text.is_empty() {
            entries.push(ChatEntry::Assistant {
                markdown: text,
                streaming: false,
                message_id: Some(message.id.clone()),
            });
        }
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                entries.push(ChatEntry::ToolCall {
                    tool_use_id: id.clone(),
                    name: name.clone(),
                    input_json: input.clone(),
                    finished: false,
                    is_error: false,
                    expanded: false,
                    output: None,
                });
            }
        }
    }
}

/// Add an approval card unless the daemon is replaying one already shown.
pub(super) fn push_approval(
    entries: &mut Vec<ChatEntry>,
    request_id: String,
    tool_name: String,
    input_json: serde_json::Value,
) {
    let known = entries
        .iter()
        .any(|entry| matches!(entry, ChatEntry::Approval(card) if card.request_id == request_id));
    if !known {
        if let Some(ChatEntry::Assistant { streaming, .. }) = entries.last_mut() {
            *streaming = false;
        }
        entries.push(ChatEntry::Approval(ApprovalCard {
            request_id,
            tool_name,
            input_json,
            state: ApprovalState::Pending,
        }));
    }
}

pub(super) fn set_approval_state(
    entries: &mut [ChatEntry],
    request_id: &str,
    state: ApprovalState,
) -> bool {
    for entry in entries.iter_mut().rev() {
        if let ChatEntry::Approval(card) = entry
            && card.request_id == request_id
        {
            card.state = state;
            return true;
        }
    }
    false
}

/// Move a pending card to `Sending` and build the reply. `None` when the
/// card is unknown or already answered (double clicks, stale shortcuts).
pub(super) fn answer_approval(
    entries: &mut [ChatEntry],
    thread_id: &ThreadId,
    request_id: &str,
    decision: ApprovalDecision,
) -> Option<ClientMessage> {
    let card = entries.iter_mut().find_map(|entry| match entry {
        ChatEntry::Approval(card) if card.request_id == request_id => Some(card),
        _ => None,
    })?;
    if card.state != ApprovalState::Pending {
        return None;
    }
    card.state = ApprovalState::Sending(decision);
    Some(ClientMessage::RespondApproval {
        thread_id: thread_id.clone(),
        request_id: request_id.to_string(),
        decision,
    })
}

/// A finished turn cannot still be waiting for an approval.
pub(super) fn expire_open_approvals(entries: &mut [ChatEntry]) {
    for entry in entries.iter_mut() {
        if let ChatEntry::Approval(card) = entry
            && card.is_open()
        {
            card.state = ApprovalState::Expired;
        }
    }
}

/// Events that only appear inside a running turn. Seen while idle, they mean
/// a turn was started elsewhere (daemon-run subthread prompt, review, land).
pub(super) fn starts_remote_turn(lifecycle: TurnLifecycle, event: &AgentEvent) -> bool {
    lifecycle == TurnLifecycle::Idle
        && matches!(
            event,
            AgentEvent::SessionStarted { .. }
                | AgentEvent::TextDelta { .. }
                | AgentEvent::ToolCallStarted { .. }
        )
}

pub(super) fn billing_badge(info: &SessionInfo) -> BillingBadge {
    BillingBadge {
        billing: info.billing,
        api_key_source: info.api_key_source.clone(),
        agent_version: info.agent_version.clone(),
        permission_mode: info.permission_mode.clone(),
    }
}

pub(super) fn decision_label(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "Allowed once",
        ApprovalDecision::AllowSession => "Allowed for this session",
        ApprovalDecision::Deny => "Denied",
    }
}

/// Compact one-line JSON of a tool input, capped for the approval card.
pub fn approval_preview(input: &serde_json::Value) -> String {
    let text = match input {
        serde_json::Value::Object(map) if map.is_empty() => String::new(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    pub(super) const LIMIT: usize = 280;
    if text.chars().count() > LIMIT {
        let mut out: String = text.chars().take(LIMIT).collect();
        out.push('…');
        out
    } else {
        text
    }
}

pub(super) fn render_approval(
    card: &ApprovalCard,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    use gpui_kit::component::Disableable as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};

    let summary = tool_summary(&card.tool_name, &card.input_json);
    let preview = approval_preview(&card.input_json);
    let open = card.state == ApprovalState::Pending;
    let header = h_flex()
        .w_full()
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .child(
            Icon::new(if card.is_open() {
                IconName::ShieldAlert
            } else {
                IconName::ShieldCheck
            })
            .with_size(px(14.))
            .text_color(if card.is_open() {
                p.warning
            } else {
                p.text_faint
            }),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(if card.is_open() {
                    format!("Allow {}?", card.tool_name)
                } else {
                    card.tool_name.clone()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .text_color(p.text_faint)
                .child(summary),
        );

    let footer: AnyElement = match &card.state {
        ApprovalState::Pending | ApprovalState::Sending(_) => {
            let sending = match card.state {
                ApprovalState::Sending(decision) => Some(decision),
                _ => None,
            };
            let button = |id: &'static str, label: &'static str, keys: &'static str, decision| {
                let request_id = card.request_id.clone();
                let mut button =
                    Button::new(SharedString::from(format!("{id}-{}", card.request_id)))
                        .small()
                        .label(label)
                        .tooltip(keys)
                        .loading(sending == Some(decision))
                        .disabled(!open)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer_approval(&request_id, decision, cx);
                        }));
                button = match decision {
                    ApprovalDecision::AllowOnce => button.primary(),
                    ApprovalDecision::AllowSession => button.ghost(),
                    ApprovalDecision::Deny => button.ghost(),
                };
                button
            };
            h_flex()
                .gap(theme::sp(1.5))
                .items_center()
                .child(button(
                    "approve-once",
                    "Allow once",
                    "\u{2318}Y",
                    ApprovalDecision::AllowOnce,
                ))
                .child(button(
                    "approve-session",
                    "Allow for session",
                    "\u{2318}\u{21e7}Y",
                    ApprovalDecision::AllowSession,
                ))
                .child(button(
                    "approve-deny",
                    "Deny",
                    "\u{2318}\u{21e7}N",
                    ApprovalDecision::Deny,
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(
                            "\u{2318}Y allow · \u{2318}\u{21e7}Y session · \u{2318}\u{21e7}N deny",
                        ),
                )
                .into_any_element()
        }
        ApprovalState::Resolved { decision, reason } => {
            let color = match decision {
                ApprovalDecision::Deny => p.danger,
                _ => p.success,
            };
            h_flex()
                .gap(theme::sp(1.5))
                .items_center()
                .text_size(theme::text_xs())
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(
                    div()
                        .text_color(p.text_muted)
                        .child(decision_label(*decision)),
                )
                .when_some(reason.clone(), |this, reason| {
                    this.child(div().text_color(p.text_faint).child(format!("· {reason}")))
                })
                .into_any_element()
        }
        ApprovalState::Expired => div()
            .text_size(theme::text_xs())
            .text_color(p.text_faint)
            .child("No longer pending: the turn ended.")
            .into_any_element(),
    };

    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            v_flex()
                .w_full()
                .px(theme::sp(3.))
                .py(theme::sp(2.5))
                .gap(theme::sp(2.))
                .rounded(theme::radius())
                .border_1()
                .border_color(if card.is_open() {
                    p.warning.opacity(0.5)
                } else {
                    p.border
                })
                .bg(if card.is_open() {
                    p.warning.opacity(0.06)
                } else {
                    p.elevated
                })
                .child(header)
                .when(card.is_open() && !preview.is_empty(), |this| {
                    this.child(
                        div()
                            .w_full()
                            .px(theme::sp(2.5))
                            .py(theme::sp(1.5))
                            .rounded(theme::radius_sm())
                            .bg(p.bg)
                            .border_1()
                            .border_color(p.border)
                            .font_family(theme::MONO_FONT)
                            .text_size(theme::text_xs())
                            .line_height(relative(1.5))
                            .text_color(p.text_muted)
                            .child(preview),
                    )
                })
                .child(footer),
        )
        .into_any_element()
}

pub(super) fn take_rejected_prompt(
    event: &AgentEvent,
    pending: &mut Option<String>,
) -> Option<String> {
    if matches!(event, AgentEvent::Failed { .. }) {
        pending.take()
    } else {
        None
    }
}
