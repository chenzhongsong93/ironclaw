use std::sync::Arc;

use ironclaw_extensions::{
    ExtensionPackage, ExtensionRegistry, ExtensionRuntime, SharedExtensionRegistry,
    package_with_discovered_hosted_mcp_tools, package_with_discovered_loopback_http_mcp_tools,
    is_host_bundled_loopback_http_mcp_package,
};
use ironclaw_host_api::{ResourceScope, RuntimeHttpEgress};
use ironclaw_mcp::{McpClient, McpClientRequest, McpHostHttpClient, McpRuntimeHttpAdapter};

use crate::extension_host::mcp::{MCP_RESPONSE_BODY_LIMIT, RegistryMcpEgressPlanner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostedMcpDiscoveryError {
    Transient(String),
    Permanent(String),
}

pub(crate) async fn discover_hosted_mcp_package(
    package: &ExtensionPackage,
    scope: ResourceScope,
    runtime_http_egress: Arc<dyn RuntimeHttpEgress>,
) -> Result<ExtensionPackage, HostedMcpDiscoveryError> {
    discover_hosted_mcp_package_with_admission(package,scope,runtime_http_egress,None).await
}

pub(crate) fn root_loopback_package_declared(package: &ExtensionPackage) -> bool {
    match &package.manifest.runtime {
        ExtensionRuntime::Mcp { url: Some(url), .. } => is_host_bundled_loopback_http_mcp_package(package, url),
        _ => false,
    }
}

pub(crate) fn root_loopback_package_admitted(
    package: &ExtensionPackage,
    tenant: Option<&ironclaw_host_api::TenantId>,
    store: Option<&crate::operator_late_admission::OperatorLateAdmissionStore>,
) -> bool {
    let Some(store) = store else { return false; };
    let Some((binding, signer)) = store.discovery(&package.id) else { return false; };
    tenant.is_none_or(|tenant| binding.tenant() == tenant)
        && store.old_binding_valid(&binding)
        && signer.is_configured() && signer.provider_id() == binding.provider()
        && signer.audience() == Some(binding.audience())
        && is_host_bundled_loopback_http_mcp_package(package, binding.audience())
        && binding.endpoint_for(package, binding.audience()).is_some()
}

pub(crate) async fn discover_hosted_mcp_package_with_admission(package:&ExtensionPackage,scope:ResourceScope,runtime_http_egress:Arc<dyn RuntimeHttpEgress>,store:Option<Arc<crate::operator_late_admission::OperatorLateAdmissionStore>>)->Result<ExtensionPackage,HostedMcpDiscoveryError> {
    let tenant = scope.tenant_id.clone();
    let root_loopback = root_loopback_package_admitted(package, Some(&tenant), store.as_deref());
    let (transport, command, args, url) = match &package.manifest.runtime {
        ExtensionRuntime::Mcp {
            transport,
            command,
            args,
            url,
        } if is_hosted_http_mcp_package(package) || root_loopback => (
            transport.clone(),
            command.clone(),
            args.clone(),
            url.clone(),
        ),
        _ => {
            return Err(HostedMcpDiscoveryError::Permanent(format!(
                "extension {} is not a host-bundled hosted MCP provider",
                package.id
            )));
        }
    };
    let registry = Arc::new(SharedExtensionRegistry::new(ExtensionRegistry::new()));
    registry.upsert(package.clone()).map_err(|error| {
        HostedMcpDiscoveryError::Permanent(format!(
            "failed to prepare hosted MCP discovery: {error}"
        ))
    })?;
    let planning_capability_id = package
        .manifest
        .capabilities
        .first()
        .map(|capability| capability.id.clone())
        .ok_or_else(|| {
            HostedMcpDiscoveryError::Permanent(format!(
                "hosted MCP provider {} has no capability template",
                package.id
            ))
        })?;
    let mut planner=RegistryMcpEgressPlanner::new(registry);
    let mut discovery_url = url.clone();
    let actual_signer=if let Some(store)=store.clone() {
        let (binding,signer)=store.discovery(&package.id).ok_or_else(||HostedMcpDiscoveryError::Permanent("sameRuntimeDiscoveryAdmissionMissing".into()))?;
        if binding.tenant()!=&scope.tenant_id||binding.endpoint_for(package,url.as_deref().unwrap_or("")).is_none(){return Err(HostedMcpDiscoveryError::Permanent("sameRuntimeDiscoveryPackageMismatch".into()));}
        discovery_url = Some(planner.for_root_discovery(binding,store).ok_or_else(||HostedMcpDiscoveryError::Permanent("sameRuntimeDiscoveryCatalogUrlRejected".into()))?);
        Some(signer)
    }else{None};
    let client=McpHostHttpClient::new(McpRuntimeHttpAdapter::new(runtime_http_egress),planner);
    let client=if let Some(signer)=actual_signer{client.with_trusted_context_signer(signer)}else{client};
    let output = client
        .discover_tools(McpClientRequest {
            provider: package.id.clone(),
            capability_id: planning_capability_id,
            scope,
            transport,
            command,
            args,
            url: discovery_url,
            input: serde_json::Value::Null,
            max_output_bytes: MCP_RESPONSE_BODY_LIMIT,
            trusted_context: None,
        })
        .await
        .map_err(|error| HostedMcpDiscoveryError::Transient(error.stable_reason().to_string()))?;
    if output.tools.is_empty() {
        return Err(HostedMcpDiscoveryError::Transient(format!(
            "hosted MCP provider {} returned no discoverable tools",
            package.id
        )));
    }
    if root_loopback {
        if !root_loopback_package_admitted(package, Some(&tenant), store.as_deref()) {
            return Err(HostedMcpDiscoveryError::Permanent("sameRuntimeLoopbackDiscoveryBindingChanged".into()));
        }
        package_with_discovered_loopback_http_mcp_tools(package, &output.tools, url.as_deref().unwrap_or(""))
            .map_err(|error| HostedMcpDiscoveryError::Permanent(error.to_string()))
    } else {
        package_with_discovered_hosted_mcp_tools(package, &output.tools)
            .map_err(|error| HostedMcpDiscoveryError::Permanent(error.to_string()))
    }
}

pub(crate) use ironclaw_extensions::is_hosted_http_mcp_package;
