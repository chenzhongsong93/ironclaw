use ironclaw_host_api::{
    CapabilityDescriptor, CapabilityId, CapabilityProfileSchemaRef, EffectKind, PermissionMode,
    RuntimeKind,
};
use serde_json::Value;

use crate::{
    CapabilityManifest, CapabilityVisibility, ExtensionError, ExtensionPackage, ExtensionRuntime,
    ManifestSource,
};

/// MCP tool descriptor discovered from a hosted provider and converted by the
/// extension domain into a dynamic capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedMcpDiscoveredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub annotations: HostedMcpDiscoveredToolAnnotations,
}

/// Advisory MCP tool behavior hints returned by `tools/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostedMcpDiscoveredToolAnnotations {
    pub destructive_hint: bool,
    pub side_effects_hint: bool,
    pub read_only_hint: bool,
}

pub fn is_hosted_http_mcp_package(package: &ExtensionPackage) -> bool {
    hosted_http_mcp_url(package).is_some()
}

pub fn package_with_discovered_hosted_mcp_tools(
    package: &ExtensionPackage,
    tools: &[HostedMcpDiscoveredTool],
) -> Result<ExtensionPackage, ExtensionError> {
    if hosted_http_mcp_url(package).is_none() {
        return Err(invalid_hosted_mcp_manifest(format!(
            "extension {} is not a host-bundled hosted MCP provider",
            package.id
        )));
    }
    let template = hosted_mcp_capability_template(package)?;

    let mut manifest = package.manifest.clone();
    manifest.capabilities = tools
        .iter()
        .map(|tool| discovered_capability_manifest(package, &template, tool))
        .collect::<Result<Vec<_>, _>>()?;

    finish_discovered_package(package, manifest, tools, DiscoverySafetyMetadata::Generic)

}

/// Pure metadata predicate for a host-bundled canonical loopback declaration.
/// The composition Root caller must separately verify its live operator binding;
/// this function performs no network I/O and grants no execution authority.
pub fn is_host_bundled_loopback_http_mcp_package(package: &ExtensionPackage, expected_url: &str) -> bool {
    if package.manifest.source != ManifestSource::HostBundled { return false; }
    let ExtensionRuntime::Mcp { transport, command: None, args, url: Some(actual) } = &package.manifest.runtime else { return false; };
    transport == "http" && args.is_empty() && actual == expected_url && valid_loopback_http_mcp_url(actual)
}

fn valid_loopback_http_mcp_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else { return false; };
    let Some(port) = url.port() else { return false; };
    url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && port > 0
        && url.username().is_empty() && url.password().is_none()
        && url.query().is_none() && url.fragment().is_none() && url.path() != "/"
        && url.as_str() == value
        && value.strip_prefix("http://").and_then(|v| v.split('/').next()) == Some(format!("127.0.0.1:{port}").as_str())
}

