use std::{collections::HashSet, path::Path};

use ironclaw_host_api::{
    HostApiError, MountAlias, MountGrant, MountPermissions, MountView, ResourceScope,
    SYSTEM_RESERVED_ID, VirtualPath,
};
use ironclaw_memory::MemoryDocumentScope;

pub(crate) const WORKSPACE_ALIAS: &str = "/workspace";
const WORKSPACE_TARGET: &str = "/projects/workspace";
const HOST_ALIAS: &str = "/host";
const HOST_TARGET: &str = "/projects/host";
const MEMORY_ALIAS: &str = "/memory";
const MEMORY_TARGET: &str = "/memory";

pub(crate) fn workspace_mount_view(
    permissions: MountPermissions,
    host_home_aliases: &[&Path],
) -> Result<MountView, HostApiError> {
    ambient_workspace_mount_view(permissions, &[], host_home_aliases)
}

/// Build the workspace mount view used by local-dev capability grants.
///
/// `workspace_aliases` is load-bearing for local-dev-yolo ambient coding tools:
/// callers must pass it only under a yolo runtime policy. Non-yolo local-dev
/// must pass an empty slice so raw host workspace paths stay denied.
pub(crate) fn ambient_workspace_mount_view(
    permissions: MountPermissions,
    workspace_aliases: &[&Path],
    host_home_aliases: &[&Path],
) -> Result<MountView, HostApiError> {
    let mut mounts = vec![grant(
        WORKSPACE_ALIAS,
        WORKSPACE_TARGET,
        permissions.clone(),
    )?];
    push_raw_alias_mounts(
        &mut mounts,
        workspace_aliases,
        WORKSPACE_TARGET,
        permissions.clone(),
        "workspace alias",
    )?;
    if !host_home_aliases.is_empty() {
        mounts.push(grant(HOST_ALIAS, HOST_TARGET, permissions.clone())?);
        push_raw_alias_mounts(
            &mut mounts,
            host_home_aliases,
            HOST_TARGET,
            permissions.clone(),
            "confirmed host-home alias",
        )?;
    }
    MountView::new(mounts)
}

/// Per-user workspace 挂载目标(虚拟路径,经 `/projects` mount 映射到物理
/// `{storage_root}/tenants/{tenant}/users/{user}/workspace`)。
/// 写入侧(agent capability grants)与读取侧(WebUI files API/浏览视图)
/// 必须共用同一 target,否则两侧物理目录错位。
fn scoped_workspace_target(tenant_id: &str, user_id: &str) -> String {
    format!("/projects/tenants/{tenant_id}/users/{user_id}/workspace")
}

/// Per-user workspace mount view(隔离:每 tenant/user 独享 workspace 目录)。
///
/// 仿 `scoped_skill_context_mount_view` 模式:grant `/workspace`(read_write)
/// → `/projects/tenants/{tenant}/users/{user}/workspace`。
/// 经 `/projects` mount(factory local_dev_project_filesystem)映射到物理
/// `{storage_root}/tenants/{tenant}/users/{user}/workspace`。
/// 用户 A 的 write_file/read_file 只能访问自己的 workspace,物理隔离防越权。
pub(crate) fn scoped_workspace_mount_view(
    tenant_id: &str,
    user_id: &str,
) -> Result<MountView, HostApiError> {
    MountView::new(vec![grant(
        WORKSPACE_ALIAS,
        &scoped_workspace_target(tenant_id, user_id),
        MountPermissions::read_write(),
    )?])
}

/// 与 agent 写入侧([`scoped_workspace_mount_view`])同一 target 的 scope 感知
/// workspace 视图:scope 带真实属主用户 → `/workspace` 映射到该用户私有目录;
/// 系统保留用户(无属主的系统线程)→ `fallback` 全局视图,保持 ambient 部署兼容。
/// WebUI 读取侧(files API / 附件)必须用本视图,否则读到 agent 从不写入的
/// 共享 `/projects/workspace`,列表恒空、读产物恒 404。
pub(crate) fn scope_aligned_workspace_mount_view(
    scope: &ResourceScope,
    fallback: &MountView,
    permissions: MountPermissions,
) -> Result<MountView, HostApiError> {
    if scope.user_id.as_str() == SYSTEM_RESERVED_ID {
        return Ok(fallback.clone());
    }
    MountView::new(vec![grant(
        WORKSPACE_ALIAS,
        &scoped_workspace_target(scope.tenant_id.as_str(), scope.user_id.as_str()),
        permissions,
    )?])
}

pub(crate) fn scoped_skill_context_mount_view(
    scope: &ResourceScope,
) -> Result<MountView, HostApiError> {
    MountView::new(vec![
        grant(
            "/skills",
            &format!(
                "/projects/tenants/{}/users/{}/skills",
                scope.tenant_id.as_str(),
                scope.user_id.as_str()
            ),
            MountPermissions::read_only(),
        )?,
        grant(
            "/tenant-shared/skills",
            "/projects/tenant-shared/skills",
            MountPermissions::read_only(),
        )?,
        grant(
            "/system/skills",
            "/projects/system/skills",
            MountPermissions::read_only(),
        )?,
    ])
}

