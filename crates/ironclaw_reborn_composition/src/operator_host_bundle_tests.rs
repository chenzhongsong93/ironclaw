//! Distinct trust/source and URL path boundary; later integration caller test still required.
use super::*;
#[test] fn only_exact_literal_loopback_and_explicit_port_are_admitted() {
    assert!(exact_loopback_url("http://127.0.0.1:3002/mcp/internal").is_ok());
    for url in ["http://localhost:3002/mcp/internal","http://127.1:3002/mcp/internal","http://127.0.0.2:3002/mcp/internal",
        "http://127.0.0.1/mcp/internal","http://127.0.0.1:0/mcp/internal","http://127.0.0.1:3002/mcp/internal?x=1",
        "http://user@127.0.0.1:3002/mcp/internal","http://127.0.0.1:3002/mcp/internal#x","http://[::1]:3002/mcp/internal"] {
        assert!(exact_loopback_url(url).is_err(),"{url}");
    }
}
#[test] fn normalized_or_aliased_paths_do_not_broaden_binding() {
    assert!(exact_loopback_url("http://127.0.0.1:3002/x/../mcp/internal").is_err());
    let endpoint=exact_loopback_url("http://127.0.0.1:3002/mcp/internal").unwrap();
    assert_ne!(endpoint,"http://127.0.0.1:3002/mcp");
}
#[test] fn roots_and_asset_paths_cannot_upgrade_an_installed_directory() {
    for path in ["../manifest.toml","/manifest.toml","a//b","a/./b","C:/foreign","a\\b"] {assert!(relative(path).is_err());}
}

