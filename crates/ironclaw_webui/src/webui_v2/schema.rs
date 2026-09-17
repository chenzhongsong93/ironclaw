//! Browser-visible WebChat v2 timeline/event schema.
//!
//! This module is the route-owned rendering contract between the WebUI SSE
//! transport and the browser. It deliberately does not expose adapter routing
//! metadata such as installation ids, reply binding refs, external conversation
//! refs, or delivery attempt ids.

use ironclaw_product_workflow::{
    AuthPromptView, CapabilityActivityView, CapabilityDisplayPreviewView, FinalReplyView,
    GatePromptView, ProductOutboundEnvelope, ProductOutboundPayload, ProductProjectionState,
    ProgressKind, ProgressUpdateView, ProjectionCursor, RebornCancelRunResponse,
    RebornGetRunStateResponse, RebornSubmitTurnResponse, TurnCostView,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebChatV2EventFrame {
    pub cursor: ProjectionCursor,
    #[serde(flatten)]
    pub event: WebChatV2Event,
}

impl WebChatV2EventFrame {
    pub fn from_outbound(envelope: ProductOutboundEnvelope) -> Self {
        Self::from(envelope)
    }

    pub fn cursor(&self) -> &ProjectionCursor {
        &self.cursor
    }

    pub fn event_name(&self) -> &'static str {
        self.event.event_name()
    }
}

impl From<ProductOutboundEnvelope> for WebChatV2EventFrame {
    fn from(envelope: ProductOutboundEnvelope) -> Self {
        let ProductOutboundEnvelope {
            projection_cursor,
            payload,
            ..
        } = envelope;
        Self {
            cursor: projection_cursor,
            event: WebChatV2Event::from(payload),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebChatV2Event {
    Accepted {
        ack: RebornSubmitTurnResponse,
    },
    Running {
        progress: ProgressUpdateView,
    },
    CapabilityProgress {
        progress: ProgressUpdateView,
    },
    CapabilityActivity {
        activity: CapabilityActivityView,
    },
    CapabilityDisplayPreview {
        preview: CapabilityDisplayPreviewView,
    },
    Gate {
        prompt: GatePromptView,
    },
    AuthRequired {
        prompt: AuthPromptView,
    },
    FinalReply {
        reply: FinalReplyView,
    },
    /// Terminal per-turn token usage (ISSUE-IRONCLAW-006). The fields sit at
    /// the frame top level on purpose: downstream billing consumers (TianQuan
    /// `parse_v2_sse_event`) read `input_tokens`/`output_tokens`/`cost_usd`
    /// directly off the frame body.
    TurnCost {
        turn_run_id: String,
        thread_id: String,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: String,
    },
    Cancelled {
        response: RebornCancelRunResponse,
    },
    Failed {
        run_state: RebornGetRunStateResponse,
    },
    ProjectionSnapshot {
        state: ProductProjectionState,
    },
    ProjectionUpdate {
        state: ProductProjectionState,
    },
    KeepAlive,
}

impl WebChatV2Event {
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Accepted { .. } => "accepted",
            Self::Running { .. } => "running",
            Self::CapabilityProgress { .. } => "capability_progress",
            Self::CapabilityActivity { .. } => "capability_activity",
            Self::CapabilityDisplayPreview { .. } => "capability_display_preview",
            Self::Gate { .. } => "gate",
            Self::AuthRequired { .. } => "auth_required",
            Self::FinalReply { .. } => "final_reply",
            Self::TurnCost { .. } => "turn_cost",
            Self::Cancelled { .. } => "cancelled",
            Self::Failed { .. } => "failed",
            Self::ProjectionSnapshot { .. } => "projection_snapshot",
            Self::ProjectionUpdate { .. } => "projection_update",
            Self::KeepAlive => "keep_alive",
        }
    }
}

impl From<ProductOutboundPayload> for WebChatV2Event {
    fn from(value: ProductOutboundPayload) -> Self {
        match value {
            ProductOutboundPayload::FinalReply(reply) => Self::FinalReply { reply },
            ProductOutboundPayload::Progress(progress)
                if progress.kind == ProgressKind::ToolRunning =>
            {
                Self::CapabilityProgress { progress }
            }
            ProductOutboundPayload::CapabilityActivity(activity) => {
                Self::CapabilityActivity { activity }
            }
            ProductOutboundPayload::CapabilityDisplayPreview(preview) => {
                Self::CapabilityDisplayPreview { preview }
            }
            ProductOutboundPayload::Progress(progress) => Self::Running { progress },
            ProductOutboundPayload::GatePrompt(prompt) => Self::Gate { prompt },
            ProductOutboundPayload::AuthPrompt(prompt) => Self::AuthRequired { prompt },
            ProductOutboundPayload::TurnCost(view) => {
                let TurnCostView {
                    turn_run_id,
                    thread_id,
                    input_tokens,
                    output_tokens,
                    cost_usd,
                } = view;
                Self::TurnCost {
                    turn_run_id: turn_run_id.to_string(),
                    thread_id,
                    input_tokens,
                    output_tokens,
                    cost_usd,
                }
            }
            ProductOutboundPayload::ProjectionSnapshot { state } => {
                Self::ProjectionSnapshot { state }
            }
            ProductOutboundPayload::ProjectionUpdate { state } => Self::ProjectionUpdate { state },
            ProductOutboundPayload::KeepAlive => Self::KeepAlive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_cost_frame_matches_tianquan_billing_contract() {
        // ISSUE-IRONCLAW-006: TianQuan parse_v2_sse_event reads event name
        // "turn_cost" with input_tokens/output_tokens/cost_usd at the frame
        // top level. Lock the wire shape.
        let event = WebChatV2Event::TurnCost {
            turn_run_id: "00000000-0000-0000-0000-000000000001".to_string(),
            thread_id: "thread-a".to_string(),
            input_tokens: 1234,
            output_tokens: 567,
            cost_usd: String::new(),
        };
        assert_eq!(event.event_name(), "turn_cost");

        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "turn_cost");
        assert_eq!(json["input_tokens"], 1234);
        assert_eq!(json["output_tokens"], 567);
        assert_eq!(json["cost_usd"], "");
        assert_eq!(json["turn_run_id"], "00000000-0000-0000-0000-000000000001");
        assert_eq!(json["thread_id"], "thread-a");

        let reparsed: WebChatV2Event = serde_json::from_value(json).expect("roundtrip must parse");
        assert_eq!(reparsed, event);
    }

    #[test]
    fn turn_cost_view_from_payload_maps_fields() {
        let payload = ProductOutboundPayload::TurnCost(TurnCostView {
            turn_run_id: ironclaw_turns::TurnRunId::parse("00000000-0000-0000-0000-000000000001")
                .expect("run id"),
            thread_id: "thread-a".to_string(),
            input_tokens: 42,
            output_tokens: 7,
            cost_usd: "0.001".to_string(),
        });
        let event = WebChatV2Event::from(payload);
        match event {
            WebChatV2Event::TurnCost {
                turn_run_id,
                thread_id,
                input_tokens,
                output_tokens,
                cost_usd,
            } => {
                assert_eq!(turn_run_id, "00000000-0000-0000-0000-000000000001");
                assert_eq!(thread_id, "thread-a");
                assert_eq!(input_tokens, 42);
                assert_eq!(output_tokens, 7);
                assert_eq!(cost_usd, "0.001");
            }
            other => panic!("expected TurnCost, got {other:?}"),
        }
    }
}
