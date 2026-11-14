use ironclaw_filesystem::{LibSqlRootFilesystem, RootFilesystem};
use ironclaw_host_api::VirtualPath;
async fn fresh() -> (
    LibSqlRootFilesystem,
    std::sync::Arc<libsql::Database>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(
        libsql::Builder::new_local(dir.path().join("prefix.db"))
            .build()
            .await
            .unwrap(),
    );
    let fs = LibSqlRootFilesystem::new(db.clone());
    fs.run_migrations().await.unwrap();
    (fs, db, dir)
}
#[tokio::test]
async fn libsql_complete_prefix_atomic_budget_and_append_stable_generation() {
    let (fs, _db, _dir) = fresh().await;
    let path = VirtualPath::new("/owned/log").unwrap();
    let other = VirtualPath::new("/other/log").unwrap();
    assert!(
        fs.complete_event_prefix(&path, 128, 65536, 4096)
            .await
            .unwrap()
            .is_none()
    );
    let a = fs.append(&path, b"a".to_vec()).await.unwrap();
    fs.append(&other, b"other-path-gap".to_vec()).await.unwrap();
    let b = fs.append(&path, b"b".to_vec()).await.unwrap();
    assert!(b.get() > a.get() + 1);
    let first = fs
        .complete_event_prefix(&path, 128, 65536, 4096)
        .await
        .unwrap()
        .unwrap();
    let generation = first.generation().to_string();
    assert_eq!(first.head(), b);
    assert_eq!(first.into_records().len(), 2);
    assert!(
        fs.complete_event_prefix(&path, 1, 65536, 4096)
            .await
            .is_err()
    );
    assert!(fs.complete_event_prefix(&path, 128, 1, 4096).await.is_err());
    fs.append(&path, b"c".to_vec()).await.unwrap();
    assert_eq!(
        fs.event_log_generation(&path).await.unwrap(),
        Some(generation.clone())
    );
    assert_eq!(
        fs.complete_event_prefix(&path, 128, 65536, 4096)
            .await
            .unwrap()
            .unwrap()
            .generation(),
        generation
    );
}
#[tokio::test]
async fn libsql_wholepath_delete_generation_and_cold_reopen() {
    let (fs, db, _dir) = fresh().await;
    let path = VirtualPath::new("/owned/log").unwrap();
    fs.write_file(&path, b"anchor").await.unwrap();
    fs.append(&path, b"same-payload".to_vec()).await.unwrap();
    let old = fs.event_log_generation(&path).await.unwrap().unwrap();
    let cold = LibSqlRootFilesystem::new(db.clone());
    assert_eq!(
        cold.event_log_generation(&path).await.unwrap(),
        Some(old.clone())
    );
    assert!(
        cold.complete_event_prefix(&path, 128, 65536, 4096)
            .await
            .unwrap()
            .is_some()
    );
    fs.delete(&path).await.unwrap();
    assert!(
        cold.complete_event_prefix(&path, 128, 65536, 4096)
            .await
            .unwrap()
            .is_none()
    );
    fs.write_file(&path, b"new-anchor").await.unwrap();
    fs.append(&path, b"same-payload".to_vec()).await.unwrap();
    assert_ne!(
        cold.event_log_generation(&path).await.unwrap().unwrap(),
        old
    );
}
#[tokio::test]
async fn libsql_large_record_rejected_before_payload_transfer() {
    let (fs, _db, _dir) = fresh().await;
    let path = VirtualPath::new("/owned/log").unwrap();
    fs.append(&path, vec![9; 8192]).await.unwrap();
    assert!(
        fs.complete_event_prefix(&path, 128, 65536, 4096)
            .await
            .is_err()
    );
}
