//! Exact server-selected profile for a host product submission.
use async_trait::async_trait;

use crate::run_profile::{
    ResolvedRunProfile, RunProfileRequestAuthority, RunProfileResolutionError,
    RunProfileResolutionRequest, RunProfileResolver,
};
use crate::{RunProfileId, RunProfileVersion};

pub(super) struct ProductProfileResolver<'a> {
    pub(super) inner: &'a dyn RunProfileResolver,
    pub(super) id: RunProfileId,
    pub(super) version: RunProfileVersion,
}

#[async_trait]
impl RunProfileResolver for ProductProfileResolver<'_> {
    async fn resolve_run_profile(
        &self,
        request: RunProfileResolutionRequest,
    ) -> Result<ResolvedRunProfile, RunProfileResolutionError> {
        if request
            .requested_run_profile
            .as_ref()
            .map(|profile| profile.as_str())
            != Some(self.id.as_str())
        {
            return Err(RunProfileResolutionError::InvalidRequest {
                reason: "host product profile request does not match its selected profile".into(),
            });
        }
        let resolved = self
            .inner
            .resolve_run_profile(request.with_authority(RunProfileRequestAuthority::ProductSurface))
            .await?;
        if resolved.profile_id != self.id || resolved.profile_version != self.version {
            return Err(RunProfileResolutionError::InvalidRequest {
                reason: "host product profile resolution does not match its selected version"
                    .into(),
            });
        }
        Ok(resolved)
    }
}
