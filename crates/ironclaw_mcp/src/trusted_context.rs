//! Optional signer for one explicitly configured host→TianQuan MCP audience.

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use ironclaw_host_api::{ExtensionId, ResourceScope};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use crate::McpTrustedExecutionContext;

type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_VERSION: &str = "TQ-MCP-CONTEXT-V1";

#[derive(Clone)]
pub struct McpTrustedContextSigner {
    provider_id: ExtensionId,
    secret: Option<Vec<u8>>,
    audience: Option<String>,
    path_and_query: Option<String>,
}

impl fmt::Debug for McpTrustedContextSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpTrustedContextSigner")
            .field("provider_id", &self.provider_id)
            .field("configured", &self.is_configured())
            .field("audience", &self.audience)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("trusted MCP context signer configuration or caller context is invalid")]
pub enum McpTrustedContextSignerError {
    InvalidConfiguration,
    MissingAuthenticatedUser,
    MissingRun,
    MissingProject,
    ActorScopeMismatch,
}

impl McpTrustedContextSigner {
    pub fn new(
        provider_id: ExtensionId,
        secret: Vec<u8>,
        audience: String,
    ) -> Result<Self, McpTrustedContextSignerError> {
        let url = Url::parse(&audience)
            .map_err(|_| McpTrustedContextSignerError::InvalidConfiguration)?;
        if secret.len() < 32
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/mcp/internal"
            || url.as_str() != audience
        {
            return Err(McpTrustedContextSignerError::InvalidConfiguration);
        }
        Ok(Self {
            provider_id,
            secret: Some(secret),
            audience: Some(audience),
            path_and_query: Some(url.path().to_string()),
        })
    }

    /// Construct the provider-scoped policy even when secrets are absent.
    /// Matching trusted calls then fail closed instead of falling back to the
    /// legacy, unsigned endpoint.
    pub fn from_env(provider_id: ExtensionId) -> Self {
        let secret = std::env::var("TIANQUAN_MCP_CONTEXT_HMAC_SECRET");
        let audience = std::env::var("TIANQUAN_MCP_CONTEXT_AUDIENCE");
        match (secret, audience) {
            (Ok(secret), Ok(audience)) => {
                Self::new(provider_id.clone(), secret.into_bytes(), audience)
                    .unwrap_or_else(|_| Self::unconfigured(provider_id))
            }
            _ => Self::unconfigured(provider_id),
        }
    }

    pub fn unconfigured(provider_id: ExtensionId) -> Self {
        Self {
            provider_id,
            secret: None,
            audience: None,
            path_and_query: None,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.secret.is_some() && self.audience.is_some() && self.path_and_query.is_some()
    }

    pub fn provider_id(&self) -> &ExtensionId {
        &self.provider_id
    }

    pub fn audience(&self) -> Option<&str> {
        self.audience.as_deref()
    }

    pub(crate) fn sign_request(
        &self,
        scope: &ResourceScope,
        trusted_context: &McpTrustedExecutionContext,
        body: &[u8],
        session_id: Option<&str>,
    ) -> Result<Vec<(String, String)>, McpTrustedContextSignerError> {
        let secret = self
            .secret
            .as_deref()
            .ok_or(McpTrustedContextSignerError::InvalidConfiguration)?;
        let audience = self
            .audience
            .as_deref()
            .ok_or(McpTrustedContextSignerError::InvalidConfiguration)?;
        let path_and_query = self
            .path_and_query
            .as_deref()
            .ok_or(McpTrustedContextSignerError::InvalidConfiguration)?;
        let actor_user_id = trusted_context
            .authenticated_actor_user_id
            .as_ref()
            .ok_or(McpTrustedContextSignerError::MissingAuthenticatedUser)?;
        if actor_user_id != &scope.user_id {
            return Err(McpTrustedContextSignerError::ActorScopeMismatch);
        }
        let run_id = trusted_context
            .run_id
            .as_ref()
            .ok_or(McpTrustedContextSignerError::MissingRun)?;
        let project_id = scope
            .project_id
            .as_ref()
            .ok_or(McpTrustedContextSignerError::MissingProject)?;
        let issued_at_unix_seconds = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| McpTrustedContextSignerError::InvalidConfiguration)?
                .as_secs(),
        )
        .map_err(|_| McpTrustedContextSignerError::InvalidConfiguration)?;
        let claims = serde_json::json!({
            "version": 1,
            "actorUserId": actor_user_id.as_str(),
            "runId": run_id.to_string(),
            "projectId": project_id.as_str(),
            "audience": audience,
            "issuedAtUnixSeconds": issued_at_unix_seconds,
            "nonce": Uuid::new_v4().to_string(),
        });
        let encoded_payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims)
                .map_err(|_| McpTrustedContextSignerError::InvalidConfiguration)?,
        );
        let body_hash = hex::encode(Sha256::digest(body));
        let canonical = format!(
            "{SIGNATURE_VERSION}\nPOST\n{path_and_query}\n{body_hash}\n{encoded_payload}\n{}",
            session_id.unwrap_or_default(),
        );
        let mut mac = HmacSha256::new_from_slice(secret)
            .map_err(|_| McpTrustedContextSignerError::InvalidConfiguration)?;
        mac.update(canonical.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        Ok(vec![
            ("x-tianquan-execution-context".to_string(), encoded_payload),
            ("x-tianquan-execution-signature".to_string(), signature),
        ])
    }
}