/// Explicit pure-data conversion used only by composition's verified Root
/// loopback discovery path. Known capability safety metadata is preserved;
/// unrecognized tools remain Ask. Generic hosted HTTPS behavior is unchanged.
pub fn package_with_discovered_loopback_http_mcp_tools(
    package: &ExtensionPackage,
    tools: &[HostedMcpDiscoveredTool],
    expected_url: &str,
) -> Result<ExtensionPackage, ExtensionError> {
    if !is_host_bundled_loopback_http_mcp_package(package, expected_url) {
        return Err(invalid_hosted_mcp_manifest("loopback MCP package declaration mismatch".into()));
    }
    // A nonempty subset must never silently retire declared functionality.
    // Do not synthesize missing tools: the actual catalog must cover each id.
    let supplied: std::collections::BTreeSet<String> = tools.iter().map(|tool| format!("{}.{}", package.id.as_str(), tool.name)).collect();
    if package.manifest.capabilities.iter().any(|known| !supplied.contains(known.id.as_str())) {
        return Err(invalid_hosted_mcp_manifest("loopback MCP catalog is missing a declared capability".into()));
    }
    let url = url::Url::parse(expected_url).map_err(|_| invalid_hosted_mcp_manifest("loopback MCP URL invalid".into()))?;
    let target = ironclaw_host_api::NetworkTargetPattern {
        scheme: Some(ironclaw_host_api::NetworkScheme::Http),
        host_pattern: "127.0.0.1".into(), port: url.port(),
    };
    let mut manifest = package.manifest.clone();
    let mut names = std::collections::BTreeSet::new();
    manifest.capabilities = tools.iter().map(|tool| {
        if !names.insert(tool.name.as_str()) {
            return Err(invalid_hosted_mcp_manifest("duplicate discovered loopback MCP tool".into()));
        }
        if !tool.input_schema.is_object() {
            return Err(invalid_hosted_mcp_manifest("discovered MCP parameters schema must be an object".into()));
        }
        let id = format!("{}.{}", package.id.as_str(), tool.name);
        if let Some(known) = package.manifest.capabilities.iter().find(|cap| cap.id.as_str() == id) {
            // Includes permission/effects/host ports/credentials/network targets,
            // visibility/resource profile and original schema/prompt coordinates.
            // Inline parameters below are still taken from the actual catalog.
            return Ok(known.clone());
        }
        // Only unknown tools need a provider-wide template. Inconsistent
        // templates fail closed instead of guessing credential/host authority.
        let template = hosted_mcp_capability_template(package)?;
        let mut unknown = discovered_capability_manifest(package, &template, tool)?;
        unknown.network_targets = vec![target.clone()];
        if template.provider_declares_external_write && !unknown.effects.contains(&EffectKind::ExternalWrite) {
            unknown.effects.push(EffectKind::ExternalWrite);
        }
        Ok(unknown)
    }).collect::<Result<Vec<_>, ExtensionError>>()?;
    finish_discovered_package(package, manifest, tools, DiscoverySafetyMetadata::PreserveKnown)
}

#[derive(Clone, Copy)]
enum DiscoverySafetyMetadata { Generic, PreserveKnown }

fn finish_discovered_package(package: &ExtensionPackage, manifest: crate::ExtensionManifest, tools: &[HostedMcpDiscoveredTool], safety: DiscoverySafetyMetadata) -> Result<ExtensionPackage, ExtensionError> {
    let capabilities = manifest
        .capabilities
        .iter()
        .zip(tools)
        .map(|(capability, tool)| {
            if matches!(safety, DiscoverySafetyMetadata::PreserveKnown) {
                if let Some(original) = package.capabilities.iter().find(|known| known.id == capability.id) {
                    let mut observed = original.clone();
                    observed.parameters_schema = tool.input_schema.clone();
                    return observed;
                }
            }
            CapabilityDescriptor {
            id: capability.id.clone(),
            provider: package.id.clone(),
            runtime: RuntimeKind::Mcp,
            trust_ceiling: manifest.descriptor_trust_default,
            description: capability.description.clone(),
            parameters_schema: tool.input_schema.clone(),
            effects: capability.effects.clone(),
            default_permission: capability.default_permission,
            runtime_credentials: capability.runtime_credentials.clone(),
            network_targets: capability.network_targets.clone(),
            resource_profile: capability.resource_profile.clone(),
            }
        })
        .collect();

    ExtensionPackage::from_host_bundled_manifest_with_inline_dynamic_schemas(
        manifest,
        package.root.clone(),
        package.manifest_digest(),
        capabilities,
    )
}

