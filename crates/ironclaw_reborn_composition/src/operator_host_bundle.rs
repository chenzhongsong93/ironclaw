//! High-level trusted operator bundle. No Deserialize on authority-shaped output.
use std::{collections::BTreeSet,fs::{File,OpenOptions},io::Read,path::{Path,PathBuf}};
use serde::Deserialize;
use ironclaw_extensions::{ExtensionManifestRecord,ExtensionPackage,ManifestSource,ExtensionRuntime};
use ironclaw_host_api::{ExtensionId,TenantId,VirtualPath,NetworkScheme,NetworkTargetPattern,sha256_digest_token};
use ironclaw_product_workflow::{ProductWorkflowError,LifecyclePackageKind,LifecyclePackageRef};
use crate::extension_host::available_extensions::{AvailableExtensionAsset,AvailableExtensionAssetContent,
    AvailableExtensionPackage,AvailableExtensionCatalog,surface_kinds_from_manifest_record};
use crate::extension_host::host_api_contracts::product_extension_host_api_contract_registry;
fn error(code:&'static str)->ProductWorkflowError {ProductWorkflowError::InvalidBindingRequest{reason:code.into()}}
fn opaque_error(_:impl std::fmt::Display)->ProductWorkflowError {error("hostBundleValidationRejected")}
fn digest(bytes:&[u8])->String {sha256_digest_token(bytes).trim_start_matches("sha256:").to_owned()}
fn hash(value:&str)->bool {value.len()==64&&value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn relative(value:&str)->Result<&Path,ProductWorkflowError> {
    if value.is_empty()||value.contains(':')||value.contains('\\')||value.split('/').any(|p|p.is_empty()||p=="."||p=="..") {return Err(error("hostBundleAssetPathRejected"));}
    Ok(Path::new(value))
}
fn no_links(path:&Path)->Result<(),ProductWorkflowError> {
    for p in path.ancestors() {let m=std::fs::symlink_metadata(p).map_err(opaque_error)?;
        if m.file_type().is_symlink(){return Err(error("hostBundleLinkRejected"));}
        #[cfg(windows)] {use std::os::windows::fs::MetadataExt;if m.file_attributes()&0x400!=0 {return Err(error("hostBundleReparseRejected"));}}
    } Ok(())
}
fn pinned(path:&Path,sha:&str,max:u64,held:&mut Vec<File>)->Result<Vec<u8>,ProductWorkflowError> {
    if !hash(sha)||max==0||max>64*1024*1024 {return Err(error("hostBundlePinOrBudgetInvalid"));}
    no_links(path)?;let canonical=path.canonicalize().map_err(opaque_error)?;
    let mut options=OpenOptions::new();options.read(true);
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt;options.share_mode(1);}
    let mut file=options.open(&canonical).map_err(opaque_error)?;
    if file.metadata().map_err(opaque_error)?.len()>max {return Err(error("hostBundleMetadataBudgetExceeded"));}
    let mut bytes=Vec::new();(&mut file).take(max+1).read_to_end(&mut bytes).map_err(opaque_error)?;
    if bytes.len() as u64>max||digest(&bytes)!=sha {return Err(error("hostBundleOriginalBytesMismatch"));}
    if path.canonicalize().map_err(opaque_error)?!=canonical {return Err(error("hostBundlePathDrift"));}
    held.push(file);Ok(bytes)
}
#[derive(Deserialize)] #[serde(rename_all="camelCase",deny_unknown_fields)]
struct Pin {path:String,sha256:String}
#[derive(Deserialize)] #[serde(rename_all="camelCase",deny_unknown_fields)]
struct OperatorConfig {schema_version:String,runtime_tenant_id:String,release_root:String,release_manifest:Pin,max_metadata_bytes:u64}
#[derive(Deserialize)] #[serde(rename_all="camelCase",deny_unknown_fields)]
struct Release {schema_version:String,provider:String,version:String,manifest:Pin,assets:Vec<Pin>,mcp:Option<OperatorBindingRecord>}
#[derive(Deserialize)] #[serde(rename_all="camelCase",deny_unknown_fields)]
struct OperatorBindingRecord {endpoint:String,audience:String,actual_proofs:Option<Vec<ProofRule>>,api_native_sha256:String,api_source_sha256:String,requester_native_sha256:String,requester_source_sha256:String}
#[derive(Deserialize)] #[serde(rename_all="camelCase",deny_unknown_fields)]
struct ProofRule {kind:String,reference:Pin,json_pointer:String,schema_pointer:String,expected_schema:String}
fn verify_actual_api_proofs(rules:&[ProofRule],tenant:&TenantId,native_sha:&str,source_sha:&str,requester_native_sha:&str,requester_source_sha:&str,endpoint:&str,audience:&str,
    read:&dyn Fn(&Pin,&mut Vec<File>)->Result<Vec<u8>,ProductWorkflowError>,held:&mut Vec<File>)->Result<(),ProductWorkflowError> {
    const REQUIRED:&[&str]=&["nativeBuildExit","nativeStdoutEof","nativeStderrEof","nativeStdoutJoined","nativeStderrJoined",
        "nativeUnknown","rootToolExit","rootToolUnknown","nativeSha","sourceSha","apiNativeSha","apiSourceSha","apiPid","apiCreationIdentity","apiUnknown","apiReady","endpoint","audience","requesterNativeSha","requesterSourceSha","requesterNativeProtocol","requesterNativeExit","requesterNativeUnknown",
        "requesterResidentNativeSha","requesterResidentSourceSha","requesterResidentProtocol","requesterResidentPid","requesterResidentCreation","requesterResidentUnknown","requesterResidentReady"];
    if rules.len()!=REQUIRED.len()&&rules.len()!=REQUIRED.len()+1{return Err(error("hostBundleMandatoryCurrentApiProofMissing"));}
    if let (Some(holder),Some(snapshot))=(rules.iter().find(|r|r.kind=="requesterResidentNativeSha"),rules.iter().find(|r|r.kind=="requesterResidentProtocol")) {
        if holder.reference.path==snapshot.reference.path{return Err(error("hostBundleResidentOriginalsMustBeSeparate"));}
    }
    let mut seen=BTreeSet::new();
    for rule in rules {
        if (!REQUIRED.contains(&rule.kind.as_str())&&rule.kind!="apiTenant")||!seen.insert(rule.kind.as_str())||!rule.json_pointer.starts_with('/')
            ||!rule.schema_pointer.starts_with('/')||rule.expected_schema.is_empty() {return Err(error("hostBundleActualProofProjectionRejected"));}
        let value:serde_json::Value=serde_json::from_slice(&read(&rule.reference,held)?).map_err(opaque_error)?;
        if value.pointer(&rule.schema_pointer).and_then(|v|v.as_str())!=Some(rule.expected_schema.as_str()){return Err(error("hostBundleActualProofSchemaMismatch"));}
        let v=value.pointer(&rule.json_pointer).ok_or_else(||error("hostBundleActualProofFieldMissing"))?;
        let valid=match rule.kind.as_str() {
            "nativeBuildExit"|"rootToolExit"|"requesterNativeExit"=>v.as_i64()==Some(0),
            "nativeStdoutEof"|"nativeStderrEof"|"nativeStdoutJoined"|"nativeStderrJoined"|"apiReady"|"requesterResidentReady"=>v.as_bool()==Some(true),
            "nativeUnknown"|"rootToolUnknown"|"apiUnknown"|"requesterNativeUnknown"|"requesterResidentUnknown"=>v.as_bool()==Some(false),
            "nativeSha"|"apiNativeSha"=>v.as_str()==Some(native_sha),"sourceSha"|"apiSourceSha"=>v.as_str()==Some(source_sha),
            "endpoint"=>v.as_str()==Some(endpoint),"audience"=>v.as_str()==Some(audience),"apiTenant"=>v.as_str()==Some(tenant.as_str()),
            "requesterNativeProtocol"=>v.as_str()==Some("network-policy-deny-redirect/1")
                &&value.get("schema").and_then(|v|v.as_str())==Some("reborn.requester_protocol_export/1")
                &&value.get("native").and_then(|v|v.as_str())==Some(requester_native_sha)
                &&value.get("pkgVersion").and_then(|v|v.as_str()).is_some_and(|version|!version.is_empty()),
            "requesterResidentProtocol"=>v.as_str()==Some("network-policy-deny-redirect/1")
                &&value.get("schema").and_then(|v|v.as_str())==Some("reborn.requester_runtime_protocol/2")
                &&value.get("tenantId").and_then(|v|v.as_str())==Some(tenant.as_str())
                &&value.get("runtimeInstanceId").and_then(|v|v.as_str()).is_some_and(|id|uuid::Uuid::parse_str(id).is_ok_and(|uuid|uuid.get_version_num()==4&&!uuid.is_nil()&&uuid.to_string()==id))
                &&value.get("shutdownRequested").and_then(|v|v.as_bool())==Some(false)
                &&["native","source","pid","creation"].iter().all(|k|value.get(*k).is_none()),
            "requesterNativeSha"|"requesterResidentNativeSha"=>v.as_str()==Some(requester_native_sha),
            "requesterSourceSha"|"requesterResidentSourceSha"=>v.as_str()==Some(requester_source_sha),
            "apiPid"|"requesterResidentPid"=>v.as_u64().is_some_and(|p|p>0&&p<=u32::MAX as u64),
            "apiCreationIdentity"|"requesterResidentCreation"=>v.as_str().is_some_and(|s|!s.is_empty())||v.as_u64().is_some_and(|t|t>0),
            _=>false,
        };
        if !valid {return Err(error("hostBundleActualApiProofMismatch"));}
    }
    if REQUIRED.iter().any(|r|!seen.contains(r)){return Err(error("hostBundleMandatoryCurrentApiProofMissing"));}
    for group in [vec!["nativeBuildExit","nativeStdoutEof","nativeStderrEof","nativeStdoutJoined","nativeStderrJoined","nativeUnknown","nativeSha","sourceSha"],
        vec!["apiNativeSha","apiSourceSha","apiPid","apiCreationIdentity","apiUnknown","apiReady","endpoint","audience"],vec!["rootToolExit","rootToolUnknown"],
        vec!["requesterNativeSha","requesterSourceSha","requesterNativeExit","requesterNativeUnknown"],
        vec!["requesterResidentNativeSha","requesterResidentSourceSha","requesterResidentPid","requesterResidentCreation"],
        vec!["requesterResidentProtocol","requesterResidentUnknown","requesterResidentReady"]] {
        let first=rules.iter().find(|r|r.kind==group[0]).ok_or_else(||error("hostBundleActualProofGroupMissing"))?;
        if group.iter().any(|kind|rules.iter().find(|r|r.kind==*kind).is_none_or(|r|
            r.reference.path!=first.reference.path||r.reference.sha256!=first.reference.sha256||r.expected_schema!=first.expected_schema)) {
            return Err(error("hostBundleMixedOriginalApiProofRejected"));
        }
    }
    verify_resident_capture_join(rules,tenant,requester_native_sha,requester_source_sha,read,held)?;
    if let Some(tenant_rule)=rules.iter().find(|r|r.kind=="apiTenant") {
        let api=rules.iter().find(|r|r.kind=="apiNativeSha").ok_or_else(||error("hostBundleActualProofGroupMissing"))?;
        if tenant_rule.reference.path!=api.reference.path||tenant_rule.reference.sha256!=api.reference.sha256||tenant_rule.expected_schema!=api.expected_schema {
            return Err(error("hostBundleMixedOriginalApiProofRejected"));
        }
    }
    Ok(())
}
/// Two original buffers, linked only by facts emitted by the original live Popen/read thread owner.
fn verify_resident_capture_join(rules:&[ProofRule],tenant:&TenantId,native:&str,source:&str,
    read:&dyn Fn(&Pin,&mut Vec<File>)->Result<Vec<u8>,ProductWorkflowError>,held:&mut Vec<File>)->Result<(),ProductWorkflowError> {
    let holder=rules.iter().find(|r|r.kind=="requesterResidentNativeSha").ok_or_else(||error("hostBundleResidentHolderMissing"))?;
    let snapshot=rules.iter().find(|r|r.kind=="requesterResidentProtocol").ok_or_else(||error("hostBundleResidentSnapshotMissing"))?;
    if holder.reference.path==snapshot.reference.path {return Err(error("hostBundleResidentOriginalsMustBeSeparate"));}
    let identity:serde_json::Value=serde_json::from_slice(&read(&holder.reference,held)?).map_err(opaque_error)?;
    let snapshot_bytes=read(&snapshot.reference,held)?;
    let body:serde_json::Value=serde_json::from_slice(&snapshot_bytes).map_err(opaque_error)?;
    if identity.get("schema").and_then(|v|v.as_str())!=Some("root.owned-requester-resident-capture/1")
        ||body.get("schema").and_then(|v|v.as_str())!=Some("reborn.requester_runtime_protocol/2") {return Err(error("hostBundleResidentCaptureSchemaRejected"));}
    let capture=identity.get("residentCapture").ok_or_else(||error("hostBundleResidentCaptureMissing"))?;
    let id=body.get("runtimeInstanceId").and_then(|v|v.as_str()).ok_or_else(||error("hostBundleResidentInstanceMissing"))?;
    let mut framed=format!("IRONCLAW_RESIDENT_PROTOCOL_V2 {} ",snapshot_bytes.len()).into_bytes();framed.extend_from_slice(&snapshot_bytes);framed.push(b'\n');
    let frame_sha=digest(&framed);
    let offset=capture.get("offset").and_then(|v|v.as_u64()).ok_or_else(||error("hostBundleResidentOriginalCaptureMismatch"))?;
    let end=offset.checked_add(framed.len() as u64).ok_or_else(||error("hostBundleResidentOriginalCaptureMismatch"))?;
    let original=identity.get("originalOperation").ok_or_else(||error("hostBundleResidentOriginalCaptureMismatch"))?;
    if body.get("tenantId").and_then(|v|v.as_str())!=Some(tenant.as_str())||!uuid::Uuid::parse_str(id).is_ok_and(|uuid|uuid.get_version_num()==4&&!uuid.is_nil()&&uuid.to_string()==id)
        ||identity.get("native").and_then(|v|v.as_str())!=Some(native)||identity.get("source").and_then(|v|v.as_str())!=Some(source)
        ||identity.get("unknown").and_then(|v|v.as_bool())!=Some(false)
        ||capture.get("origin").and_then(|v|v.as_str())!=Some("originalOwnedStdoutReadThread")
        ||capture.get("snapshotSha256").and_then(|v|v.as_str())!=Some(snapshot.reference.sha256.as_str())
        ||capture.get("snapshotPath").and_then(|v|v.as_str())!=Some(snapshot.reference.path.as_str())
        ||capture.get("runtimeInstanceId").and_then(|v|v.as_str())!=Some(id)
        ||["captureComplete","ownerAlive","originalBudgetValid"].iter().any(|k|capture.get(*k).and_then(|v|v.as_bool())!=Some(true))
        ||snapshot_bytes.is_empty()||snapshot_bytes.len()>16*1024
        ||capture.get("payloadBytes").and_then(|v|v.as_u64())!=Some(snapshot_bytes.len() as u64)
        ||capture.get("frameBytes").and_then(|v|v.as_u64())!=Some(framed.len() as u64)
        ||capture.get("frameSha256").and_then(|v|v.as_str())!=Some(frame_sha.as_str())
        ||original.get("pid")!=identity.get("pid")||original.get("processCreationFiletime100ns")!=identity.get("creation")
        ||original.get("stdoutBytesRead").and_then(|v|v.as_u64()).is_none_or(|n|n<end) {return Err(error("hostBundleResidentOriginalCaptureMismatch"));}
    Ok(())
}
/// Root must supply the pin from its private installation configuration, never HTTP/model args.
pub struct RebornHostExtensionBundle {tenant_id:TenantId,packages:Vec<AvailableExtensionPackage>,bindings:Vec<OperatorOwnedMcpBinding>,_held:Vec<File>}
#[derive(Clone,Debug)]
pub(crate) struct OperatorOwnedMcpBinding {tenant_id:TenantId,provider:ExtensionId,version:String,manifest_digest:String,
    endpoint:String,audience:String,resident_runtime_instance_id:String,api_native_sha256:String,api_source_sha256:String,api_identity_sha256:String}
impl OperatorOwnedMcpBinding {
    pub(crate) fn tenant(&self)->&TenantId {&self.tenant_id}
    pub(crate) fn runtime_instance_id(&self)->&str {&self.resident_runtime_instance_id}
    pub(crate) fn audience(&self)->&str {&self.audience}
    pub(crate) fn provider(&self)->&ExtensionId {&self.provider}
    pub(crate) fn endpoint_for(&self,package:&ExtensionPackage,url:&str)->Option<&str> {
        if package.manifest.source!=ManifestSource::HostBundled||package.id!=self.provider
            ||package.manifest.version!=self.version||package.manifest_digest.as_deref()!=Some(self.manifest_digest.as_str())
            ||url!=self.endpoint||self.audience!=self.endpoint
            ||![&self.api_native_sha256,&self.api_source_sha256,&self.api_identity_sha256].into_iter().all(|s|hash(s)) {return None;}
        match &package.manifest.runtime {ExtensionRuntime::Mcp{transport,command:None,args,url:Some(endpoint)}
            if transport=="http"&&args.is_empty()&&endpoint==&self.endpoint=>Some(&self.endpoint),_=>None}
    }
}
pub(crate) fn exact_loopback_url(value:&str)->Result<String,ProductWorkflowError> {
    let parsed=url::Url::parse(value).map_err(opaque_error)?;
    if parsed.scheme()!="http"||parsed.host_str()!=Some("127.0.0.1")||parsed.port().is_none_or(|p|p==0)
        ||!parsed.username().is_empty()||parsed.password().is_some()||parsed.query().is_some()||parsed.fragment().is_some()
        ||parsed.as_str()!=value||parsed.path()=="/" {return Err(error("hostBundleExactLoopbackRequired"));}
    // Compare original authority; Url canonicalizes shortened or octal IP spellings.
    let authority=value.strip_prefix("http://").and_then(|v|v.split('/').next()).ok_or_else(||error("hostBundleExplicitAuthorityRequired"))?;
    let port=parsed.port().ok_or_else(||error("hostBundleExplicitPortRequired"))?;
    if authority!=format!("127.0.0.1:{port}") {return Err(error("hostBundleLiteralAddressRequired"));}
    Ok(value.into())
}
impl RebornHostExtensionBundle {
    pub fn load_operator_configuration(path:&Path,trusted_sha256:&str,max_config_bytes:u64)->Result<Self,ProductWorkflowError> {
        let mut held=Vec::new();let config:OperatorConfig=serde_json::from_slice(&pinned(path,trusted_sha256,max_config_bytes,&mut held)?).map_err(opaque_error)?;
        let tenant_id=TenantId::new(config.runtime_tenant_id.clone()).map_err(opaque_error)?;
        if config.schema_version!="reborn-host-bundle-operator/1" {return Err(error("hostBundleOperatorSchemaRejected"));}
        let root=PathBuf::from(config.release_root);if !root.is_absolute(){return Err(error("hostBundleRootAbsoluteRequired"));}
        no_links(&root)?;let root=root.canonicalize().map_err(opaque_error)?;
        let read=|pin:&Pin,held:&mut Vec<File>|->Result<Vec<u8>,ProductWorkflowError> {
            let path=root.join(relative(&pin.path)?);no_links(&path)?;
            if !path.canonicalize().map_err(opaque_error)?.starts_with(&root){return Err(error("hostBundleForeignRoot"));}
            pinned(&path,&pin.sha256,config.max_metadata_bytes,held)
        };
        let release:Release=serde_json::from_slice(&read(&config.release_manifest,&mut held)?).map_err(opaque_error)?;
        if release.schema_version!="reborn-host-bundle-release/1"||release.assets.len()>512 {return Err(error("hostBundleReleaseRejected"));}
        let toml=String::from_utf8(read(&release.manifest,&mut held)?).map_err(opaque_error)?;
        let ports=ironclaw_host_runtime::default_host_port_catalog().map_err(opaque_error)?;
        let contracts=product_extension_host_api_contract_registry().map_err(opaque_error)?;
        let record=ExtensionManifestRecord::from_toml_with_contracts(toml,ManifestSource::HostBundled,&ports,None,&contracts).map_err(opaque_error)?;
        if record.manifest().id.as_str()!=release.provider||record.manifest().version!=release.version {return Err(error("hostBundleProviderVersionMismatch"));}
        if release.manifest.path!="manifest.toml" {return Err(error("hostBundleCanonicalManifestPathRequired"));}
        let mut names=BTreeSet::new();names.insert(release.manifest.path.clone());
        let mut assets=vec![AvailableExtensionAsset {path:"manifest.toml".into(),content:AvailableExtensionAssetContent::Bytes(record.raw_toml().as_bytes().to_vec())}];
        let mut total=record.raw_toml().len();
        for pin in &release.assets {
            if !names.insert(pin.path.clone()){return Err(error("hostBundleDuplicateAsset"));}
            let bytes=read(pin,&mut held)?;total=total.checked_add(bytes.len()).ok_or_else(||error("hostBundleBudgetOverflow"))?;
            if total>64*1024*1024 {return Err(error("hostBundleAssetBudgetExceeded"));}
            assets.push(AvailableExtensionAsset{path:pin.path.clone(),content:AvailableExtensionAssetContent::Bytes(bytes)});
        }
        for capability in &record.manifest().capabilities {
            for required in [capability.input_schema_ref.as_str(),capability.output_schema_ref.as_str()] {
                if !names.contains(required){return Err(error("hostBundleMissingCapabilitySchema"));}
            }
            if let Some(path)=&capability.prompt_doc_ref {if !names.contains(path.as_str()){return Err(error("hostBundleMissingPromptDoc"));}}
        }
        let surface_kinds=surface_kinds_from_manifest_record(&record,&release.provider).map_err(opaque_error)?;
        let manifest=record.manifest().clone().try_into().map_err(opaque_error)?;
        let package=ExtensionPackage::from_manifest_toml(manifest,VirtualPath::new(format!("/system/extensions/{}",release.provider)).map_err(opaque_error)?,record.raw_toml()).map_err(opaque_error)?;
        let mut bindings=Vec::new();
        if release.mcp.is_none()&&matches!(&package.manifest.runtime,ExtensionRuntime::Mcp{url:Some(url),..} if url.starts_with("http://")) {
            return Err(error("hostBundleOperatorApiBindingMissing"));
        }
        if let Some(binding)=release.mcp {
            let endpoint=exact_loopback_url(&binding.endpoint)?;
            if binding.audience!=endpoint||!hash(&binding.api_native_sha256)||!hash(&binding.api_source_sha256)||!hash(&binding.requester_native_sha256)||!hash(&binding.requester_source_sha256) {
                return Err(error("hostBundleActualApiIdentityMissing"));
            }
            let proofs=binding.actual_proofs.as_ref().ok_or_else(||error("hostBundleMandatoryCurrentApiProofMissing"))?;
            verify_actual_api_proofs(proofs,&tenant_id,&binding.api_native_sha256,&binding.api_source_sha256,&binding.requester_native_sha256,&binding.requester_source_sha256,&endpoint,&binding.audience,&read,&mut held)?;
            let snapshot_rule=proofs.iter().find(|r|r.kind=="requesterResidentProtocol").ok_or_else(||error("hostBundleResidentSnapshotMissing"))?;
            let snapshot_body:serde_json::Value=serde_json::from_slice(&read(&snapshot_rule.reference,&mut held)?).map_err(opaque_error)?;
            let resident_runtime_instance_id=snapshot_body.get("runtimeInstanceId").and_then(|v|v.as_str()).ok_or_else(||error("hostBundleResidentInstanceMissing"))?.to_owned();
            let proof_digest=digest(&serde_json::to_vec(&proofs.iter().map(|r|(&r.kind,&r.reference.path,&r.reference.sha256,&r.json_pointer,&r.expected_schema)).collect::<Vec<_>>()).map_err(opaque_error)?);
            let port=url::Url::parse(&endpoint).map_err(opaque_error)?.port().ok_or_else(||error("hostBundlePortMissing"))?;
            let target=NetworkTargetPattern{scheme:Some(NetworkScheme::Http),host_pattern:"127.0.0.1".into(),port:Some(port)};
            if package.capabilities.iter().any(|c|c.provider!=package.id||c.network_targets!=vec![target.clone()]
                || c.runtime_credentials.iter().any(|r|!matches!(&r.target,ironclaw_host_api::RuntimeCredentialTarget::Header{..}))) {return Err(error("hostBundleManifestNetworkTargetMismatch"));}
            let b=OperatorOwnedMcpBinding{tenant_id:tenant_id.clone(),provider:package.id.clone(),version:package.manifest.version.clone(),
                manifest_digest:package.manifest_digest().ok_or_else(||error("hostBundleManifestDigestMissing"))?,endpoint:endpoint.clone(),audience:binding.audience,resident_runtime_instance_id,
                api_native_sha256:binding.api_native_sha256,api_source_sha256:binding.api_source_sha256,api_identity_sha256:proof_digest};
            if b.endpoint_for(&package,&endpoint).is_none(){return Err(error("hostBundleRuntimeEndpointMismatch"));}bindings.push(b);
        }
        let available=AvailableExtensionPackage{package_ref:LifecyclePackageRef::new(LifecyclePackageKind::Extension,&release.provider).map_err(opaque_error)?,
            manifest_toml:record.raw_toml().into(),source:ManifestSource::HostBundled,package,cleanup_requirements:Vec::new(),surface_kinds,assets};
        Ok(Self{tenant_id,packages:vec![available],bindings,_held:held})
    }
    pub(crate) fn tenant(&self)->&TenantId {&self.tenant_id}
    pub(crate) fn bindings(&self)->Vec<OperatorOwnedMcpBinding> {self.bindings.clone()}
    pub(crate) fn package_refs(&self)->Vec<LifecyclePackageRef> {self.packages.iter().map(|p|p.package_ref.clone()).collect()}

    pub(crate) fn merge_into_catalog(&self,catalog:&mut AvailableExtensionCatalog)->Result<(),ProductWorkflowError> {
        for package in &self.packages {
            if let Ok(existing)=catalog.resolve(&package.package_ref) {
                if !same_reopen_package(&existing,package) {return Err(error("hostBundleInstalledSourceDrift"));}
            }
        }
        catalog.extend(self.catalog());Ok(())
    }
    pub(crate) fn catalog(&self)->AvailableExtensionCatalog {
        let packages=self.packages.iter().map(|p|AvailableExtensionPackage {
            package_ref:p.package_ref.clone(),manifest_toml:p.manifest_toml.clone(),source:p.source,
            package:p.package.clone(),cleanup_requirements:p.cleanup_requirements.clone(),surface_kinds:p.surface_kinds.clone(),
            assets:p.assets.iter().map(|a|AvailableExtensionAsset {path:a.path.clone(),content:match &a.content {
                AvailableExtensionAssetContent::Bytes(bytes)=>AvailableExtensionAssetContent::Bytes(bytes.clone())}}).collect(),
        }).collect();AvailableExtensionCatalog::from_packages(packages)
    }
}
#[cfg(test)] #[path="operator_host_bundle_tests.rs"] mod tests;

/// Duplicate provider release selection fails closed, including a different digest/version.
pub(crate) fn validate_operator_bundle_set(bundles:&[RebornHostExtensionBundle])->Result<(),ProductWorkflowError> {
    if let Some(first)=bundles.first() {if bundles.iter().any(|b|b.tenant()!=first.tenant()){return Err(error("hostBundleMixedRuntimeTenantRejected"));}}
    let mut providers=BTreeSet::new();for b in bundles {for p in &b.packages {
        if !providers.insert(p.package.id.as_str()) {return Err(error("hostBundleDuplicateProvider"));}
    }} Ok(())
}

fn same_reopen_package(existing:&AvailableExtensionPackage,verified:&AvailableExtensionPackage)->bool {
    if existing.package_ref!=verified.package_ref||existing.manifest_toml!=verified.manifest_toml
        ||existing.package.root!=verified.package.root||existing.package.manifest.version!=verified.package.manifest.version
        ||existing.package.manifest_digest!=verified.package.manifest_digest
        ||existing.package.descriptor_schema_mode!=verified.package.descriptor_schema_mode
        ||existing.package.capabilities!=verified.package.capabilities {return false;}
    let assets=|p:&AvailableExtensionPackage|->Option<std::collections::BTreeMap<String,String>> {
        let mut set=std::collections::BTreeMap::new();for a in &p.assets {
            let AvailableExtensionAssetContent::Bytes(b)=&a.content;
            if set.insert(a.path.clone(),sha256_digest_token(b)).is_some(){return None;}
        }Some(set)
    };
    assets(existing).is_some_and(|a|assets(verified).is_some_and(|b|a==b))
}
pub(crate) fn validate_operator_bundle_refs(bundles:&[std::sync::Arc<RebornHostExtensionBundle>])->Result<(),ProductWorkflowError> {
    let mut providers=BTreeSet::new();for bundle in bundles {for p in &bundle.packages {
        if !providers.insert(p.package.id.as_str()){return Err(error("hostBundleDuplicateProvider"));}
    }}Ok(())
}

impl std::fmt::Debug for RebornHostExtensionBundle {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.debug_struct("RebornHostExtensionBundle").field("package_count",&self.packages.len())
            .field("binding_count",&self.bindings.len()).finish_non_exhaustive()
    }
}
#[cfg(test)] impl RebornHostExtensionBundle {
    pub(crate) fn opaque_pin_fixture(file:File)->Self {Self{tenant_id:TenantId::new("pin-fixture-tenant").unwrap(),packages:Vec::new(),bindings:Vec::new(),_held:vec![file]}}
}

