//! TianQuan product capability policy.
//!
//! TianQuan's IronClaw instance is a fiction/world-building agent, not a coding
//! agent. This policy is compiled into the composition crate: no environment
//! variable can re-enable coding capabilities for the TianQuan tenant.

use ironclaw_host_api::{CapabilityId, CapabilitySet, EffectKind, RuntimeKind, TenantId};
use ironclaw_host_runtime::CapabilitySurfacePolicy;

pub(crate) const TIANQUAN_TENANT_ID: &str = "tianquan";

const CODING_CAPABILITIES: &[&str] = &[
    "builtin.read_file",
    "builtin.write_file",
    "builtin.list_dir",
    "builtin.glob",
    "builtin.grep",
    "builtin.apply_patch",
    "builtin.shell",
];

pub(crate) fn is_tianquan_tenant(tenant_id: &TenantId) -> bool {
    tenant_id.as_str() == TIANQUAN_TENANT_ID
}

pub(crate) fn is_tianquan_coding_capability(capability_id: &str) -> bool {
    CODING_CAPABILITIES.contains(&capability_id)
}

pub(crate) fn tianquan_surface_policy() -> CapabilitySurfacePolicy {
    CapabilitySurfacePolicy {
        allowed_runtimes: vec![RuntimeKind::Mcp, RuntimeKind::FirstParty],
        allowed_effects: vec![
            EffectKind::DispatchCapability,
            EffectKind::SpawnProcess,
            EffectKind::Network,
        ],
        include_requires_approval: true,
        denied_capabilities: CODING_CAPABILITIES
            .iter()
            .map(|id| CapabilityId::new(*id).expect("static capability id"))
            .collect(),
        max_capabilities: None,
    }
}

pub(crate) fn filter_tianquan_grants(tenant_id: &TenantId, grants: &mut CapabilitySet) {
    if !is_tianquan_tenant(tenant_id) {
        return;
    }
    grants
        .grants
        .retain(|grant| !is_tianquan_coding_capability(grant.capability.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{CapabilityGrant, CapabilityGrantId, Principal};

    #[test]
    fn tianquan_coding_capabilities_are_fixed_in_code() {
        for capability in CODING_CAPABILITIES {
            assert!(is_tianquan_coding_capability(capability));
            assert!(tianquan_surface_policy()
                .denied_capabilities
                .iter()
                .any(|denied| denied.as_str() == *capability));
        }
        assert!(!is_tianquan_coding_capability("tianquan-graph.tools"));
    }

    #[test]
    fn grant_filter_removes_coding_only_for_tianquan_tenant() {
        // The production grant path is covered through local_dev composition;
        // this module-level test only locks the deny set and tenant predicate.
        assert!(is_tianquan_tenant(&TenantId::new(TIANQUAN_TENANT_ID).expect("tenant id")));
        assert!(!is_tianquan_tenant(&TenantId::new("other-tenant").expect("tenant id")));
    }
}