#[test] fn missing_current_api_proofs_cannot_be_replaced_by_historical_build_or_label() {
    let read=|_:&Pin,_:&mut Vec<std::fs::File>|->Result<Vec<u8>,ProductWorkflowError>{panic!("no body should be read with no evidence");};
    assert!(verify_actual_api_proofs(&[],&TenantId::new("pin-fixture-tenant").unwrap(),&"a".repeat(64),&"b".repeat(64),&"c".repeat(64),&"d".repeat(64),"http://127.0.0.1:3002/mcp/internal",
        "http://127.0.0.1:3002/mcp/internal",&read,&mut Vec::new()).is_err());
}
fn complete_proof_fixture()->(Vec<ProofRule>,std::collections::BTreeMap<String,Vec<u8>>) {
    let endpoint="http://127.0.0.1:3002/mcp/internal";
    let mut files=std::collections::BTreeMap::new();
    let native=serde_json::json!({"schema":"FixtureNative/1","exit":0,"outEof":true,"errEof":true,"outJoin":true,"errJoin":true,"unknown":false,"native":"a".repeat(64),"source":"b".repeat(64)});
    let api=serde_json::json!({"schema":"FixtureResident/1","native":"a".repeat(64),"source":"b".repeat(64),"pid":123,"creation":456,"unknown":false,"ready":true,"endpoint":endpoint,"audience":endpoint});
    files.insert("native.json".into(),serde_json::to_vec(&native).unwrap());files.insert("api.json".into(),serde_json::to_vec(&api).unwrap());
    files.insert("terminal.json".into(),br#"{"schema":"FixtureTerminal/1","exit":0,"unknown":false}"#.to_vec());
    files.insert("egress-native.json".into(),serde_json::to_vec(&serde_json::json!({"schema":"FixtureEgressNative/1","native":"c".repeat(64),"source":"d".repeat(64),"protocol":"network-policy-deny-redirect/1","exit":0,"unknown":false})).unwrap());
    let snapshot=serde_json::to_vec(&serde_json::json!({"schema":"reborn.requester_runtime_protocol/2","protocol":"network-policy-deny-redirect/1","tenantId":"pin-fixture-tenant","runtimeInstanceId":"11111111-1111-4111-8111-111111111111","shutdownRequested":false,"unknown":false,"ready":true})).unwrap();
    files.insert("snapshot.json".into(),snapshot.clone());
    let mut frame=format!("IRONCLAW_RESIDENT_PROTOCOL_V2 {} ",snapshot.len()).into_bytes();frame.extend_from_slice(&snapshot);frame.push(b'\n');
    files.insert("egress-resident.json".into(),serde_json::to_vec(&serde_json::json!({"schema":"root.owned-requester-resident-capture/1","native":"c".repeat(64),"source":"d".repeat(64),"pid":789,"creation":456,"unknown":false,
        "residentCapture":{"origin":"originalOwnedStdoutReadThread","snapshotPath":"snapshot.json","snapshotSha256":digest(&snapshot),"runtimeInstanceId":"11111111-1111-4111-8111-111111111111","captureComplete":true,"ownerAlive":true,"originalBudgetValid":true,"offset":0,"payloadBytes":snapshot.len(),"frameBytes":frame.len(),"frameSha256":digest(&frame)},"originalOperation":{"pid":789,"processCreationFiletime100ns":456,"stdoutBytesRead":frame.len()}})).unwrap());
    // Independent raw CLI buffer; source is independently pinned by the actual build receipt, not self asserted by CLI.
    files.insert("cli-native.json".into(),serde_json::to_vec(&serde_json::json!({"schema":"reborn.requester_protocol_export/1","native":"c".repeat(64),"protocol":"network-policy-deny-redirect/1","pkgVersion":"0.1.0","source":null,"unknown":true,"qualified":false})).unwrap());
    let specs=[("nativeBuildExit","native.json","/exit","FixtureNative/1"),("nativeStdoutEof","native.json","/outEof","FixtureNative/1"),
        ("nativeStderrEof","native.json","/errEof","FixtureNative/1"),("nativeStdoutJoined","native.json","/outJoin","FixtureNative/1"),
        ("nativeStderrJoined","native.json","/errJoin","FixtureNative/1"),("nativeUnknown","native.json","/unknown","FixtureNative/1"),
        ("nativeSha","native.json","/native","FixtureNative/1"),("sourceSha","native.json","/source","FixtureNative/1"),
        ("rootToolExit","terminal.json","/exit","FixtureTerminal/1"),("rootToolUnknown","terminal.json","/unknown","FixtureTerminal/1"),
        ("apiNativeSha","api.json","/native","FixtureResident/1"),("apiSourceSha","api.json","/source","FixtureResident/1"),
        ("apiPid","api.json","/pid","FixtureResident/1"),("apiCreationIdentity","api.json","/creation","FixtureResident/1"),
        ("apiUnknown","api.json","/unknown","FixtureResident/1"),("apiReady","api.json","/ready","FixtureResident/1"),
        ("endpoint","api.json","/endpoint","FixtureResident/1"),("audience","api.json","/audience","FixtureResident/1"),
        ("requesterNativeSha","egress-native.json","/native","FixtureEgressNative/1"),("requesterSourceSha","egress-native.json","/source","FixtureEgressNative/1"),
        ("requesterNativeProtocol","cli-native.json","/protocol","reborn.requester_protocol_export/1"),("requesterNativeExit","egress-native.json","/exit","FixtureEgressNative/1"),("requesterNativeUnknown","egress-native.json","/unknown","FixtureEgressNative/1"),
        ("requesterResidentNativeSha","egress-resident.json","/native","root.owned-requester-resident-capture/1"),("requesterResidentSourceSha","egress-resident.json","/source","root.owned-requester-resident-capture/1"),
        ("requesterResidentProtocol","snapshot.json","/protocol","reborn.requester_runtime_protocol/2"),("requesterResidentPid","egress-resident.json","/pid","root.owned-requester-resident-capture/1"),
        ("requesterResidentCreation","egress-resident.json","/creation","root.owned-requester-resident-capture/1"),("requesterResidentUnknown","snapshot.json","/unknown","reborn.requester_runtime_protocol/2"),("requesterResidentReady","snapshot.json","/ready","reborn.requester_runtime_protocol/2")];
    let rules=specs.into_iter().map(|(kind,path,pointer,schema)|ProofRule{kind:kind.into(),reference:Pin{path:path.into(),sha256:digest(&files[path])},
        json_pointer:pointer.into(),schema_pointer:"/schema".into(),expected_schema:schema.into()}).collect();(rules,files)
}
#[test] fn full_proof_set_then_single_schema_or_unknown_field_hits_exact_branch() {
    let (rules,mut files)=complete_proof_fixture();let tenant=TenantId::new("pin-fixture-tenant").unwrap();let endpoint="http://127.0.0.1:3002/mcp/internal";
    let verify=|files:&std::collections::BTreeMap<String,Vec<u8>>| {
        // Re-pin the mutated original buffer; only one semantic field changes, not cardinality or byte identity.
        let repinned:Vec<ProofRule>=rules.iter().map(|r|ProofRule {kind:r.kind.clone(),reference:Pin {path:r.reference.path.clone(),sha256:digest(&files[&r.reference.path])},json_pointer:r.json_pointer.clone(),schema_pointer:r.schema_pointer.clone(),expected_schema:r.expected_schema.clone()}).collect();
        let read=|p:&Pin,_:&mut Vec<std::fs::File>|->Result<Vec<u8>,ProductWorkflowError>{let bytes=files[&p.path].clone();assert_eq!(digest(&bytes),p.sha256);Ok(bytes)};
        verify_actual_api_proofs(&repinned,&tenant,&"a".repeat(64),&"b".repeat(64),&"c".repeat(64),&"d".repeat(64),endpoint,endpoint,&read,&mut Vec::new())
    };
    verify(&files).unwrap();let original=files["api.json"].clone();
    let mut body:serde_json::Value=serde_json::from_slice(&original).unwrap();body["schema"]=serde_json::json!("ForeignSchema/1");
    files.insert("api.json".into(),serde_json::to_vec(&body).unwrap());assert!(verify(&files).unwrap_err().to_string().contains("hostBundleActualProofSchemaMismatch"));
    let mut body:serde_json::Value=serde_json::from_slice(&original).unwrap();body["unknown"]=serde_json::json!(true);
    files.insert("api.json".into(),serde_json::to_vec(&body).unwrap());assert!(verify(&files).unwrap_err().to_string().contains("hostBundleActualApiProofMismatch"));
}
#[test] fn public_loader_errors_do_not_include_host_path_or_configuration_value() {
    let raw=std::io::Error::new(std::io::ErrorKind::PermissionDenied,"C:/private/operator-secret-config.toml");
    let message=opaque_error(raw).to_string();assert!(message.contains("hostBundleValidationRejected"));
    assert!(!message.contains("private")&&!message.contains("secret")&&!message.contains("C:/"));
}
#[test] fn production_input_and_planner_owner_retain_actual_file_pin_until_last_owner() {
    let path=std::env::temp_dir().join(format!("ic-bundle-hold-{}-{}",std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::write(&path,b"opaque test pin, not a release proof").unwrap();
    let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt;options.share_mode(1);}
    let bundle=RebornHostExtensionBundle::opaque_pin_fixture(options.open(&path).unwrap());
    let input=crate::RebornBuildInput::disabled("owner").with_deployment(crate::deployment::DeploymentConfig::production())
        .with_host_extension_bundles(vec![bundle]).unwrap();
    let weak=std::sync::Arc::downgrade(&input.host_extension_bundles[0]);
    let registry=std::sync::Arc::new(ironclaw_extensions::SharedExtensionRegistry::new(ironclaw_extensions::ExtensionRegistry::new()));
    let signer=ironclaw_mcp::McpTrustedContextSigner::new(ironclaw_host_api::ExtensionId::new("pin-fixture").unwrap(),
        b"unit-fixture-only-not-operator-proof".to_vec(),"http://127.0.0.1:3002/mcp/internal".into()).unwrap();
    let planner=crate::extension_host::mcp::RegistryMcpEgressPlanner::new_with_operator_mcp(registry,&input.host_extension_bundles,&signer,&TenantId::new("pin-fixture-tenant").unwrap()).unwrap();
    drop(input);assert!(weak.upgrade().is_some());
    #[cfg(windows)] assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
    drop(planner);assert!(weak.upgrade().is_none());
    #[cfg(windows)] assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
    std::fs::remove_file(path).unwrap();
}

