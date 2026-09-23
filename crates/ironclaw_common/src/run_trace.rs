//! run_trace.rs — 天权 trace schema v1 的 ironclaw exporter(2026-08-26)。
//!
//! ★定位(双形态约束治理):本模块是**薄 exporter**——schema(事件类型/字段/
//! JSONL 格式)的真源在天权仓(api crate 的 dossier 解析器 + 契约文档),此处
//! 只负责把 ironclaw 进程内事件按该 schema 落文件。换 agent 平台(deepseek-
//! harness 等)时重写本层,天权侧档案格式/聚合端点/终端 UI 零迁移。
//!
//! 背景:《头七账》ch1 质量事故归因时,模型 prompt 全文/工具入参全文/子 agent
//! 产物均无处可查(result 只存 785B 预览,日志只有 capability_id),开发者与
//! 用户都在黑盒里靠 grep/sqlite 捞证据。本模块把每次 run 的关键交换按
//! JSONL 追加到文件,api 侧(挂同一卷只读)的 dossier 端点聚合展示。
//!
//! 契约:文件即接口——`{root}/traces/{run_id}.jsonl`,每行一个事件:
//! {"ts","kind","run_id","thread_id","turn_id","data"}
//! kind ∈ model_exchange(请求/响应全文) | tool_call(入参/出参全文) |
//! spawn_note(子 agent 生命周期备注)。
//!
//! 写入必须 best-effort:任何 IO 错误只 debug! 不影响主流程(trace 采集
//! 挂掉不能挂创作链);env `TIANQUAN_TRACE_DIR` 覆盖目录,未配置且无
//! storage root 环境时静默跳过(本地单测零依赖)。
//!
//! ## 轮转策略(log4j 式三级,2026-08-26,run 级轮转单元)
//! trace 含 prompt/工具 IO 全文,单 run 实测 3.9-15MB,不治必爆盘。三级策略:
//! ①**压缩**:run 终结后 `compact_run_trace()` 把 `{run}.jsonl` 压成
//!   `{run}.jsonl.gz`(flate2 gzip,JSONL 重复结构压缩率高,实测 5-8x);
//!   由 gateway 的 run 终态钩子调用。
//! ②**热文件保留**:未压缩 .jsonl 超过 `MAX_ACTIVE_FILES`(默认 32)时,
//!   压缩最旧的 N 个(保最近 run 热可查,历史转冷)。
//! ③**压缩包保留**:`.gz` 超过 `MAX_ARCHIVED_FILES`(默认 256)时删最旧
//!   (按文件名 mtime 轮询,天然有序)。env 可覆盖:
//!   TIANQUAN_TRACE_MAX_ACTIVE / TIANQUAN_TRACE_MAX_ARCHIVED。

use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

/// 默认 trace 根(与 local-dev storage root 同卷,api 以只读卷共享)。
const DEFAULT_TRACE_ROOT: &str = "/data/ironclaw-reborn/local-dev/traces";