fn hosted_http_mcp_url(package: &ExtensionPackage) -> Option<&str> {
    if package.manifest.source != ManifestSource::HostBundled {
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
    if transport != "http" || !args.is_empty() || !valid_hosted_mcp_url(url) {
        return None;
    }
    Some(url.as_str())
}

fn valid_hosted_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.host_str().is_some()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

fn hosted_mcp_capability_template(
    package: &ExtensionPackage,
) -> Result<HostedMcpCapabilityTemplate, ExtensionError> {
    let first = package.manifest.capabilities.first().ok_or_else(|| {
        invalid_hosted_mcp_manifest(format!(
            "hosted MCP provider {} has no capability template",
            package.id
        ))
    })?;
    for capability in &package.manifest.capabilities[1..] {
        if capability.required_host_ports != first.required_host_ports
            || capability.runtime_credentials != first.runtime_credentials
            || capability.resource_profile != first.resource_profile
        {
            return Err(invalid_hosted_mcp_manifest(format!(
                "hosted MCP provider {} has inconsistent capability templates",
                package.id
            )));
        }
    }
    Ok(HostedMcpCapabilityTemplate {
        provider_declares_external_write: package
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.effects.contains(&EffectKind::ExternalWrite)),
        required_host_ports: first.required_host_ports.clone(),
        runtime_credentials: first.runtime_credentials.clone(),
        resource_profile: first.resource_profile.clone(),
    })
}

struct HostedMcpCapabilityTemplate {
    provider_declares_external_write: bool,
    required_host_ports: Vec<ironclaw_host_api::HostPortId>,
    runtime_credentials: Vec<ironclaw_host_api::RuntimeCredentialRequirement>,
    resource_profile: Option<ironclaw_host_api::ResourceProfile>,
}

fn discovered_capability_manifest(
    package: &ExtensionPackage,
    template: &HostedMcpCapabilityTemplate,
    tool: &HostedMcpDiscoveredTool,
) -> Result<CapabilityManifest, ExtensionError> {
    let capability_id =
        CapabilityId::new(format!("{}.{}", package.id.as_str(), tool.name)).map_err(|error| {
            invalid_hosted_mcp_manifest(format!(
                "discovered MCP tool {} from {} cannot be published as a Reborn capability: {error}",
                tool.name, package.id
            ))
        })?;
    let schema_path = tool.name.replace('.', "/");
    let input_schema_ref = CapabilityProfileSchemaRef::new(format!(
        "schemas/{}/dynamic/{schema_path}.input.v1.json",
        package.id.as_str()
    ))
    .map_err(|error| {
        invalid_hosted_mcp_manifest(format!("invalid discovered MCP input schema ref: {error}"))
    })?;
    let output_schema_ref = CapabilityProfileSchemaRef::new(format!(
        "schemas/{}/dynamic/{schema_path}.output.v1.json",
        package.id.as_str()
    ))
    .map_err(|error| {
        invalid_hosted_mcp_manifest(format!("invalid discovered MCP output schema ref: {error}"))
    })?;
    let mut effects = vec![EffectKind::DispatchCapability, EffectKind::Network];
    if !template.runtime_credentials.is_empty() {
        effects.push(EffectKind::UseSecret);
    }
    if discovered_tool_requires_external_write(template, tool) {
        effects.push(EffectKind::ExternalWrite);
    }

    Ok(CapabilityManifest {
        id: capability_id,
        implements: Vec::new(),
        description: if tool.description.trim().is_empty() {
            format!("Invoke hosted MCP tool {}", tool.name)
        } else {
            tool.description.clone()
        },
        effects,
        default_permission: PermissionMode::Ask,
        visibility: CapabilityVisibility::Model,
        input_schema_ref,
        output_schema_ref,
        prompt_doc_ref: None,
        required_host_ports: template.required_host_ports.clone(),
        runtime_credentials: template.runtime_credentials.clone(),
        // MCP discovered tools derive egress from their credential audiences,
        // not a manifest-declared allowlist.
        network_targets: Vec::new(),
        resource_profile: template.resource_profile.clone(),
    })
}