pub(crate) fn skill_management_mount_view() -> Result<MountView, HostApiError> {
    MountView::new(vec![
        grant(
            "/skills",
            "/projects/skills",
            MountPermissions::read_write_list_delete(),
        )?,
        grant(
            "/system/skills",
            "/projects/system/skills",
            MountPermissions::read_only(),
        )?,
    ])
}

pub(crate) fn scoped_skill_management_mount_view(
    scope: &ResourceScope,
) -> Result<MountView, HostApiError> {
    MountView::new(vec![
        grant(
            "/skills",
            &format!(
                "/projects/tenants/{}/users/{}/skills",
                scope.tenant_id.as_str(),
                scope.user_id.as_str()
            ),
            MountPermissions::read_write_list_delete(),
        )?,
        grant(
            "/system/skills",
            "/projects/system/skills",
            MountPermissions::read_only(),
        )?,
    ])
}

pub(crate) fn memory_mount_view(permissions: MountPermissions) -> Result<MountView, HostApiError> {
    MountView::new(vec![grant(MEMORY_ALIAS, MEMORY_TARGET, permissions)?])
}

pub(crate) fn system_extensions_lifecycle_mount_view() -> Result<MountView, HostApiError> {
    MountView::new(vec![grant(
        "/system/extensions",
        "/system/extensions",
        MountPermissions::read_write_list_delete(),
    )?])
}

/// Read-only mount view backing the standalone WebUI filesystem viewer.
///
/// Spans every mount the read-only browser can navigate — the workspace
/// (project working files + landed attachments) and the persistent memory store
/// — over the same targets the agent's own tools resolve through, so the viewer
/// shows exactly what the agent sees. The workspace target is scope-aligned
/// (per-user when the scope carries a real owner, matching
/// [`scoped_workspace_mount_view`]; the ambient shared target only for
/// system-reserved scopes). Read-only by construction: the viewer is a
/// navigation + preview/download surface, never a write path. The aliases here
/// are the contract the browse reader confines against; keep them aligned with
/// [`BROWSE_MEMORY_ALIAS`]/[`WORKSPACE_ALIAS`].
pub(crate) const BROWSE_MEMORY_ALIAS: &str = MEMORY_ALIAS;

pub(crate) fn scoped_browse_mount_view(scope: &ResourceScope) -> Result<MountView, HostApiError> {
    let memory_target = scoped_memory_target(scope)?;
    let workspace_target = if scope.user_id.as_str() == SYSTEM_RESERVED_ID {
        WORKSPACE_TARGET.to_string()
    } else {
        scoped_workspace_target(scope.tenant_id.as_str(), scope.user_id.as_str())
    };
    MountView::new(vec![
        grant(
            WORKSPACE_ALIAS,
            workspace_target.as_str(),
            MountPermissions::read_only(),
        )?,
        grant(
            MEMORY_ALIAS,
            memory_target.as_str(),
            MountPermissions::read_only(),
        )?,
    ])
}

fn scoped_memory_target(scope: &ResourceScope) -> Result<VirtualPath, HostApiError> {
    MemoryDocumentScope::new_with_agent(
        scope.tenant_id.as_str(),
        scope.user_id.as_str(),
        scope.agent_id.as_ref().map(|id| id.as_str()),
        scope.project_id.as_ref().map(|id| id.as_str()),
    )?
    .virtual_prefix()
}

fn grant(
    alias: &str,
    target: &str,
    permissions: MountPermissions,
) -> Result<MountGrant, HostApiError> {
    Ok(MountGrant::new(
        MountAlias::new(alias)?,
        VirtualPath::new(target)?,
        permissions,
    ))
}

