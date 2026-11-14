//! Root-admitted real storage phase. Two separate native invocations, never an in-process reopen claim.
use ironclaw_filesystem::{
    FilesystemError, FilesystemOperation, LibSqlRootFilesystem, PostgresRootFilesystem,
    RootFilesystem,
};
use ironclaw_host_api::VirtualPath;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, sync::Arc};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("required Root-owned setting missing: {name}"))
}
fn case_dir() -> PathBuf {
    let path = std::fs::canonicalize(required("TQ_OWNED_PREFIX_CASE_DIR")).expect("case exists");
    let root = std::fs::canonicalize("C:/Users/89809/.codex/runtime-evidence/TianQuan")
        .expect("evidence root");
    assert!(
        path.starts_with(root) && path.is_dir(),
        "case stays inside owned evidence root"
    );
    path
}
fn writer_phase() -> bool {
    match required("TQ_OWNED_PREFIX_PHASE").as_str() {
        "writer" => true,
        "reader" => false,
        _ => panic!("phase must be writer or reader; no implicit retries"),
    }
}
fn retained_regular_file(path: &std::path::Path) {
    let meta = std::fs::symlink_metadata(path).expect("retained regular file");
    assert!(meta.is_file() && !meta.file_type().is_symlink());
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(meta.file_attributes() & 0x400, 0, "no reparse point");
    }
    let canonical = std::fs::canonicalize(path).expect("retained canonical file");
    assert_eq!(canonical.parent(), Some(case_dir().as_path()));
}
async fn libsql_backend(writer: bool) -> Box<dyn RootFilesystem> {
    let path = case_dir().join("prefix.db");
    if writer {
        assert!(
            std::fs::symlink_metadata(&path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(
            std::fs::read_dir(case_dir()).unwrap().next().is_none(),
            "fresh empty backend case"
        );
    } else {
        retained_regular_file(&path);
    }
    let db = Arc::new(
        libsql::Builder::new_local(path)
            .build()
            .await
            .expect("owned libSQL open"),
    );
    let fs = LibSqlRootFilesystem::new(db);
    if writer {
        fs.run_migrations()
            .await
            .expect("original libSQL migrations");
    }
    Box::new(fs)
}
async fn postgres_backend(writer: bool) -> Box<dyn RootFilesystem> {
    let host = required("TQ_OWNED_PREFIX_PG_HOST");
    assert!(host == "127.0.0.1" || host == "::1", "owned loopback only");
    let schema = required("TQ_OWNED_PREFIX_PG_SCHEMA");
    assert!(
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    );
    let database = required("TQ_OWNED_PREFIX_PG_DATABASE");
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&host)
        .port(required("TQ_OWNED_PREFIX_PG_PORT").parse().expect("port"))
        .user(&required("TQ_OWNED_PREFIX_PG_USER"))
        .password(required("TQ_OWNED_PREFIX_PG_PASSWORD"))
        .dbname(&database)
        .options(&format!("-csearch_path={schema}"));
    let manager = deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls);
    let pool = deadpool_postgres::Pool::builder(manager)
        .max_size(1)
        .build()
        .expect("owned pool");
    {
        let client = pool.get().await.expect("owned PG connection");
        let row = client.query_one("SELECT current_database(),current_schema(),obj_description(oid,'pg_namespace') FROM pg_namespace WHERE nspname=current_schema()", &[]).await.expect("read original schema ownership");
        assert_eq!(row.get::<_, String>(0), database);
        assert_eq!(row.get::<_, String>(1), schema);
        assert_eq!(
            row.get::<_, Option<String>>(2).as_deref(),
            Some(required("TQ_OWNED_PREFIX_PG_OWNERSHIP_COMMENT").as_str())
        );
    } // Release the only connection before the real filesystem factory uses it.
    if writer {
        let client = pool.get().await.expect("owned empty-schema precondition");
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_class WHERE relnamespace=current_schema()::regnamespace",
                &[],
            )
            .await
            .expect("real empty schema observation")
            .get(0);
        assert_eq!(
            count, 0,
            "original migrations may contain legacy index DROP; only empty owned schema admitted"
        );
        assert!(
            std::fs::read_dir(case_dir()).unwrap().next().is_none(),
            "fresh empty backend case"
        );
    }
    let fs = PostgresRootFilesystem::new(pool);
    if writer {
        fs.run_migrations()
            .await
            .expect("original PostgreSQL migrations");
    }
    Box::new(fs)
}

