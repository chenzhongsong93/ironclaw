//! Operator-scoped storage for LLM provider API-key **values**.
//!
//! The reborn provider catalog (`providers.json`) only ever references an
//! `api_key_env` *name* and the config selection only carries names — inline
//! secret values are rejected. When the webui2 settings surface lets an
//! operator paste an actual key, the value lands here instead: encrypted in the
//! scoped [`SecretStore`] under a fixed per-provider handle, and injected into
//! the resolved `LlmConfig` at provider-build / reload time.
//!
//! LLM configuration is operator-wide (a single instance config, not per-user),
//! so every key is stored under the synthetic system scope
//! ([`ResourceScope::system`]). The handle is derived from the provider id:
//! `llm_provider_<id>_api_key`.

use std::collections::HashSet;
use std::sync::Arc;

use ironclaw_host_api::{ResourceScope, SecretHandle};
use ironclaw_secrets::{SecretMaterial, SecretStore, SecretStoreError};
use thiserror::Error;

/// Thin, operator-scoped wrapper over the shared [`SecretStore`] for LLM keys.
#[derive(Clone)]
pub struct LlmKeyStore {
    store: Arc<dyn SecretStore>,
}

impl LlmKeyStore {
    /// Wrap the instance's shared secret store.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }

    /// Store (or replace) the API-key value for `provider_id`.
    pub async fn put(
        &self,
        provider_id: &str,
        value: SecretMaterial,
    ) -> Result<(), LlmKeyStoreError> {
        let handle = handle_for(provider_id)?;
        self.store
            .put(scope(), handle, value, None)
            .await
            .map_err(LlmKeyStoreError::Store)?;
        Ok(())
    }

    /// [`Self::put`] taking a plain `String` rather than [`SecretMaterial`] —
    /// for callers outside this crate (namely `ironclaw_reborn_cli::onboard`)
    /// that must not depend on `ironclaw_secrets` directly (see
    /// `crates/ironclaw_architecture/tests/reborn_dependency_boundaries.rs::reborn_cli_binary_crate_stays_separate_from_v1_root`,
    /// which pins `ironclaw_reborn_cli`'s allowed workspace dependency set).
    pub async fn put_plaintext(
        &self,
        provider_id: &str,
        value: String,
    ) -> Result<(), LlmKeyStoreError> {
        self.put(provider_id, SecretMaterial::from(value)).await
    }

    /// Whether a stored key exists for `provider_id` (without revealing it).
    pub async fn exists(&self, provider_id: &str) -> Result<bool, LlmKeyStoreError> {
        let handle = handle_for(provider_id)?;
        Ok(self
            .store
            .metadata(&scope(), &handle)
            .await
            .map_err(LlmKeyStoreError::Store)?
            .is_some())
    }

    /// Provider ids that have an operator-stored key.
    pub async fn stored_provider_ids(&self) -> Result<HashSet<String>, LlmKeyStoreError> {
        const PREFIX: &str = "llm_provider_";
        const SUFFIX: &str = "_api_key";

        Ok(self
            .store
            .metadata_for_scope(&scope())
            .await
            .map_err(LlmKeyStoreError::Store)?
            .into_iter()
            .filter_map(|metadata| {
                metadata
                    .handle
                    .as_str()
                    .strip_prefix(PREFIX)?
                    .strip_suffix(SUFFIX)
                    .map(ToString::to_string)
            })
            .collect())
    }

    /// Read back the stored key value for `provider_id`, if any.
    ///
    /// Returns `Ok(None)` when no key is stored. Uses a one-shot lease +
    /// consume; the underlying secret persists, so this is repeatable across
    /// reloads.
    pub async fn read(
        &self,
        provider_id: &str,
    ) -> Result<Option<SecretMaterial>, LlmKeyStoreError> {
        let handle = handle_for(provider_id)?;
        let scope = scope();
        let lease = match self.store.lease_once(&scope, &handle).await {
            Ok(lease) => lease,
            Err(error) if error.is_unknown_secret() => return Ok(None),
            Err(error) => return Err(LlmKeyStoreError::Store(error)),
        };
        let material = self
            .store
            .consume(&scope, lease.id)
            .await
            .map_err(LlmKeyStoreError::Store)?;
        Ok(Some(material))
    }

    /// Delete the stored key for `provider_id`. Returns whether one existed.
    pub async fn delete(&self, provider_id: &str) -> Result<bool, LlmKeyStoreError> {
        let handle = handle_for(provider_id)?;
        self.store
            .delete(&scope(), &handle)
            .await
            .map_err(LlmKeyStoreError::Store)
    }

