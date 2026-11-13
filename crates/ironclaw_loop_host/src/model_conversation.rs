//! Opaque model conversation identity from the existing trusted canonical context.
use sha2::{Digest, Sha256};

use super::{AgentLoopHostError, AgentLoopHostErrorKind, LoopRunContext};

pub(super) fn conversation_id(context: &LoopRunContext) -> Result<String, AgentLoopHostError> {
    if context.thread_id != context.scope.thread_id {
        return Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::ScopeMismatch,
            "model conversation thread does not match its canonical scope",
        ));
    }
    let actor = context.actor().map(|actor| actor.user_id.to_string());
    let owner = if context.scope.has_explicit_thread_owner() {
        context
            .scope
            .explicit_owner_user_id()
            .map(ToString::to_string)
    } else {
        actor.clone()
    };
    // Structured encoding distinguishes absent fields and delimiter-like IDs.
    // Normalize the effective owner; never hash a per-run nonce or prompt content.
    let encoded = serde_json::to_vec(&(
        "ironclaw-model-conversation/1",
        &context.scope.tenant_id,
        &context.scope.agent_id,
        &context.scope.project_id,
        &context.thread_id,
        owner,
        actor,
        &context.llm_subject,
    ))
    .map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::ScopeMismatch,
            "model conversation scope could not be encoded",
        )
    })?;
    Ok(hex::encode(Sha256::digest(encoded)))
}