const FIXTURES: [&[u8]; 3] = [b"fixture-a", b"fixture-b", b"fixture-c"];
fn record_oracle(seq: u64, payload: &[u8]) -> Value {
    json!({"seq":seq,"bytes":payload.len(),"hash":blake3::hash(payload).to_hex().to_string(),"payload":payload})
}
fn independent_oracle(seqs: [u64; 3], gap: u64) -> Value {
    assert!(seqs[0] > 0 && seqs[0] < gap && gap < seqs[1] && seqs[1] < seqs[2]);
    json!({"schemaVersion":"owned-complete-prefix-storage/2","generation":format!("{}:{}",seqs[0],blake3::hash(FIXTURES[0]).to_hex()),"head":seqs[2],"gapSeq":gap,
        "records":[record_oracle(seqs[0],FIXTURES[0]),record_oracle(seqs[1],FIXTURES[1]),record_oracle(seqs[2],FIXTURES[2])]})
}
fn validate_oracle(value: &Value) -> Value {
    let map = value.as_object().expect("proof object");
    assert_eq!(map.len(), 5);
    for key in ["schemaVersion", "generation", "head", "gapSeq", "records"] {
        assert!(map.contains_key(key));
    }
    let rows = value["records"].as_array().expect("proof records");
    assert_eq!(rows.len(), 3);
    let mut seqs = [0; 3];
    for (n, row) in rows.iter().enumerate() {
        let obj = row.as_object().expect("record object");
        assert_eq!(obj.len(), 4);
        for key in ["seq", "bytes", "hash", "payload"] {
            assert!(obj.contains_key(key));
        }
        seqs[n] = row["seq"].as_u64().expect("exact original ack sequence");
        assert_eq!(*row, record_oracle(seqs[n], FIXTURES[n]));
    }
    let expected = independent_oracle(seqs, value["gapSeq"].as_u64().expect("original gap ack"));
    assert_eq!(
        *value, expected,
        "proof schema/payloads/hash/seq/head/generation independent contract"
    );
    expected
}
async fn snapshot(fs: &dyn RootFilesystem, gap: u64) -> Value {
    let path = VirtualPath::new("/engine/owned/complete-prefix-log").unwrap();
    let prefix = fs
        .complete_event_prefix(&path, 128, 65536, 4096)
        .await
        .expect("actual bounded prefix")
        .expect("prefix exists");
    let generation = prefix.generation().to_string();
    let head = prefix.head().get();
    let records = prefix
        .into_records()
        .into_iter()
        .map(|r| record_oracle(r.seq.get(), &r.payload))
        .collect::<Vec<_>>();
    json!({"schemaVersion":"owned-complete-prefix-storage/2","generation":generation,"head":head,"gapSeq":gap,"records":records})
}
async fn exact_budget_denials(fs: &dyn RootFilesystem, expected: &Value) {
    let path = VirtualPath::new("/engine/owned/complete-prefix-log").unwrap();
    for (rows, bytes, record) in [(2, 65536, 4096), (128, 1, 4096), (128, 65536, 2)] {
        match fs.complete_event_prefix(&path, rows, bytes, record).await {
            Err(FilesystemError::Backend {
                path: actual,
                operation: FilesystemOperation::Tail,
                reason,
                ..
            }) => {
                assert_eq!(actual, path);
                assert_eq!(reason, "complete-prefix budget exceeded");
            }
            _ => panic!("must reject the exact complete-prefix budget, not any backend error"),
        }
        assert_eq!(
            snapshot(fs, expected["gapSeq"].as_u64().unwrap()).await,
            *expected,
            "budget refusal must not alter/lose actual data"
        );
    }
}
async fn phase(fs: Box<dyn RootFilesystem>, writer: bool, backend: &str) {
    let proof = case_dir().join("prefix-proof.json");
    let path = VirtualPath::new("/engine/owned/complete-prefix-log").unwrap();
    let expected = if writer {
        assert!(
            std::fs::symlink_metadata(&proof)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(
            fs.complete_event_prefix(&path, 128, 65536, 4096)
                .await
                .unwrap()
                .is_none()
        );
        let sa = fs.append(&path, FIXTURES[0].to_vec()).await.unwrap();
        let initial = fs
            .event_log_generation(&path)
            .await
            .unwrap()
            .expect("actual generation");
        let gap = fs
            .append(
                &VirtualPath::new("/engine/owned/unrelated-gap").unwrap(),
                b"gap".to_vec(),
            )
            .await
            .unwrap();
        let sb = fs.append(&path, FIXTURES[1].to_vec()).await.unwrap();
        let sc = fs.append(&path, FIXTURES[2].to_vec()).await.unwrap();
        let expected = independent_oracle([sa.get(), sb.get(), sc.get()], gap.get());
        assert_eq!(initial, expected["generation"]);
        assert_eq!(snapshot(fs.as_ref(), gap.get()).await, expected);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&proof)
            .expect("new original oracle");
        file.write_all(&serde_json::to_vec_pretty(&expected).unwrap())
            .unwrap();
        file.sync_all().unwrap();
        expected
    } else {
        retained_regular_file(&proof);
        assert!(std::fs::metadata(&proof).unwrap().len() <= 65536);
        let original: Value = serde_json::from_slice(&std::fs::read(&proof).unwrap()).unwrap();
        let expected = validate_oracle(&original);
        assert_eq!(
            snapshot(fs.as_ref(), expected["gapSeq"].as_u64().unwrap()).await,
            expected
        );
        expected
    };
    exact_budget_denials(fs.as_ref(), &expected).await;
    println!(
        "OWNED_PREFIX_PHASE_OK backend={backend} phase={} pid={} retained=true independentOracle=true exactBudgetDenied=3 providerQualified=false domainUsageQualified=false",
        if writer { "writer" } else { "reader" },
        std::process::id()
    );
}
#[tokio::test]
#[ignore = "requires fresh Root-owned file case and two separately supervised processes"]
async fn libsql_owned_complete_prefix_process_phase() {
    let writer = writer_phase();
    phase(libsql_backend(writer).await, writer, "libsql").await;
}
#[tokio::test]
#[ignore = "requires fresh Root-owned PG/schema identity and two separately supervised processes"]
async fn postgres_owned_complete_prefix_process_phase() {
    let writer = writer_phase();
    phase(postgres_backend(writer).await, writer, "postgres").await;
}
