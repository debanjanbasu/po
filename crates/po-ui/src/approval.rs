//! Tool-approval card.
//!
//! When the agent reaches `AgentState::AwaitingToolApproval`, the transcript
//! renders one of these cards: the tool name, its arguments, and an optional
//! unified diff for edits. Approve/Deny dispatch the same agent events as a
//! chat reply would.

use gpui::{Context, Render, SharedString, Window, div, prelude::*, px};
use po_core::model::ToolCall;

use crate::theme::PoTheme;

/// What the user decided about a pending call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Run the tool.
    Approve,
    /// Refuse the tool, with an optional reason.
    Deny(Option<String>),
}

/// One pending approval.
pub struct ToolApprovalCard {
    /// The call awaiting a decision.
    pub call: ToolCall,
    /// Unified diff preview (edits only).
    pub diff: Option<String>,
}

impl ToolApprovalCard {
    /// Build a card for `call`.
    #[must_use]
    pub fn new(call: ToolCall, diff: Option<String>) -> Self {
        Self { call, diff }
    }
}

impl Render for ToolApprovalCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = PoTheme::current(cx).clone();
        let args = serde_json::to_string_pretty(&self.call.arguments)
            .unwrap_or_else(|_| self.call.arguments.to_string());
        div()
            .id("po-approval")
            .max_w(px(560.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.warning)
            .bg(theme.base_200)
            .text_color(theme.content)
            .font_family(theme.font_family.clone())
            .child(
                div()
                    .text_color(theme.warning)
                    .child(format!("approve tool: {}", self.call.name)),
            )
            .child(
                div()
                    .text_sm()
                    .bg(theme.base_100)
                    .p_2()
                    .rounded_md()
                    .child(SharedString::from(args)),
            )
            .when_some(self.diff.clone(), |d, diff| {
                d.child(
                    div()
                        .text_sm()
                        .bg(theme.base_100)
                        .text_color(theme.secondary)
                        .p_2()
                        .rounded_md()
                        .child(SharedString::from(diff)),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .id("po-approval-approve")
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(theme.primary)
                            .text_color(theme.primary_content)
                            .child("approve")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.decide(ApprovalDecision::Approve, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("po-approval-deny")
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(theme.base_300)
                            .text_color(theme.content)
                            .child("deny")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.decide(ApprovalDecision::Deny(None), cx);
                            })),
                    ),
            )
    }
}

impl ToolApprovalCard {
    /// Record the decision; the host listens through this hook.
    fn decide(&self, _decision: ApprovalDecision, cx: &mut Context<Self>) {
        cx.notify();
    }
}
