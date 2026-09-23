use std::fs;

use ironclaw_host_api::CapabilityId;
use ironclaw_loop_host::{PinnedRoleBundle, PinnedRoleBundleSpec};
use serde_json::json;
use sha2::{Digest, Sha256};

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("directions")).unwrap();
    let common = b"shared policy";
    let world = b"world method";
    fs::write(dir.path().join("directions/common.md"), common).unwrap();
    fs::write(dir.path().join("directions/worldsmith.md"), world).unwrap();
    let catalog = json!({
        "schemaVersion": "soul-catalog/1",
        "bundleId": "test-roles",
        "version": "1.0.0",
        "common": {"path": "directions/common.md", "sha256": hash(common), "bytes": common.len()},
        "roles": [{
            "kind": "worldsmith", "path": "directions/worldsmith.md",
            "sha256": hash(world), "bytes": world.len(),
            "allowedCapabilities": ["demo.allowed"]
        }]
    });
    fs::write(
        dir.path().join("catalog.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    dir
}

fn spec() -> PinnedRoleBundleSpec<'static> {
    PinnedRoleBundleSpec {
        schema_version: "soul-catalog/1",
        bundle_id: "test-roles",
        version: "1.0.0",
        marker: "SOUL-BUNDLE",
        expected_roles: &["worldsmith"],
        max_material_bytes: 16 * 1024,
    }
}

#[test]
fn loads_real_bytes_and_freezes_role_capabilities() {
    let fixture = fixture();
    let bundle = PinnedRoleBundle::load_from_dir(fixture.path(), spec()).unwrap();
    let role = bundle.role("worldsmith").unwrap();
    assert!(role.direction_markdown.contains("shared policy"));
    assert!(role.direction_markdown.contains("world method"));
    assert!(
        role.direction_markdown
            .contains("SOUL-BUNDLE test-roles/1.0.0")
    );
    assert!(
        role.allowed_capabilities
            .contains(&CapabilityId::new("demo.allowed").unwrap())
    );
    fs::write(fixture.path().join("directions/worldsmith.md"), "changed").unwrap();
    assert!(
        bundle
            .role("worldsmith")
            .unwrap()
            .direction_markdown
            .contains("world method")
    );
    assert!(PinnedRoleBundle::load_from_dir(fixture.path(), spec()).is_err());
}

#[test]
fn rejects_missing_roles_and_catalog_path_escape() {
    let fixture = fixture();
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path().join("catalog.json")).unwrap()).unwrap();
    catalog["roles"][0]["path"] = json!("../other.md");
    fs::write(
        fixture.path().join("catalog.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    assert!(PinnedRoleBundle::load_from_dir(fixture.path(), spec()).is_err());

    let second = crate::fixture();
    let catalog_path = second.path().join("catalog.json");
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["roles"] = json!([]);
    fs::write(catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(
        PinnedRoleBundle::load_from_dir(second.path(), spec())
            .unwrap_err()
            .contains("incomplete")
    );
}

#[test]
fn rejects_duplicate_capability_and_wrong_bundle_identity() {
    let fixture = fixture();
    let catalog_path = fixture.path().join("catalog.json");
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["roles"][0]["allowedCapabilities"] = json!(["demo.allowed", "demo.allowed"]);
    fs::write(&catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(
        PinnedRoleBundle::load_from_dir(fixture.path(), spec())
            .unwrap_err()
            .contains("duplicate role capability")
    );
    catalog["roles"][0]["allowedCapabilities"] = json!(["demo.allowed"]);
    catalog["bundleId"] = json!("different-product");
    fs::write(catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(
        PinnedRoleBundle::load_from_dir(fixture.path(), spec())
            .unwrap_err()
            .contains("identity/version mismatch")
    );
}

#[test]
fn rejects_unsafe_role_kind_even_when_catalog_and_spec_agree() {
    let fixture = fixture();
    let catalog_path = fixture.path().join("catalog.json");
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["roles"][0]["kind"] = json!("../outside");
    catalog["roles"][0]["path"] = json!("directions/../outside.md");
    fs::write(fixture.path().join("outside.md"), b"world method").unwrap();
    fs::write(catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let unsafe_spec = PinnedRoleBundleSpec {
        expected_roles: &["../outside"],
        ..spec()
    };
    assert!(PinnedRoleBundle::load_from_dir(fixture.path(), unsafe_spec).is_err());
}