fn push_raw_alias_mounts(
    mounts: &mut Vec<MountGrant>,
    aliases: &[&Path],
    target: &str,
    permissions: MountPermissions,
    label: &str,
) -> Result<(), HostApiError> {
    let mut seen_aliases = mounts
        .iter()
        .map(|mount| mount.alias.as_str().to_string())
        .collect::<HashSet<_>>();
    for alias in aliases {
        let Some(alias) = alias.to_str() else {
            return Err(HostApiError::InvalidPath {
                value: format!("<non-utf8-{label}>"),
                reason: format!("{label} must be valid UTF-8"),
            });
        };
        let raw_alias = MountAlias::new(alias.to_string())?;
        if !seen_aliases.insert(raw_alias.as_str().to_string()) {
            continue;
        }
        mounts.push(MountGrant::new(
            raw_alias,
            VirtualPath::new(target)?,
            permissions.clone(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_scope(user_id: &str) -> ResourceScope {
        ResourceScope {
            tenant_id: ironclaw_host_api::TenantId::new("tenant-test").unwrap(),
            user_id: ironclaw_host_api::UserId::new(user_id).unwrap(),
            agent_id: Some(ironclaw_host_api::AgentId::new("agent-test").unwrap()),
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: ironclaw_host_api::InvocationId::new(),
        }
    }

    fn mount_target(view: &MountView, alias: &str) -> String {
        view.mounts
            .iter()
            .find(|mount| mount.alias.as_str() == alias)
            .unwrap_or_else(|| panic!("mount {alias} missing"))
            .target
            .as_str()
            .to_string()
    }

    #[test]
    fn scope_aligned_workspace_view_maps_real_user_to_per_user_target() {
        let fallback = workspace_mount_view(MountPermissions::read_only(), &[]).unwrap();
        let view = scope_aligned_workspace_mount_view(
            &user_scope("alice"),
            &fallback,
            MountPermissions::read_only(),
        )
        .expect("real-user scope resolves per-user workspace view");

        assert_eq!(
            mount_target(&view, WORKSPACE_ALIAS),
            "/projects/tenants/tenant-test/users/alice/workspace",
            "files API 读取侧必须与 agent 写入侧同一 per-user target"
        );
    }

    #[test]
    fn scope_aligned_workspace_view_falls_back_for_system_reserved_user() {
        let mut scope = user_scope("alice");
        scope.user_id = ironclaw_host_api::UserId::from_trusted(SYSTEM_RESERVED_ID.to_string());
        let fallback = workspace_mount_view(MountPermissions::read_only(), &[]).unwrap();
        let view =
            scope_aligned_workspace_mount_view(&scope, &fallback, MountPermissions::read_only())
                .expect("system-reserved scope falls back to ambient view");

        assert_eq!(
            mount_target(&view, WORKSPACE_ALIAS),
            WORKSPACE_TARGET,
            "无属主系统线程保持全局共享 workspace 视图"
        );
    }

    #[test]
    fn browse_view_workspace_target_is_scope_aligned() {
        let view = scoped_browse_mount_view(&user_scope("bob"))
            .expect("browse view resolves for real-user scope");

        assert_eq!(
            mount_target(&view, WORKSPACE_ALIAS),
            "/projects/tenants/tenant-test/users/bob/workspace",
            "独立浏览视图必须与 agent 可见的 per-user workspace 一致"
        );
    }

    #[test]
    fn ambient_workspace_mount_rejects_invalid_workspace_alias() {
        let err = ambient_workspace_mount_view(
            MountPermissions::read_write(),
            &[Path::new(r"C:\Users\alice\project")],
            &[],
        )
        .expect_err("invalid workspace alias should fail loudly");

        assert!(
            err.to_string().contains("backslashes are not allowed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn workspace_mount_rejects_host_home_alias_that_is_not_mount_shaped() {
        let err = workspace_mount_view(
            MountPermissions::read_write(),
            &[Path::new(r"C:\Users\alice")],
        )
        .expect_err("invalid raw alias should fail loudly");

        assert!(
            err.to_string().contains("backslashes are not allowed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ambient_workspace_mount_deduplicates_workspace_alias_against_canonical_workspace() {
        let mounts = ambient_workspace_mount_view(
            MountPermissions::read_write(),
            &[Path::new(WORKSPACE_ALIAS)],
            &[],
        )
        .expect("mount view builds");

        assert_eq!(
            mounts
                .mounts
                .iter()
                .filter(|mount| mount.alias.as_str() == WORKSPACE_ALIAS)
                .count(),
            1
        );
    }

    #[test]
    fn workspace_mount_deduplicates_normalized_host_home_aliases() {
        let mounts = workspace_mount_view(
            MountPermissions::read_write(),
            &[
                Path::new("/Users/alice"),
                Path::new("/Users/alice/"),
                Path::new("/Users/alice/."),
            ],
        )
        .expect("mount view builds");

        assert_eq!(
            mounts
                .mounts
                .iter()
                .filter(|mount| mount.alias.as_str() == "/Users/alice")
                .count(),
            1
        );
    }

    #[test]
    fn ambient_workspace_mount_includes_raw_workspace_alias() {
        let mounts = ambient_workspace_mount_view(
            MountPermissions::read_write(),
            &[Path::new("/Users/alice/project")],
            &[Path::new("/Users/alice")],
        )
        .expect("mount view builds");

        let mount_for = |alias: &str| {
            mounts
                .mounts
                .iter()
                .find(|mount| mount.alias.as_str() == alias)
                .unwrap_or_else(|| panic!("missing mount alias {alias}"))
        };
        assert_eq!(
            mount_for("/Users/alice/project").target.as_str(),
            WORKSPACE_TARGET
        );
        assert_eq!(mount_for("/Users/alice").target.as_str(), HOST_TARGET);
    }
}
