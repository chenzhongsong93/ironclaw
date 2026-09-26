//! 从可信本地目录加载内容寻址的角色材料，构造后保持不可变快照。
//! 此模块只验证文件、版本与能力 ID，不解释任何产品角色或工具权限。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use ironclaw_host_api::CapabilityId;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_CATALOG_BYTES: usize = 256 * 1024;
const MAX_ROLE_CAPABILITIES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct PinnedRoleBundleSpec<'a> {
    pub schema_version: &'a str,
    pub bundle_id: &'a str,
    pub version: &'a str,
    pub marker: &'a str,
    pub expected_roles: &'a [&'a str],
    pub max_material_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct PinnedRole {
    pub direction_markdown: String,
    pub allowed_capabilities: BTreeSet<CapabilityId>,
}

#[derive(Debug, Clone)]
pub struct PinnedRoleBundle {
    roles: BTreeMap<String, PinnedRole>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Catalog {
    schema_version: String,
    bundle_id: String,
    version: String,
    common: MaterialEntry,
    roles: Vec<RoleEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterialEntry {
    path: String,
    sha256: String,
    bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoleEntry {
    kind: String,
    path: String,
    sha256: String,
    bytes: usize,
    allowed_capabilities: Vec<String>,
}

impl PinnedRoleBundle {
    pub fn load_from_dir(root: &Path, spec: PinnedRoleBundleSpec<'_>) -> Result<Self, String> {
        let expected = spec.expected_roles.iter().copied().collect::<BTreeSet<_>>();
        if spec.expected_roles.is_empty()
            || expected.len() != spec.expected_roles.len()
            || spec.max_material_bytes == 0
            || spec.expected_roles.iter().any(|role| !safe_role_kind(role))
        {
            return Err("invalid pinned role bundle specification".into());
        }
        let catalog_path = root.join("catalog.json");
        let catalog_bytes = fs::read(&catalog_path).map_err(|error| {
            format!(
                "role catalog unavailable at {}: {error}",
                catalog_path.display()
            )
        })?;
        if catalog_bytes.len() > MAX_CATALOG_BYTES {
            return Err("role catalog exceeds byte limit".into());
        }
        let catalog: Catalog = serde_json::from_slice(&catalog_bytes)
            .map_err(|error| format!("role catalog invalid: {error}"))?;
        if catalog.schema_version != spec.schema_version
            || catalog.bundle_id != spec.bundle_id
            || catalog.version != spec.version
        {
            return Err("role catalog identity/version mismatch".into());
        }
        let common = read_pinned(
            root,
            &catalog.common,
            "directions/common.md",
            spec.max_material_bytes,
        )?;
        let mut roles = BTreeMap::new();
        for entry in catalog.roles {
            if !expected.contains(entry.kind.as_str()) {
                return Err(format!("unexpected role kind: {}", entry.kind));
            }
            if entry.allowed_capabilities.len() > MAX_ROLE_CAPABILITIES {
                return Err(format!("too many role capabilities: {}", entry.kind));
            }
            let role_path = format!("directions/{}.md", entry.kind);
            let material = MaterialEntry {
                path: entry.path,
                sha256: entry.sha256,
                bytes: entry.bytes,
            };
            let role_text = read_pinned(root, &material, &role_path, spec.max_material_bytes)?;
            let mut capabilities = BTreeSet::new();
            for id in entry.allowed_capabilities {
                let capability = CapabilityId::new(id.as_str())
                    .map_err(|error| format!("invalid role capability {id}: {error}"))?;
                if !capabilities.insert(capability) {
                    return Err(format!("duplicate role capability: {id}"));
                }
            }
            let direction_markdown = format!(
                "{} {}/{} kind={} commonSha256={} roleSha256={}\n\n{}\n\n{}",
                spec.marker,
                catalog.bundle_id,
                catalog.version,
                entry.kind,
                catalog.common.sha256,
                material.sha256,
                common,
                role_text
            );
            if roles
                .insert(
                    entry.kind.clone(),
                    PinnedRole {
                        direction_markdown,
                        allowed_capabilities: capabilities,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate role kind: {}", entry.kind));
            }
        }
        if roles.len() != expected.len() {
            return Err(format!(
                "role catalog incomplete: {}/{}",
                roles.len(),
                expected.len()
            ));
        }
        Ok(Self { roles })
    }

    pub fn role(&self, kind: &str) -> Option<&PinnedRole> {
        self.roles.get(kind)
    }
}

fn safe_role_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn read_pinned(
    root: &Path,
    entry: &MaterialEntry,
    expected: &str,
    limit: usize,
) -> Result<String, String> {
    if entry.path != expected {
        return Err(format!("role material path mismatch: {}", entry.path));
    }
    if entry.bytes == 0 || entry.bytes > limit {
        return Err(format!("role material size outside bound: {expected}"));
    }
    let body = fs::read(root.join(expected))
        .map_err(|error| format!("role material unavailable {expected}: {error}"))?;
    let digest: String = Sha256::digest(&body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if body.len() != entry.bytes || digest != entry.sha256 {
        return Err(format!("role material hash/size mismatch: {expected}"));
    }
    String::from_utf8(body)
        .map_err(|error| format!("role material is not UTF-8 {expected}: {error}"))
}