    /// Store (or replace) the Bearer token for one LLM subject/provider pair.
    /// The secret handle uses a deterministic hash so subject identifiers are
    /// not exposed through secret-store metadata listings.
    pub async fn put_subject_key(
        &self,
        subject: &str,
        provider_id: &str,
        value: SecretMaterial,
    ) -> Result<(), LlmKeyStoreError> {
        let handle = subject_handle_for(subject, provider_id)?;
        self.store
            .put(scope(), handle, value, None)
            .await
            .map(|_| ())
            .map_err(LlmKeyStoreError::Store)
    }

    pub async fn put_subject_key_plaintext(
        &self,
        subject: &str,
        provider_id: &str,
        value: String,
    ) -> Result<(), LlmKeyStoreError> {
        self.put_subject_key(subject, provider_id, SecretMaterial::from(value))
            .await
    }

    pub async fn subject_key_exists(
        &self,
        subject: &str,
        provider_id: &str,
    ) -> Result<bool, LlmKeyStoreError> {
        let handle = subject_handle_for(subject, provider_id)?;
        Ok(self
            .store
            .metadata(&scope(), &handle)
            .await
            .map_err(LlmKeyStoreError::Store)?
            .is_some())
    }

    pub async fn read_subject_key(
        &self,
        subject: &str,
        provider_id: &str,
    ) -> Result<Option<SecretMaterial>, LlmKeyStoreError> {
        let handle = subject_handle_for(subject, provider_id)?;
        let scope = scope();
        let lease = match self.store.lease_once(&scope, &handle).await {
            Ok(lease) => lease,
            Err(error) if error.is_unknown_secret() => return Ok(None),
            Err(error) => return Err(LlmKeyStoreError::Store(error)),
        };
        self.store
            .consume(&scope, lease.id)
            .await
            .map(Some)
            .map_err(LlmKeyStoreError::Store)
    }

    pub async fn delete_subject_key(
        &self,
        subject: &str,
        provider_id: &str,
    ) -> Result<bool, LlmKeyStoreError> {
        let handle = subject_handle_for(subject, provider_id)?;
        self.store
            .delete(&scope(), &handle)
            .await
            .map_err(LlmKeyStoreError::Store)
    }
}

fn scope() -> ResourceScope {
    ResourceScope::system()
}

fn handle_for(provider_id: &str) -> Result<SecretHandle, LlmKeyStoreError> {
    SecretHandle::new(format!("llm_provider_{provider_id}_api_key")).map_err(|source| {
        LlmKeyStoreError::InvalidProviderId {
            provider_id: provider_id.to_string(),
            reason: source.to_string(),
        }
    })
}

fn subject_handle_for(subject: &str, provider_id: &str) -> Result<SecretHandle, LlmKeyStoreError> {
    use sha2::{Digest, Sha256};

    let subject = subject.trim();
    let provider_id = provider_id.trim();
    if subject.is_empty() {
        return Err(LlmKeyStoreError::InvalidSubject);
    }
    if provider_id.is_empty() {
        return Err(LlmKeyStoreError::InvalidProviderId {
            provider_id: provider_id.to_string(),
            reason: "provider id is empty".to_string(),
        });
    }
    let digest = Sha256::digest(format!("{provider_id}\0{subject}").as_bytes());
    let digest_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    SecretHandle::new(format!("llm_subject_{digest_hex}_api_key")).map_err(|source| {
        LlmKeyStoreError::InvalidSubjectHandle {
            reason: source.to_string(),
        }
    })
}

#[async_trait::async_trait]
impl ironclaw_llm::SubjectKeyResolver for LlmKeyStore {
    async fn resolve_bearer_token(
        &self,
        subject: &str,
        provider_id: &str,
    ) -> Result<Option<secrecy::SecretString>, ironclaw_llm::SubjectKeyResolveError> {
        self.read_subject_key(subject, provider_id)
            .await
            .map(|material| {
                material.map(|value| {
                    secrecy::SecretString::from(
                        secrecy::ExposeSecret::expose_secret(&value).to_string(),
                    )
                })
            })
            .map_err(|_| ironclaw_llm::SubjectKeyResolveError::StoreUnavailable)
    }
}

