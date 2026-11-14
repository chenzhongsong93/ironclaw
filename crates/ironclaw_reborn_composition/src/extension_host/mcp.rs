use std::sync::Arc;

use ironclaw_extensions::{
    ExtensionPackage, ExtensionRuntime, ManifestSource, SharedExtensionRegistry,
};
use ironclaw_host_api::{
    CapabilityId, ExtensionId, NetworkMethod, NetworkPolicy, NetworkScheme, NetworkTargetPattern,
    RuntimeCredentialInjection, RuntimeCredentialSource, RuntimeHttpEgress, TenantId,
};
use ironclaw_mcp::{
    McpHostHttpClient, McpHostHttpEgressPlan, McpHostHttpEgressPlanRequest,
    McpHostHttpEgressPlanner, McpRuntime, McpRuntimeConfig, McpRuntimeHttpAdapter,
    McpTrustedContextSigner,
};

pub(crate) const MCP_RESPONSE_BODY_LIMIT: u64 = 2 * 1024 * 1024;
const MCP_NETWORK_EGRESS_LIMIT: u64 = 2 * 1024 * 1024;
#[cfg(test)]
const MCP_TIMEOUT_MS: u32 = 60_000;

pub(crate) fn hosted_http_mcp_runtime(
    registry: Arc<SharedExtensionRegistry>,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
    allow_local_mcp: bool,
    allow_hosted_local_mcp: bool,
) -> McpRuntime<
    McpHostHttpClient<McpRuntimeHttpAdapter<Arc<dyn RuntimeHttpEgress>>, RegistryMcpEgressPlanner>,
> {
    let planner = if allow_local_mcp {
        RegistryMcpEgressPlanner::new_with_local_mcp(registry)
    } else if allow_hosted_local_mcp {
        RegistryMcpEgressPlanner::new_with_hosted_local_mcp(registry)
    } else {
        RegistryMcpEgressPlanner::new(registry)
    };
    let trusted_context_signer = McpTrustedContextSigner::from_env(
        ExtensionId::new("tianquan-graph").expect("static TianQuan provider ID is valid"),
    );
    let client = McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress), planner)
        .with_trusted_context_signer(trusted_context_signer);
    McpRuntime::new(McpRuntimeConfig::default(), client)
}

pub(crate) fn hosted_http_mcp_runtime_with_store(
    registry: Arc<SharedExtensionRegistry>,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
    operator_admission: Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
    allow_local_mcp: bool,
    allow_hosted_local_mcp: bool,
) -> Result<
    McpRuntime<
        McpHostHttpClient<
            McpRuntimeHttpAdapter<Arc<dyn RuntimeHttpEgress>>,
            RegistryMcpEgressPlanner,
        >,
    >,
    ironclaw_product_workflow::ProductWorkflowError,
> {
    let mut planner = if allow_local_mcp {
        RegistryMcpEgressPlanner::new_with_local_mcp(registry)
    } else if allow_hosted_local_mcp {
        RegistryMcpEgressPlanner::new_with_hosted_local_mcp(registry)
    } else {
        RegistryMcpEgressPlanner::new(registry)
    };
    planner.late_admissions = Arc::clone(&operator_admission);
    // The platform remains domain-neutral: TianQuan's thin exporter binds this
    // signer policy to its provider ID. Other MCP providers never receive these headers.
    let trusted_context_signer = McpTrustedContextSigner::from_env(
        ExtensionId::new("tianquan-graph").expect("static TianQuan provider ID is valid"),
    );
    operator_admission.set_actual_signer(&trusted_context_signer)?;
    let client = McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress), planner)
        .with_trusted_context_signer(trusted_context_signer);
    Ok(McpRuntime::new(McpRuntimeConfig::default(), client))
}

fn configured_mcp_timeout() -> Option<u32> {
    ironclaw_reborn_config::HostHttpTimeouts::from_process_env()
        .ok()
        .map(|config| config.mcp_timeout_ms)
}

// Only the existing Root startup discovery constructor can select this mode.
// It contains no actor authority and cannot plan tools/call or fall back.
#[derive(Debug, Clone)]
struct RootCatalogDiscovery {
    provider: ExtensionId,
    public_url: String,
}

fn root_catalog_url(internal: &str) -> Option<String> {
    let mut url = url::Url::parse(internal).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.path() != "/mcp/internal"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != internal
    {
        return None;
    }
    url.set_path("/mcp");
    Some(url.to_string())
}