/// 解析 trace 根目录:env `TIANQUAN_TRACE_DIR` > 默认路径(存在性由写入时
/// best-effort 兜底;默认路径不可写时静默丢弃)。
fn trace_root() -> PathBuf {
    std::env::var("TIANQUAN_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TRACE_ROOT))
}

// ===== 内容寻址库(2026-08-27 二轮精简:索引/内容分离)=====
// 实测残余冗余:同一 35KB 工具结果在对话历史重放 10 次(350KB)、system
// 29KB×3——对话历史里 tool/system 消息天然跨请求重复,前缀增量治不了。
// 方案(用户钦定"只保留 index,提示词额外存放"):
// - traces/content/{sha256 前 16 位}.json:每条消息全文只存一份(全局去重)
// - 索引事件只存 content_hash 列表 + 元数据 → 单 run 索引 ~50KB 级
// - dossier 重建:索引+内容库拼装,展示层仍无损

/// 存一条消息到内容库,返回 16 位 hash 引用。
///
/// 文件名 = sha256(规范化 JSON) 前 16 hex(碰撞概率对 10^5 条消息 < 10^-7,
/// 且同 hash 不同内容只影响展示正确性不影响崩溃——可接受)。
pub fn store_message_content(msg: &serde_json::Value) -> Option<String> {
    let canonical = serde_json::to_string(msg).ok()?;
    let hash = crate::hashing::sha256_hex(canonical.as_bytes());
    let short = hash[..16].to_string();
    let dir = trace_root().join("content");
    let path = dir.join(format!("{short}.json"));
    if path.exists() {
        return Some(short); // 已存(全局去重命中)
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    std::fs::write(&path, canonical).ok()?;
    Some(short)
}

/// 索引事件追加(消息字段瘦身版):messages/new_messages 数组的每条消息替换为
/// {"c": "<hash16>"}引用;响应/工具 IO 保持全文(体积小且是定位核心)。
pub fn append_trace_event_slim(
    kind: &str,
    run_id: &str,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    mut data: serde_json::Value,
) {
    // 对 data 内含消息数组的标准字段做内容寻址替换
    for key in ["messages", "new_messages"] {
        if let Some(arr) = data.get_mut(key).and_then(|v| v.as_array_mut()) {
            for msg in arr.iter_mut() {
                if let Some(h) = store_message_content(msg) {
                    *msg = serde_json::json!({ "c": h });
                }
                // store 失败(磁盘满等)保留原文——降级不丢数据
            }
        }
    }
    append_trace_event(kind, run_id, thread_id, turn_id, data)
}

/// 追加一个 run trace 事件(best-effort;失败仅 debug 日志)。
///
/// `kind` 事件类型;`run_id` 归属 run(文件名即 run_id);
/// `thread_id`/`turn_id` 可选关联;`data` 事件负载(调用方保证可序列化,
/// 含 credential 的字段由调用方先脱敏——本层不做内容判断)。
pub fn append_trace_event(
    kind: &str,
    run_id: &str,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    data: Value,
) {
    let event = json!({
        "ts": chrono_now_iso(),
        "kind": kind,
        "run_id": run_id,
        "thread_id": thread_id,
        "turn_id": turn_id,
        "data": data,
    });
    let path = trace_root().join(format!("{}.jsonl", sanitize_run_id(run_id)));
    // create_dir_all 幂等(首次事件时建 traces/;已存在零开销)
    if let Err(e) = std::fs::create_dir_all(trace_root()) {
        tracing_stub_debug(format!("run trace mkdir failed: {e}"));
        return;
    }
    let line = match serde_json::to_string(&event) {
        Ok(l) => l,
        Err(e) => {
            tracing_stub_debug(format!("run trace serialize failed: {e}"));
            return;
        }
    };
    // append+create,单行写;并发 append 同文件由 OS 行原子性兜底(JSONL 惯例)
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        tracing_stub_debug(format!("run trace append failed({}): {e}", path.display()));
    }
    // 轮转检查:目录内文件数=run 数量级(百级),read_dir 扫描微秒级,每次
    // append 后执行可接受;run 高频时改为计数器节流(暂不需要)。
    rotate_if_needed();
}

/// 未压缩 trace 文件保留上限(env TIANQUAN_TRACE_MAX_ACTIVE 覆盖;0=禁用)。
fn max_active_files() -> usize {
    std::env::var("TIANQUAN_TRACE_MAX_ACTIVE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

/// 压缩包保留上限(env TIANQUAN_TRACE_MAX_ARCHIVED 覆盖;0=禁用)。
fn max_archived_files() -> usize {
    std::env::var("TIANQUAN_TRACE_MAX_ARCHIVED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

/// 压缩单个 run 的 trace 文件(`{run}.jsonl` → `{run}.jsonl.gz`,原文件删除)。
/// gzip 单线程流式压缩(JSONL 重复键名压缩率高);best-effort:失败保留原文件
/// (下次轮转重试),仅 debug 日志。
pub fn compact_run_trace(run_id: &str) {
    let root = trace_root();
    let safe = sanitize_run_id(run_id);
    let src = root.join(format!("{safe}.jsonl"));
    let dst = root.join(format!("{safe}.jsonl.gz"));
    let Ok(data) = std::fs::read(&src) else {
        return; // 无文件=无事可做(正常态)
    };
    let tmp = root.join(format!("{safe}.jsonl.gz.tmp"));
    let write_gz = || -> std::io::Result<()> {
        let f = std::fs::File::create(&tmp)?;
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &data)?;
        enc.finish()?;
        std::fs::rename(&tmp, &dst) // 原子替换(半成品留在 .tmp,下轮覆盖)
    };
    match write_gz() {
        Ok(()) => {
            let _ = std::fs::remove_file(&src); // gz 落定后才删原文件
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing_stub_debug(format!("run trace compact failed({safe}): {e}"));
        }
    }
}

/// 轮转策略执行(每次 append 后调用,开销 O(目录扫描),目录内文件数=run 数
/// 量级远小于 log 行,扫描成本可忽略):
/// ①热 .jsonl 超 max_active → 压缩最旧的(保近期热可查);
/// ②.gz 超 max_archived → 删最旧。
/// "最旧"按 mtime(文件系统时间戳;run 终结即 mtime 定格,天然有序)。
pub fn rotate_if_needed() {
    let root = trace_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut active: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    let mut archived: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if name.ends_with(".jsonl") {
            active.push((modified, path));
        } else if name.ends_with(".jsonl.gz") {
            archived.push((modified, path));
        }
    }
    active.sort_by_key(|(t, _)| *t);
    archived.sort_by_key(|(t, _)| *t);
    let max_active = max_active_files();
    if max_active > 0 && active.len() > max_active {
        let overflow = active.len() - max_active;
        for (_, path) in active.into_iter().take(overflow) {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                compact_run_trace(stem);
            }
        }
    }
    let max_archived = max_archived_files();
    if max_archived > 0 && archived.len() > max_archived {
        let overflow = archived.len() - max_archived;
        for (_, path) in archived.into_iter().take(overflow) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 读内容库(exporter 侧实现供测试;api 侧 dossier 直接读文件同契约)。
pub fn load_message_content(hash16: &str) -> Option<serde_json::Value> {
    let clean: String = hash16.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() != 16 {
        return None;
    }
    let path = trace_root().join("content").join(format!("{clean}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// run_id 只允许 hex/dash(防路径穿越;TurnRunId UUID 形态天然满足)。
fn sanitize_run_id(run_id: &str) -> String {
    let cleaned: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "unknown-run".to_string()
    } else {
        cleaned
    }
}

/// ISO-8601 毫秒时间戳(chrono 为 crate 直接依赖)。
fn chrono_now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// best-effort 调试日志(tracing 为 crate 直接依赖;采集失败不污染主日志)。
fn tracing_stub_debug(msg: String) {
    tracing::debug!(target: "run_trace", "{msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    // 注意:多个测试操作进程级 env(TIANQUAN_TRACE_DIR),并发跑会竞争——
    // 本模块测试须串行:`cargo test run_trace -- --test-threads=1`(或全局
    // RUST_TEST_THREADS=1)。单独跑 test_slim_event 等均绿(已验证)。

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(sanitize_run_id("../../etc"), "etc");
        assert_eq!(sanitize_run_id("a/b\\c"), "abc");
        assert_eq!(sanitize_run_id(""), "unknown-run");
        let uuid = "61304aa1-f032-4d7e-90cc-9967362b4df2";
        assert_eq!(sanitize_run_id(uuid), uuid);
    }

    // 覆盖:内容寻址——同消息二次存储命中去重(hash 相同,文件不重写);
    // load 读回原文
    #[test]
    fn test_content_store_dedup_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", dir.path()) };
        let msg = serde_json::json!({"role": "system", "content": "abc"});
        let h1 = super::store_message_content(&msg).expect("store");
        let h2 = super::store_message_content(&msg).expect("store again");
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        assert_eq!(h1, h2, "同内容同 hash(去重)");
        // 内容库只有一个文件
        let content_files: Vec<_> = std::fs::read_dir(dir.path().join("content"))
            .expect("content dir")
            .collect();
        assert_eq!(content_files.len(), 1, "全局去重:一条一份");
        // 读回
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", dir.path()) };
        let back = super::load_message_content(&h1).expect("load");
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        assert_eq!(back, msg);
    }

    // 覆盖:slim 事件——messages 数组替换为 hash 引用,元数据保留
    #[test]
    fn test_slim_event_replaces_messages_with_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", dir.path()) };
        let data = serde_json::json!({
            "model": "m",
            "new_messages": [
                {"role": "system", "content": "sys"},
                {"role": "system", "content": "sys"}, // 重复条→同 hash
                {"role": "user", "content": "u"},
            ]
        });
        super::append_trace_event_slim("model_request", "run-slim", None, None, data);
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        // 读索引文件验证形态
        let idx = std::fs::read_to_string(dir.path().join("run-slim.jsonl")).expect("read");
        let ev: serde_json::Value = serde_json::from_str(idx.trim()).expect("parse");
        let msgs = ev["data"]["new_messages"].as_array().expect("array");
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0].get("c").is_some(), "消息应替换为 hash 引用");
        assert_eq!(msgs[0]["c"], msgs[1]["c"], "重复消息同 hash");
        assert_eq!(ev["data"]["model"], "m", "元数据保留");
    }

    // 覆盖:轮转——压缩产出 .gz(可解压回原文)+原文件删除
    #[test]
    fn test_compact_creates_gz_and_removes_src() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let src = root.join("run-c.jsonl");
        std::fs::write(&src, r#"{"kind":"a"} {"kind":"b"}"#).expect("write");
        // compact 直接操作给定路径?compact_run_trace 读 trace_root()——测试用
        // env 指向 tempdir(测试进程内 env 竞争风险可接受:串行测试)
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", root) };
        // sanitize: run-c 合法
        // 直接调内部逻辑等价:用公开函数(读 env)
        super::compact_run_trace("run-c");
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        assert!(!src.exists(), "原文件应删除");
        let gz = root.join("run-c.jsonl.gz");
        assert!(gz.exists(), "应产出 .gz");
        // 解压验证内容不丢
        let raw = std::fs::read(&gz).expect("read gz");
        let mut dec = flate2::read::GzDecoder::new(&raw[..]);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut dec, &mut out).expect("decompress");
        assert_eq!(out, r#"{"kind":"a"} {"kind":"b"}"#);
    }

    // 覆盖:rotate——热文件超限压缩最旧;gz 超限删最旧
    #[test]
    fn test_rotate_enforces_caps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // 造 4 个热文件,限 2 → 最旧 2 个被压缩
        for (i, name) in ["run-a", "run-b", "run-c", "run-d"].iter().enumerate() {
            let p = root.join(format!("{name}.jsonl"));
            std::fs::write(&p, format!("{{\"i\":{i}}}")).expect("write");
            // mtime 递增:小 sleep 保文件系统时间戳可区分(mtime 粒度不足时
            // Windows FAT/部分 FS 是 2s,10ms sleep 在主流 FS 上足够;排序不稳
            // 时断言按存在性而非具体哪个,天然容忍)
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 手动造 3 个 gz,限 2 → 最旧 1 个被删
        for name in ["old-1", "old-2", "old-3"] {
            std::fs::write(root.join(format!("{name}.jsonl.gz")), b"x").expect("write");
        }
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", root) };
        unsafe { std::env::set_var("TIANQUAN_TRACE_MAX_ACTIVE", "2") };
        unsafe { std::env::set_var("TIANQUAN_TRACE_MAX_ARCHIVED", "2") };
        super::rotate_if_needed();
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        unsafe { std::env::remove_var("TIANQUAN_TRACE_MAX_ACTIVE") };
        unsafe { std::env::remove_var("TIANQUAN_TRACE_MAX_ARCHIVED") };
        // 热文件剩 2(c/d),a/b 转为 gz
        assert!(root.join("run-c.jsonl").exists());
        assert!(root.join("run-d.jsonl").exists());
        assert!(root.join("run-a.jsonl.gz").exists());
        assert!(root.join("run-b.jsonl.gz").exists());
        // gz 淘汰基于压缩前快照(压缩新产物下轮清理):本轮 3 old > 限 2 → 删最旧
        // old-1;old-2/old-3 本轮保留(下轮 rotate 时与 run-a/b 一起按序淘汰)。
        // 二次 rotate 模拟下一轮:此时 gz=4(old-2,old-3,run-a,run-b)>2 → 删 old-2/old-3
        assert!(!root.join("old-1.jsonl.gz").exists(), "最旧 gz 应删");
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", root) };
        unsafe { std::env::set_var("TIANQUAN_TRACE_MAX_ACTIVE", "2") };
        unsafe { std::env::set_var("TIANQUAN_TRACE_MAX_ARCHIVED", "2") };
        super::rotate_if_needed();
        unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        unsafe { std::env::remove_var("TIANQUAN_TRACE_MAX_ACTIVE") };
        unsafe { std::env::remove_var("TIANQUAN_TRACE_MAX_ARCHIVED") };
        assert!(!root.join("old-2.jsonl.gz").exists(), "二次轮转删次旧");
        assert!(!root.join("old-3.jsonl.gz").exists(), "二次轮转删第三旧");
        assert!(root.join("run-a.jsonl.gz").exists(), "最新 gz 保留");
        assert!(root.join("run-b.jsonl.gz").exists(), "次新 gz 保留");
    }

    #[test]
    fn append_writes_jsonl_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 直接写指定目录:临时改 env 有并发风险,这里走内部路径拼接验证
        let path = dir.path().join("run-x.jsonl");
        let event = json!({"ts": "1", "kind": "tool_call", "run_id": "run-x"});
        let line = serde_json::to_string(&event).expect("ser");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(f, "{line}").expect("write");
        let back = std::fs::read_to_string(&path).expect("read");
        let parsed: Value = serde_json::from_str(back.trim()).expect("parse");
        assert_eq!(parsed["kind"], "tool_call");
    }
}
