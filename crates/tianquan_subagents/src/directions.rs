//! 天权兼容 exporter：只绑定包身份和旧 kind，文件校验使用中性宿主加载器。

use std::path::Path;

use ironclaw_loop_host::{PinnedRoleBundle, PinnedRoleBundleSpec};

use crate::flavors::TIANQUAN_SOUL_KINDS;

pub use ironclaw_loop_host::PinnedRole as SoulRole;

#[derive(Debug, Clone)]
pub struct SoulBundle(PinnedRoleBundle);

impl SoulBundle {
    /// 在构造时读入并冻结包内容；缺失或修改任一文件即拒绝创建。
    pub fn load_from_dir(root: &Path) -> Result<Self, String> {
        PinnedRoleBundle::load_from_dir(
            root,
            PinnedRoleBundleSpec {
                schema_version: "soul-catalog/1",
                bundle_id: "novel-studio-souls",
                version: "1.0.0",
                marker: "SOUL-BUNDLE",
                expected_roles: TIANQUAN_SOUL_KINDS,
                max_material_bytes: 16 * 1024,
            },
        )
        .map(Self)
    }

    pub fn role(&self, kind: &str) -> Option<&SoulRole> {
        self.0.role(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let worldsmith = bundle.role("worldsmith").unwrap();
        assert!(worldsmith.allowed_capabilities.contains(
            &ironclaw_host_api::CapabilityId::new("tianquan-graph.run_world_patch").unwrap()
        ));
        assert!(
            !worldsmith
                .allowed_capabilities
                .contains(&ironclaw_host_api::CapabilityId::new("builtin.write_file").unwrap())
        );
    }
}
