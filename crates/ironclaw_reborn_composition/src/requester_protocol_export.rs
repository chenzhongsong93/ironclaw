//! Early native support export. Missing compiled source receipt deliberately prevents qualification.
use std::io::Read;
use sha2::{Digest,Sha256};
fn lower_digest_hex(digest: &[u8]) -> String { hex::encode(digest) }
fn executable_sha()->Result<String,&'static str> {
    let path=std::env::current_exe().map_err(|_|"requesterExecutableIdentityUnavailable")?;
    let metadata=std::fs::symlink_metadata(&path).map_err(|_|"requesterExecutableIdentityUnavailable")?;
    if !metadata.is_file()||metadata.file_type().is_symlink() {return Err("requesterExecutableIdentityRejected");}
    #[cfg(windows)] {use std::os::windows::fs::MetadataExt;if metadata.file_attributes()&0x400!=0 {return Err("requesterExecutableIdentityRejected");}}
    let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt;options.share_mode(1);}
    let mut file=options.open(path).map_err(|_|"requesterExecutableIdentityUnavailable")?;
    const MAX:u64=1024*1024*1024;
    if file.metadata().map_err(|_|"requesterExecutableIdentityUnavailable")?.len()>MAX {return Err("requesterExecutableBudgetExceeded");}
    let mut hash=Sha256::new();let mut buffer=[0_u8;64*1024];let mut total=0_u64;
    loop {let len=file.read(&mut buffer).map_err(|_|"requesterExecutableReadFailed")?;if len==0 {break;}
        total=total.checked_add(len as u64).ok_or("requesterExecutableBudgetExceeded")?;
        if total>MAX {return Err("requesterExecutableBudgetExceeded");}hash.update(&buffer[..len]);}
    Ok(lower_digest_hex(&hash.finalize()))
}
fn export(native:&str,host_package_version:&'static str)->serde_json::Value {
    serde_json::json!({"schema":"reborn.requester_protocol_export/1",
        "protocol":crate::reborn_requester_egress_protocol_version(),"pkgVersion":host_package_version,
        "native":native,"source":null,"unknown":true,"qualified":false})
}
pub(crate) fn print(host_package_version:&'static str)->Result<(),&'static str> {
    if host_package_version.is_empty(){return Err("requesterPackageVersionMissing");}
    let body=export(&executable_sha()?,host_package_version);
    let output=serde_json::to_string(&body).map_err(|_|"requesterProtocolSerializationFailed")?;
    use std::io::Write;let stdout=std::io::stdout();let mut locked=stdout.lock();
    locked.write_all(output.as_bytes()).map_err(|_|"requesterProtocolOutputFailed")?;
    locked.write_all(b"\n").map_err(|_|"requesterProtocolOutputFailed")?;
    locked.flush().map_err(|_|"requesterProtocolOutputFailed")
}
#[cfg(test)] mod tests {
    #[test]
    fn actual_digest_encoder_matches_sha256_known_vectors() {
        use sha2::{Digest, Sha256};
        assert_eq!(super::lower_digest_hex(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(super::lower_digest_hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(super::lower_digest_hex(&[0, 15, 16, 255]), "000f10ff");
    }
    #[test] fn compiled_support_export_cannot_claim_source_or_runtime_qualification() {
        let v=super::export(&"a".repeat(64),"unit-fixture-only");assert_eq!(v["protocol"],"network-policy-deny-redirect/1");
        assert!(v["source"].is_null());assert_eq!(v["unknown"],true);assert_eq!(v["qualified"],false);
        assert!(v.get("pid").is_none()&&v.get("ready").is_none()&&v.get("tenant").is_none());
    }
}
