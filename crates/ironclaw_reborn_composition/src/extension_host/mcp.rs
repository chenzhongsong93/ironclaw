use std::sync::Arc;

use ironclaw_extensions::{
    ExtensionPackage, ExtensionRuntime, ManifestSource, SharedExtensionRegistry,
};
use ironclaw_host_api::{
    CapabilityId, ExtensionId, NetworkPolicy, NetworkScheme, NetworkTargetPattern,
    RuntimeCredentialInjection, RuntimeCredentialSource, RuntimeHttpEgress,
};
use ironclaw_mcp::{
    McpHostHttpClient, McpHostHttpEgressPlan, McpHostHttpEgressPlanRequest,
    McpHostHttpEgressPlanner, McpRuntime, McpRuntimeConfig, McpRuntimeHttpAdapter,
};

pub(crate) const MCP_RESPONSE_BODY_LIMIT: u64 = 2 * 1024 * 1024;
const MCP_NETWORK_EGRESS_LIMIT: u64 = 2 * 1024 * 1024;
const MCP_TIMEOUT_MS: u32 = 60_000;

pub(crate) fn hosted_http_mcp_runtime(
    registry: Arc<SharedExtensionRegistry>,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
    allow_local_mcp: bool,
) -> McpRuntime<
    McpHostHttpClient<McpRuntimeHttpAdapter<Arc<dyn RuntimeHttpEgress>>, RegistryMcpEgressPlanner>,
> {
    let planner = if allow_local_mcp {
        RegistryMcpEgressPlanner::new_with_local_mcp(registry)
    } else {
        RegistryMcpEgressPlanner::new(registry)
    };
    let client = McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress), planner);
    McpRuntime::new(McpRuntimeConfig::default(), client)
}

#[derive(Debug, Clone)]
pub(crate) struct RegistryMcpEgressPlanner {
    registry: Arc<SharedExtensionRegistry>,
    /// Local-dev override: when true, admits InstalledLocal MCP packages,
    /// http scheme, and private/loopback egress targets. Production builds
    /// must keep this false (fail-closed against internal metadata services).
    allow_local_mcp: bool,
}

impl RegistryMcpEgressPlanner {
    pub(crate) fn new(registry: Arc<SharedExtensionRegistry>) -> Self {
        Self {
            registry,
            allow_local_mcp: false,
        }
    }

    /// Local-dev constructor: admits InstalledLocal + http + private/loopback
    /// MCP egress. Caller must gate on `profile.is_local_dev()` to keep
    /// production fail-closed.
    pub(crate) fn new_with_local_mcp(registry: Arc<SharedExtensionRegistry>) -> Self {
        Self {
            registry,
            allow_local_mcp: true,
        }
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
        registry
            .get_extension(provider)
            .and_then(|pkg| hosted_http_mcp_endpoint(pkg, self.allow_local_mcp))
    }
}

impl McpHostHttpEgressPlanner for RegistryMcpEgressPlanner {
    fn plan(&self, request: McpHostHttpEgressPlanRequest<'_>) -> McpHostHttpEgressPlan {
        let Some(endpoint) = self.provider_endpoint(request.provider) else {
            return McpHostHttpEgressPlan::default();
        };
        if !hosted_mcp_url_allowed(request.url, &endpoint) {
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
            network_policy: hosted_mcp_network_policy(&endpoint, self.allow_local_mcp),
            credential_injections,
            response_body_limit: Some(MCP_RESPONSE_BODY_LIMIT),
            timeout_ms: Some(MCP_TIMEOUT_MS),
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
        Self::parse(url, allow_http).is_some_and(|request_endpoint| request_endpoint == *self)
    }
}

pub(crate) fn hosted_http_mcp_endpoint(
    package: &ExtensionPackage,
    allow_local: bool,
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
    HostedMcpEndpoint::parse(url, allow_local)
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
    }

    // ── URL allowlist unit tests ───────────────────────────────────────────

    #[test]
    fn hosted_mcp_url_allowed_accepts_canonical_notion_url() {
        let endpoint = HostedMcpEndpoint::parse(NOTION_MCP_URL, false).unwrap();
        assert!(hosted_mcp_url_allowed(NOTION_MCP_URL, &endpoint));
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