#[test] fn reopen_compares_complete_asset_set_and_descriptor_projection() {
    let catalog=AvailableExtensionCatalog::from_first_party_assets().unwrap();
    let reference=LifecyclePackageRef::new(LifecyclePackageKind::Extension,"notion").unwrap();
    let original=catalog.resolve(&reference).unwrap();
    let clone_package=||AvailableExtensionPackage {package_ref:original.package_ref.clone(),manifest_toml:original.manifest_toml.clone(),
        source:ManifestSource::InstalledLocal,package:original.package.clone(),cleanup_requirements:original.cleanup_requirements.clone(),
        surface_kinds:original.surface_kinds.clone(),assets:original.assets.iter().map(|a|AvailableExtensionAsset{path:a.path.clone(),
            content:match &a.content {AvailableExtensionAssetContent::Bytes(b)=>AvailableExtensionAssetContent::Bytes(b.clone())}}).collect()};
    let mut discovered=clone_package();discovered.package.manifest.source=ManifestSource::InstalledLocal;
    assert!(same_reopen_package(&discovered,&original));
    discovered.assets.push(AvailableExtensionAsset {path:"unreviewed-extra.json".into(),content:AvailableExtensionAssetContent::Bytes(b"{}".to_vec())});
    assert!(!same_reopen_package(&discovered,&original));
    discovered=clone_package();if let Some(asset)=discovered.assets.first_mut() {
        let AvailableExtensionAssetContent::Bytes(bytes)=&mut asset.content;bytes.push(0);assert!(!same_reopen_package(&discovered,&original));
    }
    discovered=clone_package();discovered.package.manifest.version.push_str("-changed");assert!(!same_reopen_package(&discovered,&original));
    discovered=clone_package();discovered.package.capabilities.clear();assert!(!same_reopen_package(&discovered,&original));
}

