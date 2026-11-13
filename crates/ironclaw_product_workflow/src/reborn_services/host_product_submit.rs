use sha2::{Digest, Sha256};

use super::{
    RebornServicesError, RunProfileId, RunProfileVersion, TurnError, WebUiAuthenticatedCaller,
    WebUiSendMessageRequest, map_turn_error,
};

const ACTION_PREFIX: &str = "host-product-v1:";

pub(super) fn is_host_product_action(action: &str) -> bool {
    action.trim().starts_with(ACTION_PREFIX)
}

/// Domain-separated key for exactly the bytes the host approved. Public chat
/// cannot reserve this replay namespace.
pub(super) fn host_product_action_id(
    caller: &WebUiAuthenticatedCaller,
    request: &WebUiSendMessageRequest,
    profile: &RunProfileId,
    version: RunProfileVersion,
) -> Result<String, RebornServicesError> {
    if request.requested_run_id.is_none()
        || request.thread_id.as_deref().is_none_or(str::is_empty)
        || request.content.as_deref().is_none_or(str::is_empty)
        || !request.attachments.is_empty()
        || request.model.is_some()
    {
        return Err(map_turn_error(TurnError::Unauthorized));
    }
    let payload = serde_json::to_vec(&(
        &caller.tenant_id,
        &caller.user_id,
        &caller.agent_id,
        &caller.project_id,
        request.requested_run_id,
        &request.thread_id,
        &request.content,
        &request.llm_subject,
        profile.as_str(),
        version.as_u64(),
    ))
    .map_err(|_| RebornServicesError::internal_invariant())?;
    let mut digest = Sha256::new();
    digest.update(ACTION_PREFIX.as_bytes());
    digest.update(payload);
    Ok(format!(
        "{ACTION_PREFIX}{}",
        crate::workflow::lower_hex(&digest.finalize())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{TenantId, UserId};
    use ironclaw_turns::TurnRunId;

    #[test]
    fn action_binds_host_inputs_and_rejects_incomplete_or_model_driven_requests() {
        let caller = WebUiAuthenticatedCaller::new(
            TenantId::new("tenant").unwrap(),
            UserId::new("author").unwrap(),
            None,
            None,
        );
        let profile = RunProfileId::new("host-review").unwrap();
        let version = RunProfileVersion::new(1);
        let request = WebUiSendMessageRequest {
            requested_run_id: Some(TurnRunId::new()),
            thread_id: Some("thread".into()),
            content: Some("frozen-command".into()),
            ..Default::default()
        };
        let key = host_product_action_id(&caller, &request, &profile, version).unwrap();
        assert!(is_host_product_action(&key));
        assert!(is_host_product_action(&format!(" {key}")));
        assert!(!is_host_product_action("ordinary-action"));
        assert_eq!(
            host_product_action_id(&caller, &request, &profile, version).unwrap(),
            key
        );
        let mut changed = request.clone();
        changed.content = Some("another-command".into());
        assert_ne!(
            host_product_action_id(&caller, &changed, &profile, version).unwrap(),
            key
        );
        assert_ne!(
            host_product_action_id(&caller, &request, &profile, RunProfileVersion::new(2)).unwrap(),
            key
        );
        changed.model = Some("ordinary-model".into());
        assert!(host_product_action_id(&caller, &changed, &profile, version).is_err());
        changed = request;
        changed.requested_run_id = None;
        assert!(host_product_action_id(&caller, &changed, &profile, version).is_err());
    }
}
