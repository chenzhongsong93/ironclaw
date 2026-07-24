use ironclaw_extensions::{CapabilityManifest, ExtensionError};
use ironclaw_host_api::{EffectKind, PermissionMode};

pub const SPAWN_SUBAGENT_CAPABILITY_ID: &str = "builtin.spawn_subagent";

pub(crate) fn manifest() -> Result<CapabilityManifest, ExtensionError> {
    super::first_party_capability_manifest(
        SPAWN_SUBAGENT_CAPABILITY_ID,
        "Authorize a scoped child subagent run",
        vec![EffectKind::DispatchCapability, EffectKind::SpawnProcess],
        // 天权定制(2026-07-24):Ask→Allow。Ask 抬升 approval gate 致 register/invoke 错位
        // (authorize() fold 中 spawn 路径在 register_provider_tool_call 填充 spawn_authorizations
        // map 前就执行 invoke_capability → authorize_spawn 查 map 为空 → spawn_requires_provider_registration)。
        // Allow 让 register 先于 invoke 完成 map 填充。local-dev 放行 spawn_subagent。
        PermissionMode::Allow,
        super::resource_profile(),
    )
}

pub(crate) fn dispatch() -> serde_json::Value {
    serde_json::json!({
        "authorized": true,
    })
}