fn discovered_tool_requires_external_write(
    template: &HostedMcpCapabilityTemplate,
    tool: &HostedMcpDiscoveredTool,
) -> bool {
    if tool.annotations.destructive_hint || tool.annotations.side_effects_hint {
        return true;
    }
    if tool.annotations.read_only_hint {
        return false;
    }
    // MCP annotations are advisory. For providers whose bundled manifest
    // declares write-capable tools, unannotated discovered tools stay
    // conservative so policy surfaces do not understate possible side effects.
    template.provider_declares_external_write
}

fn invalid_hosted_mcp_manifest(reason: String) -> ExtensionError {
    ExtensionError::InvalidManifest { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExtensionManifest, HostPortCatalog, ManifestSource};
    use ironclaw_host_api::{EffectKind, RuntimeKind, VirtualPath};

    const NOTION_MANIFEST: &str = r#"
schema_version = "reborn.extension_manifest.v2"
id = "notion"
name = "Notion"
version = "1.0.0"
description = "Hosted Notion MCP"
trust = "third_party"

[runtime]
kind = "mcp"
transport = "http"
url = "https://mcp.notion.com/mcp"

[[capabilities]]
id = "notion.notion-fetch"
description = "Fetch a Notion page"
effects = ["dispatch_capability", "network", "use_secret"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/notion/fetch.input.json"
output_schema_ref = "schemas/notion/fetch.output.json"
runtime_credentials = [
  { handle = "notion_access_token", source = { type = "product_auth_account", provider = "notion" }, audience = { scheme = "https", host_pattern = "mcp.notion.com" }, target = { type = "header", name = "authorization", prefix = "Bearer " }, required = true }
]
"#;

    #[test]
    fn discovered_mcp_tools_replace_provider_capabilities_with_inline_schemas() {
        let package = notion_package();
        let tools = vec![
            HostedMcpDiscoveredTool {
                name: "notion-search".to_string(),
                description: "Search live Notion tools".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                annotations: HostedMcpDiscoveredToolAnnotations {
                    read_only_hint: true,
                    ..Default::default()
                },
            },
            HostedMcpDiscoveredTool {
                name: "notion-create-pages".to_string(),
                description: "Create pages from live schema".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"pages": {"type": "array"}}
                }),
                annotations: HostedMcpDiscoveredToolAnnotations {
                    side_effects_hint: true,
                    ..Default::default()
                },
            },
        ];

        let discovered = package_with_discovered_hosted_mcp_tools(&package, &tools)
            .expect("build discovered package");

        assert!(
            discovered
                .capabilities
                .iter()
                .all(|capability| capability.id.as_str() != "notion.notion-fetch")
        );
        let search = discovered
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == "notion.notion-search")
            .expect("discovered search capability");
        assert_eq!(search.description, "Search live Notion tools");
        assert_eq!(search.runtime, RuntimeKind::Mcp);
        assert_eq!(
            search.parameters_schema,
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            })
        );
        assert_eq!(search.runtime_credentials.len(), 1);
        assert!(search.effects.contains(&EffectKind::UseSecret));
        assert!(!search.effects.contains(&EffectKind::ExternalWrite));
        let create_pages = discovered
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == "notion.notion-create-pages")
            .expect("discovered create-pages capability");
        assert!(create_pages.effects.contains(&EffectKind::ExternalWrite));
        assert_eq!(discovered.manifest.capabilities.len(), 2);
    }

    #[test]
    fn discovered_mcp_tools_reject_inconsistent_provider_templates() {
        let mut package = notion_package();
        let mut inconsistent = package.manifest.capabilities[0].clone();
        inconsistent.id = CapabilityId::new("notion.other").expect("valid capability id");
        inconsistent.runtime_credentials.clear();
        package.manifest.capabilities.push(inconsistent);
        let tools = vec![HostedMcpDiscoveredTool {
            name: "notion-search".to_string(),
            description: "Search live Notion tools".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: Default::default(),
        }];

        let error = package_with_discovered_hosted_mcp_tools(&package, &tools)
            .expect_err("inconsistent templates must fail discovery");

        assert!(
            error
                .to_string()
                .contains("inconsistent capability templates"),
            "unexpected error: {error}"
        );
    }

    fn loopback_package() -> ExtensionPackage {
        let mut manifest = notion_package().manifest;
        manifest.runtime = ExtensionRuntime::Mcp { transport: "http".into(), command: None, args: Vec::new(), url: Some("http://127.0.0.1:3002/mcp/internal".into()) };
        manifest.capabilities[0].default_permission = PermissionMode::Allow;
        manifest.capabilities[0].network_targets = vec![ironclaw_host_api::NetworkTargetPattern { scheme: Some(ironclaw_host_api::NetworkScheme::Http), host_pattern: "127.0.0.1".into(), port: Some(3002) }];
        manifest.capabilities[0].required_host_ports = vec![ironclaw_host_api::HostPortId::new("host.runtime.http_egress").unwrap()];
        ExtensionPackage::from_manifest(manifest, VirtualPath::new("/system/extensions/notion").unwrap()).unwrap()
    }

    fn tool(name: &str) -> HostedMcpDiscoveredTool {
        HostedMcpDiscoveredTool { name: name.into(), description: "Observed schema".into(), input_schema: serde_json::json!({"type":"object","properties":{"observed":{"type":"string"}}}), annotations: HostedMcpDiscoveredToolAnnotations { read_only_hint: true, ..Default::default() } }
    }

    #[test]
    fn loopback_projection_keeps_known_security_metadata_and_actual_parameters() {
        let mut package = loopback_package();
        let mut write = package.manifest.capabilities[0].clone();
        write.id = CapabilityId::new("notion.write").unwrap();write.default_permission = PermissionMode::Ask;write.effects.push(EffectKind::ExternalWrite);write.runtime_credentials.clear();
        package.manifest.capabilities.push(write);
        package = ExtensionPackage::from_manifest(package.manifest, package.root).unwrap();
        let mut other_schema=tool("write");other_schema.input_schema=serde_json::json!({"type":"object","properties":{"other":{"type":"integer"}}});
        let tools = vec![tool("notion-fetch"),other_schema];
        assert_eq!(package.manifest.capabilities[0].input_schema_ref,package.manifest.capabilities[1].input_schema_ref);
        let discovered = package_with_discovered_loopback_http_mcp_tools(&package,&tools,"http://127.0.0.1:3002/mcp/internal").unwrap();
        assert_eq!(discovered.manifest.capabilities,package.manifest.capabilities);
        for (cap,original) in discovered.capabilities.iter().zip(&package.capabilities) {
            let mut normalized = cap.clone();normalized.parameters_schema=original.parameters_schema.clone();
            assert_eq!(&normalized,original); // All present and future safety fields, not a hand-picked subset.
        }
        assert_eq!(discovered.capabilities[0].parameters_schema,tools[0].input_schema);
        assert_eq!(discovered.capabilities[1].parameters_schema,tools[1].input_schema);
        assert_ne!(discovered.capabilities[0].parameters_schema,discovered.capabilities[1].parameters_schema);
        assert_eq!(discovered.descriptor_schema_mode,crate::CapabilityDescriptorSchemaMode::InlineDynamic);
        let mut registry=crate::ExtensionRegistry::new();registry.insert(discovered.clone()).unwrap();
        assert_eq!(registry.get_capability(&CapabilityId::new("notion.notion-fetch").unwrap()).unwrap().parameters_schema,tools[0].input_schema);
        assert_eq!(registry.get_capability(&CapabilityId::new("notion.write").unwrap()).unwrap().parameters_schema,tools[1].input_schema);
        assert!(discovered.capabilities[1].effects.contains(&EffectKind::ExternalWrite));
        assert_eq!(discovered.manifest.capabilities[0].required_host_ports,package.manifest.capabilities[0].required_host_ports);
    }

    #[test]
    fn loopback_projection_unknown_ask_and_generic_https_stays_strict() {
        let package = loopback_package();let tools = vec![tool("notion-fetch"),tool("new-tool")];
        assert!(!is_hosted_http_mcp_package(&package));
        assert!(package_with_discovered_hosted_mcp_tools(&package,&tools).is_err());
        let discovered = package_with_discovered_loopback_http_mcp_tools(&package,&tools,"http://127.0.0.1:3002/mcp/internal").unwrap();
        let cap = discovered.capabilities.iter().find(|cap|cap.id.as_str()=="notion.new-tool").unwrap();assert_eq!(cap.default_permission,PermissionMode::Ask);
        assert_eq!(cap.network_targets,package.capabilities[0].network_targets);
        assert_eq!(discovered.manifest.capabilities[0].required_host_ports,package.manifest.capabilities[0].required_host_ports);
        assert!(is_hosted_http_mcp_package(&notion_package()));
        assert!(package_with_discovered_hosted_mcp_tools(&notion_package(),&tools).is_ok());
    }

    #[test]
    fn loopback_projection_rejects_foreign_source_url_and_bad_catalog() {
        let package=loopback_package();let endpoint="http://127.0.0.1:3002/mcp/internal";
        for url in ["http://127.1:3002/mcp/internal","http://localhost:3002/mcp/internal","http://127.0.0.1:3003/mcp/internal","http://127.0.0.1:3002/mcp/internal?x=1"] {
            assert!(!is_host_bundled_loopback_http_mcp_package(&package,url));
            let mut invalid=package.clone();
            invalid.manifest.runtime=ExtensionRuntime::Mcp {transport:"http".into(),command:None,args:Vec::new(),url:Some(url.into())};
            if url != "http://127.0.0.1:3003/mcp/internal" {assert!(!is_host_bundled_loopback_http_mcp_package(&invalid,url));}
        }
        let mut foreign=package.clone();foreign.manifest.source=ManifestSource::InstalledLocal;
        assert!(package_with_discovered_loopback_http_mcp_tools(&foreign,&[tool("new-tool")],endpoint).is_err());
        let mut bad=tool("new-tool");bad.input_schema=serde_json::json!([]);
        assert!(package_with_discovered_loopback_http_mcp_tools(&package,&[tool("notion-fetch"),bad],endpoint).is_err());
        assert!(package_with_discovered_loopback_http_mcp_tools(&package,&[tool("notion-fetch"),tool("new-tool"),tool("new-tool")],endpoint).is_err());
    }

    #[test]
    fn loopback_projection_rejects_nonempty_catalog_missing_a_declared_tool() {
        let mut package=loopback_package();let mut second=package.manifest.capabilities[0].clone();
        second.id=CapabilityId::new("notion.second").unwrap();package.manifest.capabilities.push(second);
        package=ExtensionPackage::from_manifest(package.manifest,package.root).unwrap();
        let endpoint="http://127.0.0.1:3002/mcp/internal";
        assert!(package_with_discovered_loopback_http_mcp_tools(&package,&[tool("notion-fetch")],endpoint).is_err());
        assert!(package_with_discovered_loopback_http_mcp_tools(&package,&[tool("new-tool")],endpoint).is_err());
        let complete=package_with_discovered_loopback_http_mcp_tools(&package,&[tool("notion-fetch"),tool("second"),tool("new-tool")],endpoint).unwrap();
        assert_eq!(complete.manifest.capabilities.len(),3);
        assert_eq!(complete.capabilities.iter().find(|cap|cap.id.as_str()=="notion.new-tool").unwrap().default_permission,PermissionMode::Ask);
    }

    fn notion_package() -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            NOTION_MANIFEST,
            ManifestSource::HostBundled,
            &HostPortCatalog::default(),
        )
        .expect("valid Notion manifest");
        ExtensionPackage::from_manifest(
            manifest,
            VirtualPath::new("/system/extensions/notion").expect("valid root"),
        )
        .expect("valid Notion package")
    }
}
