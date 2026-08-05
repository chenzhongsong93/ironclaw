use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_reborn_config::{RebornBootConfig, RebornConfigFile};

use crate::LlmKeyStore;
use crate::llm_admin::llm_catalog::{
    RebornLlmCatalogError, apply_stored_api_key, resolve_llm_selection_allow_missing_key,
    resolve_reborn_mission_llm, resolve_reborn_runtime_llm,
};
use crate::llm_admin::llm_config_service::LlmReloadTrigger;
use crate::runtime_input::ResolvedRebornLlm;

/// Live-reload adapter wired by the runtime. Re-resolves the LLM config from
/// `config.toml` + `providers.json` + the stored key, then hot-swaps the
/// running provider's inner backend via the `ironclaw_llm` reload handle.
///
/// In the dual-model setup (`[llm.mission]` configured) it additionally
/// rebuilds and swaps the mission provider chain (TianQuan: novelist subagent →
/// minimax) so both routes hot-reload without a restart.
pub(crate) struct RebornLlmReloadAdapter {
    boot: RebornBootConfig,
    reload_handle: Arc<ironclaw_llm::LlmReloadHandle>,
    session: Arc<ironclaw_llm::SessionManager>,
    keys: LlmKeyStore,
    mission_swappable: Option<Arc<ironclaw_llm::SwappableLlmProvider>>,
}

impl RebornLlmReloadAdapter {
    pub(crate) fn new(
        boot: RebornBootConfig,
        reload_handle: Arc<ironclaw_llm::LlmReloadHandle>,
        session: Arc<ironclaw_llm::SessionManager>,
        keys: LlmKeyStore,
        mission_swappable: Option<Arc<ironclaw_llm::SwappableLlmProvider>>,
    ) -> Self {
        Self {
            boot,
            reload_handle,
            session,
            keys,
            mission_swappable,
        }
    }

    /// Resolve the effective LLM selection, tolerating a required-but-unset
    /// API key env var when a stored key already exists for that provider.
    ///
    /// Without this, a provider selected purely through `config.toml` with a
    /// key that only lives in the encrypted secret store (never written to
    /// an env var — the onboarding menu's stored-key path) would fail closed
    /// here with `ApiKeyEnvUnset` before this adapter ever reaches the
    /// stored-key lookup below, leaving the placeholder gateway wired
    /// forever. Mirrors `resolve_reborn_runtime_llm_with_stored_key_fallback`
    /// in the CLI's `serve` boot path — this adapter is the *other* caller of
    /// the same resolution (live settings-save reload and boot-time reload),
    /// so it needs the same tolerance. Only `ApiKeyEnvUnset` is treated
    /// specially, and only when a stored key genuinely exists; every other
    /// failure (including `ApiKeyEnvUnset` with nothing stored) surfaces
    /// unchanged.
    async fn resolve_effective_llm(
        &self,
        config_file: Option<&RebornConfigFile>,
    ) -> Result<Option<ResolvedRebornLlm>, String> {
        let error = match resolve_reborn_runtime_llm(&self.boot, config_file) {
            Ok(resolved) => return Ok(resolved),
            Err(error) => error,
        };
        let RebornLlmCatalogError::ApiKeyEnvUnset { ref provider, .. } = error else {
            return Err(error.to_string());
        };
        let Some(selection) = config_file.and_then(|file| file.default_llm_slot()) else {
            return Err(error.to_string());
        };
        if !self
            .keys
            .exists(provider)
            .await
            .map_err(|store_error| store_error.to_string())?
        {
            return Err(error.to_string());
        }
        resolve_llm_selection_allow_missing_key(
            selection,
            Some(self.boot.home().providers_file_path().as_path()),
        )
        .map(ResolvedRebornLlm::from_llm_config)
        .map(Some)
        .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl LlmReloadTrigger for RebornLlmReloadAdapter {
    async fn reload(&self) -> Result<(), String> {
        let config_file = RebornConfigFile::load(&self.boot.home().config_file_path())
            .map_err(|error| error.to_string())?;
        let Some(resolved) = self.resolve_effective_llm(config_file.as_ref()).await? else {
            // No provider selected yet, so there is nothing to swap.
            return Ok(());
        };
        let provider_id = resolved.provider_id().to_string();
        let mut config = resolved.config;
        let key_applied = match self
            .keys
            .read(&provider_id)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(stored) => {
                apply_stored_api_key(&mut config, stored);
                true
            }
            None => false,
        };
        let result = self
            .reload_handle
            .reload(&config, Arc::clone(&self.session))
            .await
            .map_err(|error| error.to_string());
        // Never log key material — only provider id and whether a stored key
        // was applied.
        tracing::debug!(
            provider_id = %provider_id,
            key_applied,
            succeeded = result.is_ok(),
            "LLM reload applied to the live provider"
        );
        result?;

        // Dual-model: rebuild and swap the mission provider chain too.
        // Failure to reload the mission route is non-fatal (default chain is
        // already swapped); it is logged and the previous mission provider
        // stays active until the next reload.
        if let Some(mission_swappable) = self.mission_swappable.as_ref() {
            if let Some(mission_resolved) =
                resolve_reborn_mission_llm(&self.boot, config_file.as_ref())
                    .map_err(|error| error.to_string())?
            {
                let mission_provider_id = mission_resolved.provider_id().to_string();
                let mut mission_config = mission_resolved.config;
                if let Some(stored) = self
                    .keys
                    .read(&mission_provider_id)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    apply_stored_api_key(&mut mission_config, stored);
                }
                match ironclaw_llm::build_provider_chain(&mission_config, Arc::clone(&self.session))
                    .await
                {
                    Ok((mission_chain, _, _, _)) => {
                        mission_swappable.swap(mission_chain);
                        tracing::debug!(
                            mission_provider_id = %mission_provider_id,
                            "mission LLM provider reloaded"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            mission_provider_id = %mission_provider_id,
                            "mission LLM reload failed; previous mission provider stays active"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}