/// Errors surfaced when storing or reading LLM key values.
#[derive(Debug, Error)]
pub enum LlmKeyStoreError {
    #[error("invalid provider id `{provider_id}` for secret handle: {reason}")]
    InvalidProviderId { provider_id: String, reason: String },
    #[error("LLM subject must not be empty")]
    InvalidSubject,
    #[error("invalid LLM subject secret handle: {reason}")]
    InvalidSubjectHandle { reason: String },
    #[error("secret store error: {0}")]
    Store(#[source] SecretStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_secrets::FilesystemSecretStore;

    fn store() -> LlmKeyStore {
        LlmKeyStore::new(Arc::new(FilesystemSecretStore::ephemeral()))
    }

    #[tokio::test]
    async fn put_then_read_round_trips() {
        let keys = store();
        assert!(!keys.exists("acme").await.expect("exists"));
        assert!(keys.read("acme").await.expect("read").is_none());
        assert!(
            keys.stored_provider_ids()
                .await
                .expect("stored provider ids")
                .is_empty()
        );

        keys.put("acme", SecretMaterial::from("sk-test-value"))
            .await
            .expect("put");

        assert!(keys.exists("acme").await.expect("exists"));
        assert_eq!(
            keys.stored_provider_ids()
                .await
                .expect("stored provider ids"),
            HashSet::from(["acme".to_string()])
        );
        let value = keys.read("acme").await.expect("read").expect("some");
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&value),
            "sk-test-value"
        );
    }

    #[tokio::test]
    async fn read_is_repeatable_across_reloads() {
        let keys = store();
        keys.put("acme", SecretMaterial::from("sk-test-value"))
            .await
            .expect("put");
        // Two reads in a row must both succeed (lease+consume must not destroy
        // the underlying secret).
        assert!(keys.read("acme").await.expect("read 1").is_some());
        assert!(keys.read("acme").await.expect("read 2").is_some());
    }

    #[tokio::test]
    async fn delete_removes_key() {
        let keys = store();
        keys.put("acme", SecretMaterial::from("v"))
            .await
            .expect("put");
        assert!(keys.delete("acme").await.expect("delete"));
        assert!(!keys.exists("acme").await.expect("exists"));
        assert!(!keys.delete("acme").await.expect("delete again"));
    }

    #[tokio::test]
    async fn stored_provider_ids_ignores_unrelated_secret_handles() {
        let store = Arc::new(FilesystemSecretStore::ephemeral());
        store
            .put(
                scope(),
                SecretHandle::new("not_an_llm_key").expect("handle"),
                SecretMaterial::from("unrelated"),
                None,
            )
            .await
            .expect("put unrelated");
        let keys = LlmKeyStore::new(store);
        keys.put("openai", SecretMaterial::from("sk-openai"))
            .await
            .expect("put openai");

        assert_eq!(
            keys.stored_provider_ids()
                .await
                .expect("stored provider ids"),
            HashSet::from(["openai".to_string()])
        );
    }

    #[tokio::test]
    async fn unknown_provider_id_is_rejected() {
        let keys = store();
        let err = keys
            .put("bad id!", SecretMaterial::from("v"))
            .await
            .expect_err("must reject");
        assert!(matches!(err, LlmKeyStoreError::InvalidProviderId { .. }));
    }

    #[tokio::test]
    async fn subject_keys_round_trip_and_stay_isolated() {
        let keys = store();
        keys.put_subject_key_plaintext("user-a", "anthropic", "key-a".to_string())
            .await
            .expect("put a");
        keys.put_subject_key_plaintext("user-b", "anthropic", "key-b".to_string())
            .await
            .expect("put b");

        let a = keys
            .read_subject_key("user-a", "anthropic")
            .await
            .expect("read a")
            .expect("a exists");
        let b = keys
            .read_subject_key("user-b", "anthropic")
            .await
            .expect("read b")
            .expect("b exists");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&a), "key-a");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&b), "key-b");
        assert!(
            keys.subject_key_exists("user-a", "anthropic")
                .await
                .unwrap()
        );

        assert!(
            keys.delete_subject_key("user-a", "anthropic")
                .await
                .unwrap()
        );
        assert!(
            !keys
                .subject_key_exists("user-a", "anthropic")
                .await
                .unwrap()
        );
        assert!(
            keys.subject_key_exists("user-b", "anthropic")
                .await
                .unwrap()
        );
    }

    #[test]
    fn subject_handle_does_not_expose_subject_or_provider() {
        let handle = subject_handle_for("user-secret-label", "anthropic").expect("handle");
        assert!(!handle.as_str().contains("user-secret-label"));
        assert!(!handle.as_str().contains("anthropic"));
        assert!(handle.as_str().starts_with("llm_subject_"));
    }

    #[test]
    fn subject_handle_rejects_blank_subject() {
        assert!(matches!(
            subject_handle_for("   ", "anthropic"),
            Err(LlmKeyStoreError::InvalidSubject)
        ));
    }
}