fn root_catalog_request(request: &McpHostHttpEgressPlanRequest<'_>) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CatalogRpc {
        jsonrpc: String,
        id: Option<u64>,
        method: String,
        params: Option<serde_json::Value>,
    }
    if request.method != NetworkMethod::Post
        || request.transport != "http"
        || request.body.len() > 64 * 1024
        || request.headers.len() != 2
        || !request
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type") && v == "application/json")
        || !request.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("Accept") && v == "application/json, text/event-stream"
        })
    {
        return false;
    }
    let Ok(rpc) = serde_json::from_slice::<CatalogRpc>(request.body) else {
        return false;
    };
    if rpc.jsonrpc != "2.0" {
        return false;
    }
    match rpc.method.as_str() {
        "initialize" => rpc.id.is_some() && rpc.params.is_some_and(|p| p.is_object()),
        "notifications/initialized" => rpc.id.is_none() && rpc.params.is_none(),
        "tools/list" => rpc.id.is_some() && rpc.params.is_none(),
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegistryMcpEgressPlanner {
    registry: Arc<SharedExtensionRegistry>,
    /// Local-dev override: when true, admits InstalledLocal MCP packages,
    /// http scheme, and private/loopback egress targets. Production builds
    /// must keep this false (fail-closed against internal metadata services).
    allow_local_mcp: bool,
    allow_hosted_local_mcp: bool,
    timeout_ms: Option<u32>,
    operator_bindings: Vec<crate::operator_host_bundle::OperatorOwnedMcpBinding>,
    root_catalog: Option<RootCatalogDiscovery>,
    late_admissions: Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
    // Runtime sessions/unknown host calls retain the original trusted config/asset files too.
    _operator_bundles: Vec<Arc<crate::RebornHostExtensionBundle>>,
}

impl RegistryMcpEgressPlanner {
    pub(crate) fn new(registry: Arc<SharedExtensionRegistry>) -> Self {
        Self {
            registry,
            allow_local_mcp: false,
            allow_hosted_local_mcp: false,
            timeout_ms: configured_mcp_timeout(),
            operator_bindings: Vec::new(),
            root_catalog: None,
            late_admissions: Arc::new(
                crate::operator_late_admission::OperatorLateAdmissionStore::default(),
            ),
            _operator_bundles: Vec::new(),
        }
    }

    /// Local-dev constructor: admits InstalledLocal + http + private/loopback
    /// MCP egress. Caller must gate on `profile.is_local_dev()` to keep
    /// production fail-closed.
    pub(crate) fn new_with_local_mcp(registry: Arc<SharedExtensionRegistry>) -> Self {
        Self {
            registry,
            allow_local_mcp: true,
            allow_hosted_local_mcp: false,
            timeout_ms: configured_mcp_timeout(),
            operator_bindings: Vec::new(),
            root_catalog: None,
            late_admissions: Arc::new(
                crate::operator_late_admission::OperatorLateAdmissionStore::default(),
            ),
            _operator_bundles: Vec::new(),
        }
    }
    /// Hosted-single-tenant constructor: admits host-bundled HTTP and private
    /// MCP endpoints for a self-hosted local API, but never InstalledLocal
    /// packages. The generic Production profile remains on `new` (fail-closed).
    pub(crate) fn new_with_hosted_local_mcp(registry: Arc<SharedExtensionRegistry>) -> Self {
        Self {
            registry,
            allow_local_mcp: false,
            allow_hosted_local_mcp: true,
            timeout_ms: configured_mcp_timeout(),
            operator_bindings: Vec::new(),
            root_catalog: None,
            late_admissions: Arc::new(
                crate::operator_late_admission::OperatorLateAdmissionStore::default(),
            ),
            _operator_bundles: Vec::new(),
        }
    }

    /// Root startup calls only after the validated bundle and actual signer are configured.
    /// No RuntimeInput/caller/model DTO receives an OperatorOwnedMcpBinding constructor.
    pub(crate) fn new_with_operator_mcp(
        registry: Arc<SharedExtensionRegistry>,
        bundles: &[Arc<crate::RebornHostExtensionBundle>],
        signer: &McpTrustedContextSigner,
        actual_validated_runtime_tenant: &TenantId,
    ) -> Result<Self, ironclaw_product_workflow::ProductWorkflowError> {
        crate::operator_host_bundle::validate_operator_bundle_refs(bundles)?;
        crate::operator_host_bundle::validate_operator_runtime_tenant(
            bundles,
            actual_validated_runtime_tenant,
        )?;
        let mut planner = Self::new(registry);
        planner._operator_bundles = bundles.to_vec();
        for bundle in bundles {
            for binding in bundle.bindings() {
                if !signer.is_configured()
                    || signer.provider_id() != binding.provider()
                    || signer.audience() != Some(binding.audience())
                {
                    return Err(
                        ironclaw_product_workflow::ProductWorkflowError::InvalidBindingRequest {
                            reason: "hostBundleActualSignerMismatch".into(),
                        },
                    );
                }
                planner.operator_bindings.push(binding);
            }
        }
        Ok(planner)
    }
    #[cfg(test)]
    pub(crate) fn with_late_mechanism_store(
        registry: Arc<SharedExtensionRegistry>,
        store: Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
    ) -> Self {
        let mut planner = Self::new(registry);
        planner.late_admissions = store;
        planner.timeout_ms = Some(1000);
        planner
    }
    pub(crate) fn for_root_discovery(
        &mut self,
        binding: crate::operator_host_bundle::OperatorOwnedMcpBinding,
        store: Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
    ) -> Option<String> {
        if self.root_catalog.is_some() {
            return None;
        }
        let public_url = root_catalog_url(binding.audience())?;
        self.root_catalog = Some(RootCatalogDiscovery {
            provider: binding.provider().clone(),
            public_url: public_url.clone(),
        });
        self.late_admissions = store;
        self.operator_bindings.push(binding);
        Some(public_url)
    }
    fn operator_plan(
        &self,
        request: &McpHostHttpEgressPlanRequest<'_>,
    ) -> Option<McpHostHttpEgressPlan> {
        let late = self.late_admissions.binding(request.provider);
        let binding = late.as_ref().or_else(|| {
            self.operator_bindings.iter().find(|b| {
                b.provider() == request.provider && self.late_admissions.old_binding_valid(b)
            })
        })?;
        if binding.tenant() != &request.scope.tenant_id {
            return None;
        }
        let snapshot = self.registry.snapshot();
        let package = snapshot.get_extension(request.provider)?;
        let capability = snapshot.get_capability(request.capability_id)?;
        if &capability.provider != request.provider || request.transport != "http" {
            return None;
        }
        let url = if let Some(catalog) = &self.root_catalog {
            if &catalog.provider != request.provider
                || request.url != catalog.public_url
                || !root_catalog_request(request)
            {
                return None;
            }
            // Recheck the original signed package/binding before selecting only
            // the public catalog URL; no manifest/digest/tenant replacement.
            binding.endpoint_for(package, binding.audience())?;
            catalog.public_url.as_str()
        } else {
            // Ordinary actor traffic keeps the original exact signed endpoint.
            binding.endpoint_for(package, request.url)?
        };
        let parsed = HostedMcpEndpoint::parse(url, true)?;
        if self.timeout_ms.is_none() {
            return None;
        }
        let mut policy = hosted_mcp_network_policy(&parsed, true);
        policy.deny_redirects = true; // shared typed deny-only constraint, transport candidate required.
        Some(McpHostHttpEgressPlan {
            network_policy: policy,
            credential_injections: self.credential_injections(
                request.provider,
                request.capability_id,
                &parsed,
            ),
            response_body_limit: Some(MCP_RESPONSE_BODY_LIMIT),
            timeout_ms: self.timeout_ms,
        })
    }

    fn credential_injections(
        &self,
        provider: &ExtensionId,
        capability_id: &CapabilityId,
        endpoint: &HostedMcpEndpoint,
    ) -> Vec<RuntimeCredentialInjection> {
        self.registry
            .snapshot()
            .get_capability(capability_id)
            .filter(|descriptor| &descriptor.provider == provider)
            .map(|descriptor| {
                descriptor
                    .runtime_credentials
                    .iter()
                    .filter(|credential| endpoint.allows_target(&credential.audience))
                    .map(|credential| RuntimeCredentialInjection {
                        handle: credential.handle.clone(),
                        source: RuntimeCredentialSource::StagedObligation {
                            capability_id: capability_id.clone(),
                        },
                        target: credential.target.clone(),
                        required: credential.required,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn provider_endpoint(&self, provider: &ExtensionId) -> Option<HostedMcpEndpoint> {
        let registry = self.registry.snapshot();
        registry.get_extension(provider).and_then(|pkg| {
            hosted_http_mcp_endpoint(pkg, self.allow_local_mcp, self.allow_hosted_local_mcp)
        })
    }
}

impl McpHostHttpEgressPlanner for RegistryMcpEgressPlanner {
    fn plan(&self, request: McpHostHttpEgressPlanRequest<'_>) -> McpHostHttpEgressPlan {
        if self.root_catalog.is_some() {
            return self.operator_plan(&request).unwrap_or_default();
        }
        if let Some(plan) = self.operator_plan(&request) {
            return plan;
        }
        // A configured operator provider can never silently fall back to another endpoint/source.
        if self.late_admissions.requires_admission(request.provider)
            || self
                .operator_bindings
                .iter()
                .any(|b| b.provider() == request.provider)
        {
            return McpHostHttpEgressPlan::default();
        }
        let Some(endpoint) = self.provider_endpoint(request.provider) else {
            return McpHostHttpEgressPlan::default();
        };
        if !hosted_mcp_url_allowed(request.url, &endpoint) {
            return McpHostHttpEgressPlan::default();
        }
        if self.timeout_ms.is_none() {
            return McpHostHttpEgressPlan::default();
        }
        let credential_injections =
            self.credential_injections(request.provider, request.capability_id, &endpoint);
        McpHostHttpEgressPlan {
            // Credential-free hosted MCP providers are valid: the manifest may
            // expose a public/unauthenticated server, and host network policy
            // is still enforced below. Missing credentials for providers that
            // should authenticate are a manifest/catalog validation concern,
            // not an egress-planning reason to block the HTTP request.
            // Must match the bundled manifest's network policy
            // (deny_private_ip_ranges: true) or the dispatcher rejects the
            // request. Under local-dev (`allow_local_mcp`), private/loopback
            // IPs and http scheme are admitted so container-internal MCP
            // servers (e.g. localhost:3002) can be reached.
            network_policy: hosted_mcp_network_policy(
                &endpoint,
                self.allow_local_mcp || self.allow_hosted_local_mcp,
            ),
            credential_injections,
            response_body_limit: Some(MCP_RESPONSE_BODY_LIMIT),
            timeout_ms: self.timeout_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedMcpEndpoint {
    host_pattern: String,
    port: Option<u16>,
    path: String,
    /// URL scheme retained from the manifest runtime url. `Https` is the
    /// production default; `Http` is only accepted under local-dev profile
    /// (see `hosted_http_mcp_endpoint` `allow_local` gate).
    scheme: NetworkScheme,
}

impl HostedMcpEndpoint {
    fn parse(url: &str, allow_http: bool) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        let scheme = match parsed.scheme() {
            "https" => NetworkScheme::Https,
            "http" if allow_http => NetworkScheme::Http,
            _ => return None,
        };
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return None;
        }
        Some(Self {
            host_pattern: parsed.host_str()?.to_ascii_lowercase(),
            port: parsed.port(),
            path: normalize_mcp_path(parsed.path()),
            scheme,
        })
    }

    fn allows_target(&self, target: &NetworkTargetPattern) -> bool {
        target.scheme == Some(self.scheme)
            && target.host_pattern.eq_ignore_ascii_case(&self.host_pattern)
            && target.port == self.port
    }

    fn matches_url(&self, url: &str) -> bool {
        // Re-parse the request url tolerating the endpoint's own scheme so
        // an http endpoint admits http requests (local-dev only).
        let allow_http = self.scheme == NetworkScheme::Http;
        Self::parse(url, allow_http).is_some_and(|request_endpoint| {
            request_endpoint == *self
                // Trusted host signers may move a manifest's legacy MCP path
                // to a same-origin protected path (TianQuan: /mcp →
                // /mcp/internal). Scheme, host, port, and provider policy
                // remain identical; only the server-owned path changes.
                || (self.path == "/mcp" && request_endpoint.path == "/mcp/internal")
        })
    }
}

pub(crate) fn hosted_http_mcp_endpoint(
    package: &ExtensionPackage,
    allow_local: bool,
    allow_hosted_local_http: bool,
) -> Option<HostedMcpEndpoint> {
    // Production: only HostBundled extensions may register hosted HTTP MCP
    // endpoints. Local-dev (`allow_local`) additionally admits InstalledLocal
    // packages so container-internal MCP servers (e.g. TianQuan api on
    // localhost:3002) can be reached without baking a first-party bundle.
    if package.manifest.source != ManifestSource::HostBundled
        && !(allow_local && package.manifest.source == ManifestSource::InstalledLocal)
    {
        return None;
    }
    let ExtensionRuntime::Mcp {
        transport,
        command: None,
        args,
        url: Some(url),
    } = &package.manifest.runtime
    else {
        return None;
    };
    if transport != "http" || !args.is_empty() {
        return None;
    }
    HostedMcpEndpoint::parse(url, allow_local || allow_hosted_local_http)
}

/// Returns `true` when `url` scheme/host/path match the endpoint. Under
/// local-dev the endpoint may carry an `http` scheme (see `HostedMcpEndpoint::scheme`).
fn hosted_mcp_url_allowed(url: &str, endpoint: &HostedMcpEndpoint) -> bool {
    endpoint.matches_url(url)
}

fn normalize_mcp_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn hosted_mcp_network_policy(
    endpoint: &HostedMcpEndpoint,
    allow_private_ip: bool,
) -> NetworkPolicy {
    NetworkPolicy {
        allowed_targets: vec![NetworkTargetPattern {
            scheme: Some(endpoint.scheme),
            host_pattern: endpoint.host_pattern.clone(),
            port: endpoint.port,
        }],
        // Production: deny private/loopback IPs (fail-closed against cloud
        // metadata services etc.). Local-dev (`allow_private_ip`): admit them
        // so container-internal MCP servers (localhost / docker DNS) resolve.
        deny_private_ip_ranges: !allow_private_ip,
        deny_redirects: false,
        max_egress_bytes: Some(MCP_NETWORK_EGRESS_LIMIT),
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_extensions::{ExtensionManifest, ExtensionPackage, ManifestSource};
    use ironclaw_host_api::{
        CapabilityId, CapabilityProfileSchemaRef, ExtensionId, InvocationId, NetworkMethod,
        NetworkScheme, NetworkTargetPattern, PermissionMode, ProjectId, ResourceScope,
        RuntimeCredentialAccountProviderId, RuntimeCredentialRequirementSource,
        RuntimeCredentialTarget, SecretHandle, TenantId, TrustClass, UserId, VirtualPath,
    };

    use super::*;

    #[test]
    fn actual_planner_denies_foreign_request_tenant_without_legacy_fallback() {
        let endpoint = "http://127.0.0.1:3002/mcp/internal";
        let registry = Arc::new(SharedExtensionRegistry::new(
            real_digest_registry_for_tenant_fixture(endpoint),
        ));
        let provider = ExtensionId::new("notion").unwrap();
        let capability_id = CapabilityId::new("notion.notion-search").unwrap();
        let mut planner = RegistryMcpEgressPlanner::new(registry.clone());
        planner.timeout_ms = Some(1000);
        let snapshot = registry.snapshot();
        let package = snapshot.get_extension(&provider).unwrap();
        let mut scope = sample_scope();
        planner
            .late_admissions
            .bind_runtime("11111111-1111-4111-8111-111111111111", Arc::new(|| true))
            .unwrap();
        planner.operator_bindings.push(
            crate::operator_host_bundle::OperatorOwnedMcpBinding::tenant_test_fixture(
                package,
                scope.tenant_id.clone(),
                endpoint,
            ),
        );
        let positive = planner.plan(sample_plan_request(
            &provider,
            &capability_id,
            endpoint,
            &scope,
        ));
        assert!(positive.network_policy.deny_redirects);
        scope.tenant_id = TenantId::new("foreign-tenant").unwrap();
        let denied = planner.plan(sample_plan_request(
            &provider,
            &capability_id,
            endpoint,
            &scope,
        ));
        assert!(denied.network_policy.allowed_targets.is_empty());
    }

    #[test]
    fn actual_planner_selected_signer_public_package_is_denied_before_reserve() {
        // Shared registry already holds the public package; no pending entry exists yet.
        // This is a source mechanism fixture, not a qualified operator/runtime receipt.
        let endpoint = "https://selected.example.com/mcp";
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_provider(
            "selected",
            endpoint,
            "selected.search",
            "fixture_token",
        )));
        let mut planner = RegistryMcpEgressPlanner::new(registry.clone());
        planner.timeout_ms = Some(1000);
        let provider = ExtensionId::new("selected").unwrap();
        let capability = CapabilityId::new("selected.search").unwrap();
        let signer = McpTrustedContextSigner::new(
            provider.clone(),
            b"unit-fixture-not-a-qualified-operator-proof".to_vec(),
            "https://selected.example.com/mcp/internal".into(),
        )
        .unwrap();
        assert!(signer.is_configured());
        planner.late_admissions.set_actual_signer(&signer).unwrap();
        assert!(registry.snapshot().get_extension(&provider).is_some());
        assert!(!planner.late_admissions.contains(&provider));
        let scope = sample_scope();
        let plan = planner.plan(sample_plan_request(
            &provider,
            &capability,
            endpoint,
            &scope,
        ));
        assert!(plan.network_policy.allowed_targets.is_empty());
        assert!(plan.credential_injections.is_empty());
    }

    #[test]
    fn actual_planner_nonselected_public_http_https_keep_original_policy() {
        // Production already supports HTTPS; HTTP keeps the explicit pre-existing Local constructor.
        // No production allow_local widening is introduced by the admission guard.
        for (local, endpoint, scheme) in [
            (
                false,
                "https://ordinary.example.com/mcp",
                NetworkScheme::Https,
            ),
            (
                true,
                "http://ordinary.example.com:8080/mcp",
                NetworkScheme::Http,
            ),
        ] {
            let registry = Arc::new(SharedExtensionRegistry::new(registry_with_provider(
                "ordinary",
                endpoint,
                "ordinary.search",
                "fixture_token",
            )));
            let mut planner = if local {
                RegistryMcpEgressPlanner::new_with_local_mcp(registry)
            } else {
                RegistryMcpEgressPlanner::new(registry)
            };
            planner.timeout_ms = Some(1000);
            let signer = McpTrustedContextSigner::new(
                ExtensionId::new("selected").unwrap(),
                b"unit-fixture-not-a-qualified-operator-proof".to_vec(),
                "https://selected.example.com/mcp/internal".into(),
            )
            .unwrap();
            planner.late_admissions.set_actual_signer(&signer).unwrap();
            let provider = ExtensionId::new("ordinary").unwrap();
            let capability = CapabilityId::new("ordinary.search").unwrap();
            let scope = sample_scope();
            assert!(!planner.late_admissions.requires_admission(&provider));
            let plan = planner.plan(sample_plan_request(
                &provider,
                &capability,
                endpoint,
                &scope,
            ));
            assert_eq!(plan.network_policy.allowed_targets.len(), 1);
            assert_eq!(
                plan.network_policy.allowed_targets[0].host_pattern,
                "ordinary.example.com"
            );
            assert_eq!(plan.network_policy.allowed_targets[0].scheme, Some(scheme));
            assert_eq!(plan.network_policy.deny_private_ip_ranges, !local);
            assert!(!plan.network_policy.deny_redirects);
        }
    }

    fn root_catalog_fixture() -> (
        RegistryMcpEgressPlanner,
        ResourceScope,
        ExtensionId,
        CapabilityId,
        String,
    ) {
        let internal = "http://127.0.0.1:3002/mcp/internal";
        let registry = Arc::new(SharedExtensionRegistry::new(
            real_digest_registry_for_tenant_fixture(internal),
        ));
        let provider = ExtensionId::new("notion").unwrap();
        let capability = CapabilityId::new("notion.notion-search").unwrap();
        let scope = sample_scope();
        let package = registry
            .snapshot()
            .get_extension(&provider)
            .unwrap()
            .clone();
        let binding = crate::operator_host_bundle::OperatorOwnedMcpBinding::tenant_test_fixture(
            &package,
            scope.tenant_id.clone(),
            internal,
        );
        let store = Arc::new(crate::operator_late_admission::OperatorLateAdmissionStore::default());
        store
            .bind_runtime("11111111-1111-4111-8111-111111111111", Arc::new(|| true))
            .unwrap();
        let mut planner = RegistryMcpEgressPlanner::new(registry);
        planner.timeout_ms = Some(1000);
        let public = planner.for_root_discovery(binding, store).unwrap();
        (planner, scope, provider, capability, public)
    }

    fn catalog_headers() -> Vec<(String, String)> {
        vec![
            ("Content-Type".into(), "application/json".into()),
            (
                "Accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ]
    }

    #[test]
    fn root_catalog_actual_planner_only_three_public_methods() {
        let (planner, scope, provider, capability, public) = root_catalog_fixture();
        let headers = catalog_headers();
        for value in [
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        ] {
            let body = serde_json::to_vec(&value).unwrap();
            let mut request = sample_plan_request(&provider, &capability, &public, &scope);
            request.headers = &headers;
            request.body = &body;
            let plan = planner.plan(request);
            assert_eq!(plan.network_policy.allowed_targets.len(), 1);
            assert!(plan.network_policy.deny_redirects);
            assert_eq!(plan.timeout_ms, Some(1000));
            assert_eq!(plan.response_body_limit, Some(MCP_RESPONSE_BODY_LIMIT));
        }
    }

    #[test]
    fn root_catalog_actual_planner_denies_tools_batch_and_foreign_without_fallback() {
        let (planner, mut scope, provider, capability, public) = root_catalog_fixture();
        let headers = catalog_headers();
        for body in [
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#.as_slice(),
            br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","method":"tools/list"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":true,"method":"tools/list"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","arguments":{}}"#.as_slice(),
        ] {
            let mut r = sample_plan_request(&provider, &capability, &public, &scope);
            r.headers = &headers;
            r.body = body;
            assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
        }
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        for url in [
            "http://127.0.0.1:3002/mcp/internal",
            "http://127.0.0.1:3003/mcp",
            "http://127.0.0.1:3002/mcp?x=1",
            "https://foreign.example/mcp",
        ] {
            let mut r = sample_plan_request(&provider, &capability, url, &scope);
            r.headers = &headers;
            r.body = body;
            assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
        }
        let mut r = sample_plan_request(&provider, &capability, &public, &scope);
        r.headers = &headers;
        r.body = body;
        r.method = NetworkMethod::Get;
        assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
        let mut r = sample_plan_request(&provider, &capability, &public, &scope);
        r.body = body;
        assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
        scope.tenant_id = TenantId::new("foreign").unwrap();
        let mut r = sample_plan_request(&provider, &capability, &public, &scope);
        r.headers = &headers;
        r.body = body;
        assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
        let foreign = ExtensionId::new("foreign").unwrap();
        let mut r = sample_plan_request(&foreign, &capability, &public, &scope);
        r.headers = &headers;
        r.body = body;
        assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
    }

    #[test]
    fn ordinary_operator_keeps_exact_internal_and_root_rechecks_package() {
        let (mut planner, scope, provider, capability, public) = root_catalog_fixture();
        planner.root_catalog = None;
        assert!(
            !planner
                .plan(sample_plan_request(
                    &provider,
                    &capability,
                    "http://127.0.0.1:3002/mcp/internal",
                    &scope
                ))
                .network_policy
                .allowed_targets
                .is_empty()
        );
        assert!(
            planner
                .plan(sample_plan_request(&provider, &capability, &public, &scope))
                .network_policy
                .allowed_targets
                .is_empty()
        );
        let (mut planner, scope, provider, capability, public) = root_catalog_fixture();
        planner.registry = Arc::new(SharedExtensionRegistry::new(
            real_digest_registry_for_tenant_fixture("http://127.0.0.1:3003/mcp/internal"),
        ));
        let headers = catalog_headers();
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let mut r = sample_plan_request(&provider, &capability, &public, &scope);
        r.headers = &headers;
        r.body = body;
        assert!(planner.plan(r).network_policy.allowed_targets.is_empty());
    }

    #[derive(Default)]
    struct CatalogOnlyEgress {
        calls: std::sync::Mutex<Vec<String>>,
        tools: Vec<serde_json::Value>,
    }
    #[async_trait::async_trait]
    impl RuntimeHttpEgress for CatalogOnlyEgress {
        async fn execute(
            &self,
            request: ironclaw_host_api::RuntimeHttpEgressRequest,
        ) -> Result<
            ironclaw_host_api::RuntimeHttpEgressResponse,
            ironclaw_host_api::RuntimeHttpEgressError,
        > {
            assert_eq!(request.method, NetworkMethod::Post);
            assert_eq!(request.url, "http://127.0.0.1:3002/mcp");
            assert_eq!(request.network_policy.allowed_targets.len(), 1);
            assert!(request.network_policy.deny_redirects);
            assert!(request.headers.iter().all(|(k, _)| {
                !k.eq_ignore_ascii_case("x-tianquan-execution-context")
                    && !k.eq_ignore_ascii_case("x-tianquan-execution-signature")
            }));
            let value: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method = value["method"].as_str().unwrap();
            self.calls.lock().unwrap().push(method.to_owned());
            let result = match method {
                "initialize" => {
                    serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"catalog-fixture","version":"1"}})
                }
                "notifications/initialized" => serde_json::json!({}),
                "tools/list" => serde_json::json!({"tools":self.tools}),
                _ => panic!("Root discovery attempted noncatalog method"),
            };
            let body = serde_json::to_vec(
                &serde_json::json!({"jsonrpc":"2.0","id":value["id"],"result":result}),
            )
            .unwrap();
            Ok(ironclaw_host_api::RuntimeHttpEgressResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                request_bytes: u64::try_from(request.body.len()).unwrap(),
                response_bytes: u64::try_from(body.len()).unwrap(),
                body,
                saved_body: None,
                redaction_applied: false,
            })
        }
    }

    #[tokio::test]
    async fn root_discovery_actual_caller_uses_public_catalog_without_actor_headers() {
        let internal = "http://127.0.0.1:3002/mcp/internal";
        let provider = ExtensionId::new("notion").unwrap();
        let scope = sample_scope();
        let registry = real_digest_registry_for_tenant_fixture(internal);
        let package = registry.get_extension(&provider).unwrap().clone();
        let available = crate::extension_host::available_extensions::AvailableExtensionPackage {
            package_ref: ironclaw_product_workflow::LifecyclePackageRef::new(
                ironclaw_product_workflow::LifecyclePackageKind::Extension,
                "notion",
            )
            .unwrap(),
            package: package.clone(),
            manifest_toml: include_str!(
                "../../../ironclaw_first_party_extensions/assets/notion-mcp/manifest.toml"
            )
            .replace("https://mcp.notion.com/mcp", internal),
            source: ManifestSource::HostBundled,
            cleanup_requirements: Vec::new(),
            surface_kinds: Vec::new(),
            assets: Vec::new(),
        };
        let instance = "11111111-1111-4111-8111-111111111111";
        let bundle = Arc::new(crate::RebornHostExtensionBundle::late_mechanism_fixture(
            available,
            scope.tenant_id.clone(),
            instance,
        ));
        let store = Arc::new(crate::operator_late_admission::OperatorLateAdmissionStore::default());
        let signer = McpTrustedContextSigner::new(
            provider,
            b"unit-fixture-not-a-qualified-operator-proof".to_vec(),
            internal.into(),
        )
        .unwrap();
        store.set_actual_signer(&signer).unwrap();
        store.bind_runtime(instance, Arc::new(|| true)).unwrap();
        store
            .reserve(bundle, &scope.tenant_id, instance, Arc::new(|| true))
            .unwrap();
        let prefix = format!("{}.", package.id.as_str());
        let tools=package.manifest.capabilities.iter().map(|cap|serde_json::json!({"name":cap.id.as_str().strip_prefix(&prefix).unwrap(),"description":"Complete public fixture","inputSchema":{"type":"object","properties":{}}})).collect();
        let egress = Arc::new(CatalogOnlyEgress {
            calls: std::sync::Mutex::new(Vec::new()),
            tools,
        });
        // Ordinary HTTP remains denied before any host request; the same exact
        // package is accepted only with this validated reserved Root store.
        assert!(
            crate::extension_host::mcp_discovery::discover_hosted_mcp_package_with_admission(
                &package,
                scope.clone(),
                egress.clone(),
                None
            )
            .await
            .is_err()
        );
        assert!(egress.calls.lock().unwrap().is_empty());
        let result =
            crate::extension_host::mcp_discovery::discover_hosted_mcp_package_with_admission(
                &package,
                scope.clone(),
                egress.clone(),
                Some(store.clone()),
            )
            .await;
        let discovered = result.expect("actual Root HTTP catalog caller must succeed");
        let known = package
            .manifest
            .capabilities
            .iter()
            .find(|c| c.id.as_str() == "notion.notion-search")
            .unwrap();
        let actual = discovered
            .manifest
            .capabilities
            .iter()
            .find(|c| c.id == known.id)
            .unwrap();
        assert_eq!(actual, known); // Actual parameters differ only in inline descriptor schema.
        assert_eq!(
            discovered.manifest.capabilities,
            package.manifest.capabilities
        ); // No declared tool was silently dropped.
        assert_eq!(
            discovered.capabilities[0].parameters_schema,
            serde_json::json!({"type":"object","properties":{}})
        );
        assert_eq!(
            *egress.calls.lock().unwrap(),
            vec!["initialize", "notifications/initialized", "tools/list"]
        );
        assert!(
            matches!(&package.manifest.runtime,ExtensionRuntime::Mcp{url:Some(url),..}if url==internal)
        );
        let mut foreign = scope;
        foreign.tenant_id = TenantId::new("foreign").unwrap();
        let before = egress.calls.lock().unwrap().len();
        assert!(
            crate::extension_host::mcp_discovery::discover_hosted_mcp_package_with_admission(
                &package,
                foreign,
                egress.clone(),
                Some(store)
            )
            .await
            .is_err()
        );
        assert_eq!(egress.calls.lock().unwrap().len(), before);
    }

    const NOTION_MCP_HOST: &str = "mcp.notion.com";
    const NOTION_MCP_URL: &str = "https://mcp.notion.com/mcp";

    // ── credential projection ──────────────────────────────────────────────

    #[test]
    fn mcp_planner_projects_manifest_runtime_credentials_to_staged_injections() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("notion").unwrap();
        let capability_id = CapabilityId::new("notion.notion-search").unwrap();
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();

        let injections = planner.credential_injections(&provider, &capability_id, &endpoint);

        assert_eq!(injections.len(), 1);
        assert_eq!(
            injections[0].handle,
            SecretHandle::new("mcp_notion_access_token").unwrap()
        );
        assert_eq!(
            injections[0].source,
            RuntimeCredentialSource::StagedObligation { capability_id }
        );
        assert!(matches!(
            injections[0].target,
            RuntimeCredentialTarget::Header { .. }
        ));
    }

    // ── provider scoping ───────────────────────────────────────────────────

    #[test]
    fn planner_denies_unknown_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("not-notion").unwrap();
        let cap = CapabilityId::new("notion.notion-search").unwrap();

        let scope = sample_scope();
        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "https://mcp.notion.com/mcp",
            &scope,
        ));

        assert!(plan.credential_injections.is_empty());
        assert!(plan.network_policy.allowed_targets.is_empty());
    }

    #[test]
    fn planner_accepts_any_host_bundled_http_mcp_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_provider(
            "fixture",
            "https://fixture.example.com/mcp",
            "fixture.search",
            "fixture_token",
        )));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("fixture").unwrap();
        let cap = CapabilityId::new("fixture.search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "https://fixture.example.com/mcp",
            &scope,
        ));

        assert_eq!(plan.credential_injections.len(), 1);
        assert_eq!(
            plan.network_policy.allowed_targets[0].host_pattern,
            "fixture.example.com"
        );
    }

    /// Local-dev override admits InstalledLocal + http + loopback MCP servers
    /// (e.g. TianQuan api on localhost:3002). Production planner (`new`)
    /// must reject the same package — verified by `planner_denies_http_scheme`.
    #[test]
    fn planner_local_dev_admits_installed_local_http_loopback() {
        let registry = Arc::new(SharedExtensionRegistry::new(
            registry_with_local_mcp_provider("tianquan-graph", "http://localhost:3002/mcp"),
        ));
        let planner = RegistryMcpEgressPlanner::new_with_local_mcp(registry);
        let provider = ExtensionId::new("tianquan-graph").unwrap();
        let cap = CapabilityId::new("tianquan-graph.tools").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "http://localhost:3002/mcp",
            &scope,
        ));

        // InstalledLocal + http admitted under local-dev: egress plan non-empty.
        assert!(!plan.network_policy.allowed_targets.is_empty());
        assert_eq!(
            plan.network_policy.allowed_targets[0].host_pattern,
            "localhost"
        );
        // Private/loopback IPs admitted (deny_private_ip_ranges flipped off).
        assert!(!plan.network_policy.deny_private_ip_ranges);
        // http scheme retained (not forced to https).
        assert_eq!(
            plan.network_policy.allowed_targets[0].scheme,
            Some(NetworkScheme::Http)
        );
    }

    #[test]
    fn planner_hosted_single_tenant_admits_host_bundled_private_http() {
        let endpoint = "http://127.0.0.1:10516/mcp/internal";
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_provider(
            "hosted-local-fixture",
            endpoint,
            "hosted-local-fixture.search",
            "fixture_token",
        )));
        let planner = RegistryMcpEgressPlanner::new_with_hosted_local_mcp(registry);
        let provider = ExtensionId::new("hosted-local-fixture").unwrap();
        let cap = CapabilityId::new("hosted-local-fixture.search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(&provider, &cap, endpoint, &scope));

        assert_eq!(plan.network_policy.allowed_targets.len(), 1);
        assert_eq!(
            plan.network_policy.allowed_targets[0].host_pattern,
            "127.0.0.1"
        );
        assert_eq!(
            plan.network_policy.allowed_targets[0].scheme,
            Some(NetworkScheme::Http)
        );
        assert!(!plan.network_policy.deny_private_ip_ranges);
    }

    #[test]
    fn planner_hosted_single_tenant_rejects_installed_local_http() {
        let endpoint = "http://localhost:3002/mcp";
        let registry = Arc::new(SharedExtensionRegistry::new(
            registry_with_local_mcp_provider("tianquan-graph", endpoint),
        ));
        let planner = RegistryMcpEgressPlanner::new_with_hosted_local_mcp(registry);
        let provider = ExtensionId::new("tianquan-graph").unwrap();
        let cap = CapabilityId::new("tianquan-graph.tools").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(&provider, &cap, endpoint, &scope));

        assert!(plan.network_policy.allowed_targets.is_empty());
    }

    #[test]
    fn planner_generic_production_rejects_host_bundled_private_http() {
        let endpoint = "http://127.0.0.1:10516/mcp/internal";
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_provider(
            "production-local-fixture",
            endpoint,
            "production-local-fixture.search",
            "fixture_token",
        )));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("production-local-fixture").unwrap();
        let cap = CapabilityId::new("production-local-fixture.search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(&provider, &cap, endpoint, &scope));

        assert!(plan.network_policy.allowed_targets.is_empty());
    }

    /// Production planner (`new`, allow_local_mcp=false) must reject an
    /// InstalledLocal http MCP package — fail-closed even if the package is
    /// registered. Guards against accidental production enablement.
    #[test]
    fn planner_production_rejects_installed_local_http_loopback() {
        let registry = Arc::new(SharedExtensionRegistry::new(
            registry_with_local_mcp_provider("tianquan-graph", "http://localhost:3002/mcp"),
        ));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("tianquan-graph").unwrap();
        let cap = CapabilityId::new("tianquan-graph.tools").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "http://localhost:3002/mcp",
            &scope,
        ));

        // Production: InstalledLocal + http rejected, egress plan empty.
        assert!(plan.network_policy.allowed_targets.is_empty());
    }

    fn registry_with_local_mcp_provider(
        provider: &str,
        url: &str,
    ) -> ironclaw_extensions::ExtensionRegistry {
        // Minimal InstalledLocal + http MCP package, no credentials (mirrors
        // TianQuan api /mcp: unauthenticated container-internal server).
        let mut registry = ironclaw_extensions::ExtensionRegistry::new();
        registry
            .insert(
                ExtensionPackage::from_manifest(
                    ExtensionManifest {
                        schema_version: ironclaw_extensions::MANIFEST_SCHEMA_VERSION.to_string(),
                        id: ExtensionId::new(provider).unwrap(),
                        name: provider.to_string(),
                        version: "0.1.0".to_string(),
                        description: "Local MCP".to_string(),
                        source: ManifestSource::InstalledLocal,
                        requested_trust: ironclaw_host_api::RequestedTrustClass::ThirdParty,
                        descriptor_trust_default: TrustClass::Sandbox,
                        runtime: ironclaw_extensions::ExtensionRuntime::Mcp {
                            transport: "http".to_string(),
                            command: None,
                            args: Vec::new(),
                            url: Some(url.to_string()),
                        },
                        host_apis: Vec::new(),
                        hooks: Vec::new(),
                        capabilities: vec![ironclaw_extensions::CapabilityManifest {
                            id: CapabilityId::new("tianquan-graph.tools").unwrap(),
                            implements: Vec::new(),
                            description: "Aggregated tools".to_string(),
                            effects: vec![
                                ironclaw_host_api::EffectKind::DispatchCapability,
                                ironclaw_host_api::EffectKind::Network,
                            ],
                            default_permission: PermissionMode::Allow,
                            visibility: ironclaw_extensions::CapabilityVisibility::Model,
                            input_schema_ref: CapabilityProfileSchemaRef::new(
                                "schemas/tianquan-graph/tools.input.v1.json",
                            )
                            .unwrap(),
                            output_schema_ref: CapabilityProfileSchemaRef::new(
                                "schemas/tianquan-graph/tools.output.v1.json",
                            )
                            .unwrap(),
                            prompt_doc_ref: None,
                            required_host_ports: Vec::new(),
                            runtime_credentials: Vec::new(),
                            network_targets: Vec::new(),
                            resource_profile: None,
                        }],
                    },
                    VirtualPath::new(format!("/system/extensions/{provider}")).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        registry
    }

    #[test]
    fn planner_denies_wrong_host_for_notion_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("notion").unwrap();
        let cap = CapabilityId::new("notion.notion-search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "https://evil.example.com/mcp",
            &scope,
        ));

        assert!(plan.credential_injections.is_empty());
        assert!(plan.network_policy.allowed_targets.is_empty());
    }

    #[test]
    fn planner_denies_http_scheme_for_notion_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("notion").unwrap();
        let cap = CapabilityId::new("notion.notion-search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "http://mcp.notion.com/mcp",
            &scope,
        ));

        assert!(plan.credential_injections.is_empty());
    }

    #[test]
    fn planner_denies_wrong_path_for_notion_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("notion").unwrap();
        let cap = CapabilityId::new("notion.notion-search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "https://mcp.notion.com/other",
            &scope,
        ));

        assert!(plan.credential_injections.is_empty());
    }

    // ── happy path policy ─────────────────────────────────────────────────

    #[test]
    fn planner_emits_locked_policy_for_notion_provider() {
        let registry = Arc::new(SharedExtensionRegistry::new(registry_with_notion()));
        let planner = RegistryMcpEgressPlanner::new(registry);
        let provider = ExtensionId::new("notion").unwrap();
        let cap = CapabilityId::new("notion.notion-search").unwrap();
        let scope = sample_scope();

        let plan = planner.plan(sample_plan_request(
            &provider,
            &cap,
            "https://mcp.notion.com/mcp",
            &scope,
        ));

        assert_eq!(plan.credential_injections.len(), 1);
        assert!(plan.network_policy.deny_private_ip_ranges);
        assert_eq!(plan.network_policy.allowed_targets.len(), 1);
        assert_eq!(
            plan.network_policy.allowed_targets[0].host_pattern,
            NOTION_MCP_HOST.to_string()
        );
        assert_eq!(
            plan.network_policy.allowed_targets[0].scheme,
            Some(NetworkScheme::Https)
        );
        assert_eq!(plan.response_body_limit, Some(MCP_RESPONSE_BODY_LIMIT));
        assert_eq!(plan.timeout_ms, Some(MCP_TIMEOUT_MS));
        // 覆盖:AE-L2-109的planner部分；实际延迟网络caller仍需另验。
        let mut configured = planner;
        configured.timeout_ms = Some(300_000);
        let long_plan = configured.plan(sample_plan_request(
            &provider,
            &cap,
            "https://mcp.notion.com/mcp",
            &scope,
        ));
        assert_eq!(long_plan.timeout_ms, Some(300_000));
        assert!(long_plan.network_policy.deny_private_ip_ranges);
        assert_eq!(long_plan.response_body_limit, Some(MCP_RESPONSE_BODY_LIMIT));
        configured.timeout_ms = None;
        let denied = configured.plan(sample_plan_request(
            &provider,
            &cap,
            "https://mcp.notion.com/mcp",
            &scope,
        ));
        assert!(denied.network_policy.allowed_targets.is_empty());
    }

    // ── URL allowlist unit tests ───────────────────────────────────────────

    #[test]
    fn hosted_mcp_url_allowed_accepts_canonical_notion_url() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(hosted_mcp_url_allowed(NOTION_MCP_URL, &endpoint));
    }

    #[test]
    fn hosted_mcp_url_allowed_accepts_same_origin_trusted_internal_path() {
        let endpoint = HostedMcpEndpoint::parse("http://api:3002/mcp", true).unwrap();
        assert!(hosted_mcp_url_allowed(
            "http://api:3002/mcp/internal",
            &endpoint
        ));
    }

    #[test]
    fn hosted_mcp_url_allowed_rejects_http_scheme() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(!hosted_mcp_url_allowed(
            "http://mcp.notion.com/mcp",
            &endpoint
        ));
    }

    #[test]
    fn hosted_mcp_url_allowed_rejects_wrong_host() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(!hosted_mcp_url_allowed(
            "https://evil.example.com/mcp",
            &endpoint
        ));
    }

    #[test]
    fn hosted_mcp_url_allowed_rejects_wrong_path() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(!hosted_mcp_url_allowed(
            "https://mcp.notion.com/other",
            &endpoint
        ));
    }

    #[test]
    fn hosted_mcp_url_allowed_accepts_trailing_slash() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(hosted_mcp_url_allowed(
            "https://mcp.notion.com/mcp/",
            &endpoint
        ));
    }

    #[test]
    fn hosted_mcp_url_allowed_rejects_extra_url_components() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();

        assert!(!hosted_mcp_url_allowed(
            "https://mcp.notion.com/mcp?token=shadow",
            &endpoint
        ));
        assert!(!hosted_mcp_url_allowed(
            "https://mcp.notion.com/mcp#fragment",
            &endpoint
        ));
        assert!(!hosted_mcp_url_allowed(
            "https://user@mcp.notion.com/mcp",
            &endpoint
        ));
    }

    // ── helpers ───────────────────────────────────────────────────────────

    fn sample_scope() -> ResourceScope {
        ResourceScope {
            tenant_id: TenantId::new("test-tenant").unwrap(),
            user_id: UserId::new("test-user").unwrap(),
            agent_id: None,
            project_id: Some(ProjectId::new("test-project").unwrap()),
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        }
    }

    fn sample_plan_request<'a>(
        provider: &'a ExtensionId,
        capability_id: &'a CapabilityId,
        url: &'a str,
        scope: &'a ResourceScope,
    ) -> McpHostHttpEgressPlanRequest<'a> {
        McpHostHttpEgressPlanRequest {
            provider,
            capability_id,
            scope,
            transport: "http",
            method: NetworkMethod::Post,
            url,
            headers: &[],
            body: &[],
        }
    }

    fn registry_with_notion() -> ironclaw_extensions::ExtensionRegistry {
        registry_with_provider(
            "notion",
            NOTION_MCP_URL,
            "notion.notion-search",
            "mcp_notion_access_token",
        )
    }

    // Exercise the real validated TOML digest production constructor; from_manifest has no digest.
    fn real_digest_registry_for_tenant_fixture(
        endpoint: &str,
    ) -> ironclaw_extensions::ExtensionRegistry {
        let raw = include_str!(
            "../../../ironclaw_first_party_extensions/assets/notion-mcp/manifest.toml"
        )
        .replace("https://mcp.notion.com/mcp", endpoint);
        let ports = ironclaw_host_runtime::default_host_port_catalog().unwrap();
        let contracts=crate::extension_host::host_api_contracts::product_extension_host_api_contract_registry().unwrap();
        let record = ironclaw_extensions::ExtensionManifestRecord::from_toml_with_contracts(
            raw,
            ManifestSource::HostBundled,
            &ports,
            None,
            &contracts,
        )
        .unwrap();
        let manifest = record.manifest().clone().try_into().unwrap();
        let package = ExtensionPackage::from_manifest_toml(
            manifest,
            VirtualPath::new("/system/extensions/notion").unwrap(),
            record.raw_toml(),
        )
        .unwrap();
        assert_eq!(
            package.manifest_digest().as_deref(),
            Some(ironclaw_host_api::sha256_digest_token(record.raw_toml().as_bytes()).as_str())
        );
        let mut registry = ironclaw_extensions::ExtensionRegistry::new();
        registry.insert(package).unwrap();
        registry
    }

    fn registry_with_provider(
        provider: &str,
        url: &str,
        capability_id: &str,
        credential_handle: &str,
    ) -> ironclaw_extensions::ExtensionRegistry {
        let mut registry = ironclaw_extensions::ExtensionRegistry::new();
        let host = url::Url::parse(url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        registry
            .insert(
                ExtensionPackage::from_manifest(
                    ExtensionManifest {
                        schema_version: ironclaw_extensions::MANIFEST_SCHEMA_VERSION.to_string(),
                        id: ExtensionId::new(provider).unwrap(),
                        name: provider.to_string(),
                        version: "0.1.0".to_string(),
                        description: "Hosted MCP".to_string(),
                        source: ManifestSource::HostBundled,
                        requested_trust: ironclaw_host_api::RequestedTrustClass::ThirdParty,
                        descriptor_trust_default: TrustClass::Sandbox,
                        runtime: ironclaw_extensions::ExtensionRuntime::Mcp {
                            transport: "http".to_string(),
                            command: None,
                            args: Vec::new(),
                            url: Some(url.to_string()),
                        },
                        host_apis: Vec::new(),
                        hooks: Vec::new(),
                        capabilities: vec![ironclaw_extensions::CapabilityManifest {
                            id: CapabilityId::new(capability_id).unwrap(),
                            implements: Vec::new(),
                            description: "Search".to_string(),
                            effects: vec![
                                ironclaw_host_api::EffectKind::DispatchCapability,
                                ironclaw_host_api::EffectKind::Network,
                                ironclaw_host_api::EffectKind::UseSecret,
                            ],
                            default_permission: PermissionMode::Allow,
                            visibility: ironclaw_extensions::CapabilityVisibility::Model,
                            input_schema_ref: CapabilityProfileSchemaRef::new(
                                "schemas/notion/notion-search.input.v1.json",
                            )
                            .unwrap(),
                            output_schema_ref: CapabilityProfileSchemaRef::new(
                                "schemas/notion/notion-search.output.v1.json",
                            )
                            .unwrap(),
                            prompt_doc_ref: None,
                            required_host_ports: Vec::new(),
                            runtime_credentials: vec![
                                ironclaw_host_api::RuntimeCredentialRequirement {
                                    handle: SecretHandle::new(credential_handle).unwrap(),
                                    source:
                                        RuntimeCredentialRequirementSource::ProductAuthAccount {
                                            provider: RuntimeCredentialAccountProviderId::new(
                                                provider,
                                            )
                                            .unwrap(),
                                            setup: Default::default(),
                                        },
                                    provider_scopes: Vec::new(),
                                    audience: NetworkTargetPattern {
                                        scheme: Some(NetworkScheme::Https),
                                        host_pattern: host,
                                        port: None,
                                    },
                                    target: RuntimeCredentialTarget::Header {
                                        name: "authorization".to_string(),
                                        prefix: Some("Bearer ".to_string()),
                                    },
                                    required: true,
                                },
                            ],
                            network_targets: Vec::new(),
                            resource_profile: None,
                        }],
                    },
                    VirtualPath::new(format!("/system/extensions/{provider}")).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        registry
    }
}

pub(crate) fn hosted_http_mcp_runtime_with_operator_bundles(
    registry: Arc<SharedExtensionRegistry>,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
    bundles: &[Arc<crate::RebornHostExtensionBundle>],
    actual_validated_runtime_tenant: &TenantId,
) -> Result<
    McpRuntime<
        McpHostHttpClient<
            McpRuntimeHttpAdapter<Arc<dyn RuntimeHttpEgress>>,
            RegistryMcpEgressPlanner,
        >,
    >,
    ironclaw_product_workflow::ProductWorkflowError,
> {
    if bundles.is_empty() {
        return Ok(hosted_http_mcp_runtime(
            registry,
            runtime_http_egress,
            false,
            false,
        ));
    }
    let provider = bundles
        .iter()
        .flat_map(|b| b.bindings())
        .next()
        .map(|b| b.provider().clone());
    let provider = match provider {
        Some(provider) => provider,
        None => ExtensionId::new("tianquan-graph").map_err(|error| {
            ironclaw_product_workflow::ProductWorkflowError::InvalidBindingRequest {
                reason: error.to_string(),
            }
        })?,
    };
    let signer = McpTrustedContextSigner::from_env(provider);
    let planner = RegistryMcpEgressPlanner::new_with_operator_mcp(
        registry,
        bundles,
        &signer,
        actual_validated_runtime_tenant,
    )?;
    let client = McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress), planner)
        .with_trusted_context_signer(signer);
    Ok(McpRuntime::new(McpRuntimeConfig::default(), client))
}