#[test] fn bundle_set_is_bound_to_the_validated_runtime_tenant_not_installed_owner_label() {
    let path=std::env::temp_dir().join(format!("ic-tenant-bundle-{}",std::process::id()));std::fs::write(&path,b"fixture").unwrap();
    let bundles=vec![std::sync::Arc::new(RebornHostExtensionBundle::opaque_pin_fixture(std::fs::File::open(&path).unwrap()))];
    assert!(validate_operator_runtime_tenant(&bundles,&TenantId::new("pin-fixture-tenant").unwrap()).is_ok());
    assert!(validate_operator_runtime_tenant(&bundles,&TenantId::new("foreign-tenant").unwrap()).is_err());drop(bundles);std::fs::remove_file(path).unwrap();
}

#[test] fn actual_input_builder_rejects_bundle_for_a_foreign_explicit_runtime_identity() {
    let path=std::env::temp_dir().join(format!("ic-builder-tenant-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::write(&path,b"fixture").unwrap();
    let input=crate::RebornBuildInput::disabled("owner").with_deployment(crate::deployment::DeploymentConfig::production())
        .with_local_runtime_identity(TenantId::new("foreign-tenant").unwrap(),ironclaw_host_api::AgentId::new("fixture-agent").unwrap());
    let result=input.with_host_extension_bundles(vec![RebornHostExtensionBundle::opaque_pin_fixture(std::fs::File::open(&path).unwrap())]);
    assert!(result.err().unwrap().to_string().contains("hostBundleValidatedRuntimeTenantMismatch"));std::fs::remove_file(path).unwrap();
}

fn verify_fixture_semantics(rules:&[ProofRule],files:&std::collections::BTreeMap<String,Vec<u8>>)->Result<(),ProductWorkflowError> {
    let repinned:Vec<ProofRule>=rules.iter().map(|r|ProofRule {kind:r.kind.clone(),reference:Pin {path:r.reference.path.clone(),sha256:digest(&files[&r.reference.path])},
        json_pointer:r.json_pointer.clone(),schema_pointer:r.schema_pointer.clone(),expected_schema:r.expected_schema.clone()}).collect();
    let read=|p:&Pin,_:&mut Vec<File>| {let body=files[&p.path].clone();assert_eq!(digest(&body),p.sha256);Ok(body)};
    verify_actual_api_proofs(&repinned,&TenantId::new("pin-fixture-tenant").unwrap(),&"a".repeat(64),&"b".repeat(64),&"c".repeat(64),&"d".repeat(64),
        "http://127.0.0.1:3002/mcp/internal","http://127.0.0.1:3002/mcp/internal",&read,&mut Vec::new())
}
#[test] fn complete_thirty_original_rules_then_individual_native_resident_and_version_mutations() {
    let (rules,files)=complete_proof_fixture();assert_eq!(rules.len(),30);verify_fixture_semantics(&rules,&files).unwrap();
    let cases=[
        ("api.json","schema",serde_json::json!("Foreign/1"),"hostBundleActualProofSchemaMismatch"),
        ("api.json","unknown",serde_json::json!(true),"hostBundleActualApiProofMismatch"),
        ("cli-native.json","protocol",serde_json::json!("network-policy-deny-redirect/0"),"hostBundleActualApiProofMismatch"),
        ("snapshot.json","protocol",serde_json::json!("network-policy-deny-redirect/0"),"hostBundleActualApiProofMismatch"),
        ("cli-native.json","native",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("cli-native.json","pkgVersion",serde_json::json!(""),"hostBundleActualApiProofMismatch"),
        ("cli-native.json","schema",serde_json::json!("ForeignExporter/1"),"hostBundleActualProofSchemaMismatch"),
        ("egress-native.json","native",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("egress-native.json","source",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","native",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","source",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("egress-native.json","exit",serde_json::json!(1),"hostBundleActualApiProofMismatch"),
        ("egress-native.json","unknown",serde_json::json!(true),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","pid",serde_json::json!(0),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","creation",serde_json::json!(null),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","unknown",serde_json::json!(true),"hostBundleResidentOriginalCaptureMismatch"),
        ("snapshot.json","ready",serde_json::json!(false),"hostBundleActualApiProofMismatch"),
        ("native.json","native",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
        ("api.json","source",serde_json::json!("e".repeat(64)),"hostBundleActualApiProofMismatch"),
    ];
    for (path,field,value,code) in cases {
        let mut changed=files.clone();let mut body:serde_json::Value=serde_json::from_slice(&files[path]).unwrap();body[field]=value;
        changed.insert(path.into(),serde_json::to_vec(&body).unwrap());let failure=verify_fixture_semantics(&rules,&changed).unwrap_err().to_string();assert!(failure.contains(code),"field={field},code={code}");
    }
    // Legacy API18 was a full fixture once; it is insufficient for a Requester supporting the new protocol.
    let legacy:Vec<ProofRule>=rules.into_iter().filter(|r|!r.kind.starts_with("requester")).collect();assert_eq!(legacy.len(),18);
    assert!(verify_fixture_semantics(&legacy,&files).unwrap_err().to_string().contains("hostBundleMandatoryCurrentApiProofMissing"));
}
#[test] fn optional_api_tenant_must_match_runtime_tenant_and_same_api_original_buffer() {
    let (mut rules,mut files)=complete_proof_fixture();let mut api:serde_json::Value=serde_json::from_slice(&files["api.json"]).unwrap();api["tenant"]=serde_json::json!("pin-fixture-tenant");
    files.insert("api.json".into(),serde_json::to_vec(&api).unwrap());
    rules.push(ProofRule {kind:"apiTenant".into(),reference:Pin {path:"api.json".into(),sha256:digest(&files["api.json"])},json_pointer:"/tenant".into(),schema_pointer:"/schema".into(),expected_schema:"FixtureResident/1".into()});
    verify_fixture_semantics(&rules,&files).unwrap();api["tenant"]=serde_json::json!("foreign-tenant");files.insert("api.json".into(),serde_json::to_vec(&api).unwrap());
    assert!(verify_fixture_semantics(&rules,&files).unwrap_err().to_string().contains("hostBundleActualApiProofMismatch"));
    api["tenant"]=serde_json::json!("pin-fixture-tenant");files.insert("api.json".into(),serde_json::to_vec(&api).unwrap());
    files.insert("foreign-api.json".into(),files["api.json"].clone());rules.last_mut().unwrap().reference.path="foreign-api.json".into();
    assert!(verify_fixture_semantics(&rules,&files).unwrap_err().to_string().contains("hostBundleMixedOriginalApiProofRejected"));
}
#[test] fn real_public_loader_sanitizes_io_paths_and_json_configuration_values() {
    let root=std::env::temp_dir().join(format!("ic-real-loader-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir(&root).unwrap();let path=root.join("private-operator-config.json");
    let bytes=serde_json::to_vec(&serde_json::json!({"schemaVersion":"reborn-host-bundle-operator/1","runtimeTenantId":"pin-fixture-tenant",
        "releaseRoot":root.to_str().unwrap(),"releaseManifest":{"path":"private-secret-release.json","sha256":"a".repeat(64)},"maxMetadataBytes":4096})).unwrap();
    std::fs::write(&path,&bytes).unwrap();
    let result=RebornHostExtensionBundle::load_operator_configuration(&path,&digest(&bytes),4096).err().unwrap().to_string();
    assert!(result.contains("hostBundleValidationRejected"));for private in ["private-secret","private-operator",root.to_str().unwrap()] {assert!(!result.contains(private));}
    std::fs::write(&path,b"{untrusted-config-private-value}").unwrap();
    let result=RebornHostExtensionBundle::load_operator_configuration(&path,&digest(b"{untrusted-config-private-value}"),4096).err().unwrap().to_string();
    assert!(result.contains("hostBundleValidationRejected"));assert!(!result.contains("untrusted-config-private-value"));
    std::fs::remove_file(path).unwrap();std::fs::remove_dir(root).unwrap();
}

#[test] fn resident_originals_are_split_and_provenance_missing_mismatch_tenant_version_shutdown_reject() {
    let (rules,files)=complete_proof_fixture();verify_fixture_semantics(&rules,&files).unwrap();
    let cases=[("snapshot.json","tenantId",serde_json::json!("foreign-tenant"),"hostBundleActualApiProofMismatch"),
        ("snapshot.json","schema",serde_json::json!("reborn.requester_runtime_protocol/1"),"hostBundleActualProofSchemaMismatch"),
        ("snapshot.json","shutdownRequested",serde_json::json!(true),"hostBundleActualApiProofMismatch"),
        ("snapshot.json","unknown",serde_json::json!(true),"hostBundleActualApiProofMismatch"),
        ("snapshot.json","runtimeInstanceId",serde_json::json!("not-a-uuid"),"hostBundleActualApiProofMismatch"),
        ("egress-resident.json","residentCapture",serde_json::json!(null),"hostBundleResidentOriginalCaptureMismatch")];
    for (path,field,value,code) in cases {let mut changed=files.clone();let mut body:serde_json::Value=serde_json::from_slice(&files[path]).unwrap();body[field]=value;
        changed.insert(path.into(),serde_json::to_vec(&body).unwrap());assert!(verify_fixture_semantics(&rules,&changed).unwrap_err().to_string().contains(code),"field={field}");}
    for (field,value) in [("origin",serde_json::json!("httpHeader")),("snapshotSha256",serde_json::json!("e".repeat(64))),
        ("snapshotPath",serde_json::json!("arbitrary-ready.json")),("runtimeInstanceId",serde_json::json!("22222222-2222-4222-8222-222222222222")),
        ("captureComplete",serde_json::json!(false)),("ownerAlive",serde_json::json!(false)),("originalBudgetValid",serde_json::json!(false))] {
        let mut changed=files.clone();let mut body:serde_json::Value=serde_json::from_slice(&files["egress-resident.json"]).unwrap();body["residentCapture"][field]=value;
        changed.insert("egress-resident.json".into(),serde_json::to_vec(&body).unwrap());assert!(verify_fixture_semantics(&rules,&changed).unwrap_err().to_string().contains("hostBundleResidentOriginalCaptureMismatch"),"field={field}");}
    let (mut old_merged,files)=complete_proof_fixture();for rule in old_merged.iter_mut().filter(|r|["requesterResidentProtocol","requesterResidentUnknown","requesterResidentReady"].contains(&r.kind.as_str())) {
        rule.reference.path="egress-resident.json".into();rule.reference.sha256=digest(&files["egress-resident.json"]);rule.expected_schema="root.owned-requester-resident-capture/1".into();}
    assert!(verify_fixture_semantics(&old_merged,&files).unwrap_err().to_string().contains("hostBundleResidentOriginalsMustBeSeparate"));
}
