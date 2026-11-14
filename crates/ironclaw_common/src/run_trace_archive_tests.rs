use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Barrier};

struct TraceEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl TraceEnv {
    fn new(root: &Path) -> Self {
        let keys = [
            "TIANQUAN_TRACE_DIR",
            "TIANQUAN_TRACE_MAX_ACTIVE",
            "TIANQUAN_TRACE_MAX_ARCHIVED",
        ];
        let old = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        unsafe {
            std::env::set_var(keys[0], root);
            std::env::set_var(keys[1], "0");
            std::env::set_var(keys[2], "0");
        }
        Self(old)
    }
}
impl Drop for TraceEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("new owned trace test directory")
}
fn append(run: &str, id: usize) {
    super::append_trace_event(
        "archive_test",
        run,
        Some("same-thread"),
        None,
        serde_json::json!({"id": id}),
    );
}
fn decode(root: &Path, run: &str) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(root.join(format!("{run}.jsonl.gz"))).expect("archive exists");
    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut text = String::new();
    decoder
        .read_to_string(&mut text)
        .expect("complete valid gzip");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("complete event"))
        .collect()
}
#[test]
fn resumed_run_keeps_archived_prefix_and_no_hot_is_idempotent() {
    let dir = root();
    let root = dir.path();
    let _env = TraceEnv::new(root);
    let run = "resumed-run";
    append(run, 0);
    append(run, 1);
    let prefix = std::fs::read(root.join(format!("{run}.jsonl"))).expect("first hot");
    super::compact_run_trace(run);
    append(run, 2);
    append(run, 3);
    let suffix = std::fs::read(root.join(format!("{run}.jsonl"))).expect("resumed hot");
    let expected: Vec<serde_json::Value> = prefix
        .split(|b| *b == b'\n')
        .chain(suffix.split(|b| *b == b'\n'))
        .filter(|v| !v.is_empty())
        .map(|v| serde_json::from_slice(v).unwrap())
        .collect();
    super::compact_run_trace(run);
    assert_eq!(
        decode(root, run),
        expected,
        "archive prefix lost after resumed compact"
    );
    let old = std::fs::read(root.join(format!("{run}.jsonl.gz"))).unwrap();
    super::compact_run_trace(run);
    assert_eq!(
        std::fs::read(root.join(format!("{run}.jsonl.gz"))).unwrap(),
        old,
        "no hot compact changed archive"
    );
}
#[test]
fn damaged_prefix_keeps_both_original_files() {
    let dir = root();
    let root = dir.path();
    let _env = TraceEnv::new(root);
    let run = "damaged-run";
    append(run, 0);
    super::compact_run_trace(run);
    let archive = root.join(format!("{run}.jsonl.gz"));
    let mut damaged = std::fs::read(&archive).unwrap();
    damaged.truncate(damaged.len() - 5);
    std::fs::write(&archive, &damaged).unwrap();
    append(run, 1);
    let hot = root.join(format!("{run}.jsonl"));
    let old_hot = std::fs::read(&hot).unwrap();
    super::compact_run_trace(run);
    assert_eq!(
        std::fs::read(&archive).unwrap(),
        damaged,
        "damaged archive overwritten"
    );
    assert_eq!(
        std::fs::read(&hot).expect("valid hot must be retained"),
        old_hot
    );
}
#[test]
fn concurrent_append_compact_keeps_complete_unique_events() {
    let dir = root();
    let root = dir.path();
    let _env = TraceEnv::new(root);
    let run = "concurrent-run";
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for worker in 0..3 {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            for n in 0..24 {
                append(run, worker * 24 + n);
            }
        }));
    }
    barrier.wait();
    for _ in 0..24 {
        super::compact_run_trace(run);
    }
    for worker in workers {
        worker.join().expect("append worker joined");
    }
    super::compact_run_trace(run);
    let events = decode(root, run);
    let ids: BTreeSet<u64> = events
        .iter()
        .map(|ev| ev["data"]["id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        events.len(),
        72,
        "concurrent trace event loss or duplication"
    );
    assert_eq!(
        ids,
        (0..72).collect(),
        "concurrent trace complete unique set"
    );
}