pub(crate) fn hosted_http_mcp_runtime_with_late_store(
    registry: Arc<SharedExtensionRegistry>,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
    bundles: &[Arc<crate::RebornHostExtensionBundle>],
    tenant: &TenantId,
    store: Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
) -> Result<
    McpRuntime<
        McpHostHttpClient<
            McpRuntimeHttpAdapter<Arc<dyn RuntimeHttpEgress>>,
            RegistryMcpEgressPlanner,
        >,
    >,
    ironclaw_product_workflow::ProductWorkflowError,
> {
    let provider = bundles
        .iter()
        .flat_map(|b| b.bindings())
        .next()
        .map(|b| b.provider().clone())
        .unwrap_or_else(|| {
            ExtensionId::new("tianquan-graph").expect("existing static signer provider")
        });
    let signer = McpTrustedContextSigner::from_env(provider);
    store.set_actual_signer(&signer)?;
    let mut planner = if bundles.is_empty() {
        RegistryMcpEgressPlanner::new(registry)
    } else {
        RegistryMcpEgressPlanner::new_with_operator_mcp(registry, bundles, &signer, tenant)?
    };
    planner.late_admissions = store;
    let client = McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress), planner)
        .with_trusted_context_signer(signer);
    Ok(McpRuntime::new(McpRuntimeConfig::default(), client))
}