pub(crate) fn validate_operator_runtime_tenant(bundles:&[std::sync::Arc<RebornHostExtensionBundle>],runtime_tenant:&TenantId)->Result<(),ProductWorkflowError> {
    if bundles.iter().any(|b|b.tenant()!=runtime_tenant||b.bindings.iter().any(|binding|binding.tenant()!=runtime_tenant)) {
        return Err(error("hostBundleValidatedRuntimeTenantMismatch"));
    }Ok(())
}

#[cfg(test)] impl OperatorOwnedMcpBinding {
    pub(crate) fn tenant_test_fixture(package:&ExtensionPackage,tenant:TenantId,endpoint:&str)->Self {
        Self {tenant_id:tenant,provider:package.id.clone(),version:package.manifest.version.clone(),manifest_digest:package.manifest_digest().unwrap(),
            endpoint:endpoint.into(),audience:endpoint.into(),resident_runtime_instance_id:"11111111-1111-4111-8111-111111111111".into(),api_native_sha256:"a".repeat(64),api_source_sha256:"b".repeat(64),api_identity_sha256:"c".repeat(64)}
    }
}

#[cfg(test)] impl RebornHostExtensionBundle {
    pub(crate) fn late_mechanism_fixture(available:AvailableExtensionPackage,tenant:TenantId,instance:&str)->Self {
        let endpoint=match &available.package.manifest.runtime {ExtensionRuntime::Mcp {url:Some(url),..}=>url.clone(),_=>panic!("MCP fixture")};
        let mut binding=OperatorOwnedMcpBinding::tenant_test_fixture(&available.package,tenant.clone(),&endpoint);binding.resident_runtime_instance_id=instance.into();
        Self {tenant_id:tenant,packages:vec![available],bindings:vec![binding],_held:Vec::new()}
    }
}
