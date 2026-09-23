//! 兼容 exporter：读取天权仓的版本化 SOUL 包并在构造时冻结内容。
//! 平台二进制不再内嵌天权方法论；缺包、缺角色或哈希不符时拒绝注入。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::flavors::TIANQUAN_SOUL_KINDS;

const MAX_MATERIAL_BYTES: usize = 16 * 1024;

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

#[derive(Debug, Clone)]
pub struct SoulRole {
    pub direction_markdown: String,
    pub allowed_capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct SoulBundle {
    roles: BTreeMap<String, SoulRole>,
}

impl SoulBundle {
    /// 从可信安装目录一次性加载快照；在途 child 不受文件热更新影响。
    pub fn load_from_dir(root: &Path) -> Result<Self, String> {
        let catalog_path = root.join("catalog.json");
        let catalog_bytes = fs::read(&catalog_path).map_err(|error| {
            format!(
                "SOUL catalog unavailable at {}: {error}",
                catalog_path.display()
            )
        })?;
        let catalog: Catalog = serde_json::from_slice(&catalog_bytes)
            .map_err(|error| format!("SOUL catalog invalid: {error}"))?;
        if catalog.schema_version != "soul-catalog/1"
            || catalog.bundle_id != "novel-studio-souls"
            || catalog.version != "1.0.0"
        {
            return Err("SOUL catalog identity/version mismatch".into());
        }
        let common = read_pinned(root, &catalog.common, "directions/common.md")?;
        let mut roles = BTreeMap::new();
        for entry in catalog.roles {
            if !TIANQUAN_SOUL_KINDS.contains(&entry.kind.as_str()) {
                return Err(format!("unknown SOUL kind in catalog: {}", entry.kind));
            }
            let expected_path = format!("directions/{}.md", entry.kind);
            let material = MaterialEntry {
                path: entry.path,
                sha256: entry.sha256,
                bytes: entry.bytes,
            };
            let role_text = read_pinned(root, &material, &expected_path)?;
            let allowed_capabilities = entry.allowed_capabilities.into_iter().collect();
            let direction_markdown = format!(
                "SOUL-BUNDLE {}/{} kind={} commonSha256={} roleSha256={}\n\n{}\n\n{}",
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
                    SoulRole {
                        direction_markdown,
                        allowed_capabilities,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate SOUL kind: {}", entry.kind));
            }
        }
        if roles.len() != TIANQUAN_SOUL_KINDS.len() {
            return Err(format!(
                "SOUL catalog incomplete: {}/{} roles",
                roles.len(),
                TIANQUAN_SOUL_KINDS.len()
            ));
        }
        Ok(Self { roles })
    }

    pub fn role(&self, kind: &str) -> Option<&SoulRole> {
        self.roles.get(kind)
    }
}

fn read_pinned(root: &Path, entry: &MaterialEntry, expected_path: &str) -> Result<String, String> {
    if entry.path != expected_path {
        return Err(format!("SOUL material path mismatch: {}", entry.path));
    }
    if entry.bytes == 0 || entry.bytes > MAX_MATERIAL_BYTES {
        return Err(format!("SOUL material size outside bound: {expected_path}"));
    }
    let body = fs::read(root.join(expected_path))
        .map_err(|error| format!("SOUL material unavailable {expected_path}: {error}"))?;
    let digest = sha256_hex(&body);
    if body.len() != entry.bytes || digest != entry.sha256 {
        return Err(format!("SOUL material hash/size mismatch: {expected_path}"));
    }
    String::from_utf8(body)
        .map_err(|error| format!("SOUL material is not UTF-8 {expected_path}: {error}"))
}

fn sha256_hex(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("directions")).unwrap();
        let common = b"common contract";
        fs::write(root.path().join("directions/common.md"), common).unwrap();
        let roles: Vec<_> = TIANQUAN_SOUL_KINDS
            .iter()
            .map(|kind| {
                let text = format!("role contract for {kind}");
                fs::write(
                    root.path().join(format!("directions/{kind}.md")),
                    text.as_bytes(),
                )
                .unwrap();
                json!({
                    "kind": kind,
                    "path": format!("directions/{kind}.md"),
                    "sha256": sha256_hex(text.as_bytes()),
                    "bytes": text.len(),
                    "allowedCapabilities": []
                })
            })
            .collect();
        let catalog = json!({
            "schemaVersion": "soul-catalog/1",
            "bundleId": "novel-studio-souls",
            "version": "1.0.0",
            "common": {
                "path": "directions/common.md",
                "sha256": sha256_hex(common),
                "bytes": common.len()
            },
            "roles": roles
        });
        fs::write(
            root.path().join("catalog.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        root
    }

    #[test]
    fn missing_bundle_fails_closed() {
        let missing = tempfile::tempdir().unwrap();
        assert!(SoulBundle::load_from_dir(missing.path()).is_err());
    }

    #[test]
    fn unknown_kind_is_never_resolved() {
        assert!(!TIANQUAN_SOUL_KINDS.contains(&"general"));
    }

    #[test]
    fn loads_all_roles_with_hash_evidence_and_freezes_snapshot() {
        let root = fixture();
        let bundle = SoulBundle::load_from_dir(root.path()).unwrap();
        let novelist = bundle.role("novelist").unwrap();
        assert!(
            novelist
                .direction_markdown
                .contains("SOUL-BUNDLE novel-studio-souls/1.0.0 kind=novelist")
        );
        assert!(novelist.direction_markdown.contains("common contract"));
        assert!(
            novelist
                .direction_markdown
                .contains("role contract for novelist")
        );
        fs::write(root.path().join("directions/novelist.md"), "tampered").unwrap();
        assert!(
            bundle
                .role("novelist")
                .unwrap()
                .direction_markdown
                .contains("role contract for novelist")
        );
        assert!(
            SoulBundle::load_from_dir(root.path())
                .unwrap_err()
                .contains("hash/size mismatch")
        );
    }

    #[test]
    fn path_traversal_in_catalog_is_rejected() {
        let root = fixture();
        let catalog_path = root.path().join("catalog.json");
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
        catalog["roles"][0]["path"] = json!("../outside.md");
        fs::write(catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        assert!(
            SoulBundle::load_from_dir(root.path())
                .unwrap_err()
                .contains("path mismatch")
        );
    }

    #[test]
    #[ignore = "requires TIANQUAN_SOUL_TEST_BUNDLE_DIR pointing to the sibling TianQuan worktree"]
    fn real_tianquan_bundle_matches_catalog() {
        let root = std::env::var("TIANQUAN_SOUL_TEST_BUNDLE_DIR").unwrap();
        let bundle = SoulBundle::load_from_dir(Path::new(&root)).unwrap();
        assert!(
            bundle
                .role("novelist")
                .unwrap()
                .direction_markdown
                .contains("小说正文创作")
        );
    }
}
