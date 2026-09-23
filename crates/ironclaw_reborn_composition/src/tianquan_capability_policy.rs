//! TianQuan product capability policy.
//!
//! TianQuan's IronClaw instance is a fiction/world-building agent, not a coding
//! agent. This policy is compiled into the composition crate: no environment
//! variable can re-enable coding capabilities for the TianQuan tenant.

use std::collections::BTreeMap;

use ironclaw_host_api::{CapabilityId, CapabilitySet, EffectKind, RuntimeKind, TenantId};
use ironclaw_host_runtime::{
    CapabilitySurfacePolicy, TODO_READ_CAPABILITY_ID, TODO_WRITE_CAPABILITY_ID,
};

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
        // 仅封闭字段的任务读写可通过文件效果可见性过滤；其余能力仍受原策略约束。
        allowed_effect_overrides: [
            (
                TODO_READ_CAPABILITY_ID,
                vec![EffectKind::DispatchCapability, EffectKind::ReadFilesystem],
            ),
            (
                TODO_WRITE_CAPABILITY_ID,
                vec![
                    EffectKind::DispatchCapability,
                    EffectKind::ReadFilesystem,
                    EffectKind::WriteFilesystem,
                ],
            ),
        ]
        .into_iter()
        .filter_map(|(id, effects)| CapabilityId::new(id).ok().map(|id| (id, effects)))
        .collect::<BTreeMap<_, _>>(),
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

    #[test]
    fn tianquan_coding_capabilities_are_fixed_in_code() {
        for capability in CODING_CAPABILITIES {
            assert!(is_tianquan_coding_capability(capability));
            assert!(
                tianquan_surface_policy()
                    .denied_capabilities
                    .iter()
                    .any(|denied| denied.as_str() == *capability)
            );
        }
        assert!(!is_tianquan_coding_capability("tianquan-graph.tools"));
    }

    #[test]
    fn todo_effect_override_only_advertises_scoped_task_tools() {
        let policy = tianquan_surface_policy();
        assert!(!policy.allowed_effects.contains(&EffectKind::ReadFilesystem));
        assert!(
            !policy
                .allowed_effects
                .contains(&EffectKind::WriteFilesystem)
        );
        assert_eq!(policy.allowed_effect_overrides.len(), 2);
        let read = CapabilityId::new(TODO_READ_CAPABILITY_ID).unwrap();
        let write = CapabilityId::new(TODO_WRITE_CAPABILITY_ID).unwrap();
        assert_eq!(
            policy.allowed_effect_overrides.get(&read),
            Some(&vec![
                EffectKind::DispatchCapability,
                EffectKind::ReadFilesystem
            ]),
        );
        assert!(
            policy
                .allowed_effect_overrides
                .get(&write)
                .is_some_and(|effects| effects.contains(&EffectKind::WriteFilesystem))
        );
        assert!(
            policy
                .denied_capabilities
                .iter()
                .any(|id| id.as_str() == "builtin.read_file")
        );
        assert!(
            !policy
                .allowed_effect_overrides
                .contains_key(&CapabilityId::new("builtin.read_file").unwrap())
        );
    }
    #[test]
    fn grant_filter_removes_coding_only_for_tianquan_tenant() {
        // The production grant path is covered through local_dev composition;
        // this module-level test only locks the deny set and tenant predicate.
        assert!(is_tianquan_tenant(
            &TenantId::new(TIANQUAN_TENANT_ID).expect("tenant id")
        ));
        assert!(!is_tianquan_tenant(
            &TenantId::new("other-tenant").expect("tenant id")
        ));
    }
}
