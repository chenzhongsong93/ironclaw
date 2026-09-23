//! TianQuan 16 SOUL subagent flavors — 对齐 skill-adapter 的 9 tier:role + 工具白名单。
//!
//! 每个 SOUL 是一个 subagent kind(novelist/worldsmith 等),按 tier:role 分组。
//! 工具白名单对齐 skill-adapter state_machine 的阶段工具语义:
//!   - META/WORLD 层:读 + delegate(派下游)
//!   - LOOP 层:读 + 写 + delegate + validate(经 MCP 调左脑引擎)
//!
//! 注:这里的 allowed_capabilities 是迁移期安全上界；运行许可由天权
//! catalog 声明、此上界和 host 授权取交集。
//! 天权左脑引擎经 MCP 暴露(ironclaw 调 mcp 工具 → 天权 api),不在此白名单
//! (MCP 工具是 ironclaw 的 builtin.mcp__* capability,子 agent 默认可见经 profile 配置)。

use ironclaw_host_api::CapabilityId;

/// 16 个天权 SOUL subagent kind(对齐天权 infra/agents/novel-studio/catalog.json)。
///
/// kind 命名用连字符(对齐 SubagentKindId 校验:ascii alphanumeric/_/-)。
pub const TIANQUAN_SOUL_KINDS: &[&str] = &[
    // META tier (meta:greenlight / meta:ontology)
    "market-researcher",
    "story-architect",
    "market-evaluator",
    "schema-architect",
    "ontologist",
    // WORLD tier (world:static / loop:state)
    "worldsmith",
    // LOOP tier (loop:plot / loop:event / loop:discourse / loop:pack / loop:prose)
    "plotter",
    "event-simulator",
    "discourse-planner",
    "chapter-packer",
    "scene-reasoner",
    "novelist",
    "auditor",
    "committer",
    "chapter-reviewer",
    "polisher",
];

/// tier:role 分组(对齐 skill-adapter 9 tier:role 枚举)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TianquanTier {
    MetaGreenlight,
    MetaOntology,
    WorldStatic,
    LoopState,
    LoopPlot,
    LoopEvent,
    LoopDiscourse,
    LoopPack,
    LoopProse,
}

impl TianquanTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MetaGreenlight => "meta:greenlight",
            Self::MetaOntology => "meta:ontology",
            Self::WorldStatic => "world:static",
            Self::LoopState => "loop:state",
            Self::LoopPlot => "loop:plot",
            Self::LoopEvent => "loop:event",
            Self::LoopDiscourse => "loop:discourse",
            Self::LoopPack => "loop:pack",
            Self::LoopProse => "loop:prose",
        }
    }
}

/// 天权 SOUL flavor 定义。
pub struct TianquanSoulFlavor {
    pub kind: &'static str,
    pub tier: TianquanTier,
    pub allow_nesting: bool,
    pub summary: &'static str,
    /// 工具白名单(IronClaw capability id)。子 agent 经 MCP 调天权左脑引擎不在此列
    /// (MCP 工具经 profile 配置可见)。此白名单控制 IronClaw 原生工具(文件/shell 等)。
    pub tool_allowlist: &'static [&'static str],
}

/// 16 SOUL flavor 表。
///
/// 工具白名单策略(对齐 skill-adapter state_machine):
/// - META/WORLD(规划+世界):读 + delegate(派下游),不写 canon
/// - LOOP plot/event/discourse(剧情层):读 + delegate + validate(调左脑校验)
/// - LOOP pack(章节层):读 + delegate
/// - LOOP prose(novelist/auditor/committer/polisher):只读输入，产物统一经 Artifact API/MCP 写入
pub const TIANQUAN_SOUL_FLAVORS: &[TianquanSoulFlavor] = &[
    // META greenlight (L0)
    TianquanSoulFlavor {
        kind: "market-researcher",
        tier: TianquanTier::MetaGreenlight,
        allow_nesting: false,
        summary: "市场调研+题材可爆性分析(L0 立项)",
        tool_allowlist: &["builtin.http"],
    },
    TianquanSoulFlavor {
        kind: "story-architect",
        tier: TianquanTier::MetaGreenlight,
        allow_nesting: false,
        summary: "项目身份立项+故事架构(L0)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "market-evaluator",
        tier: TianquanTier::MetaGreenlight,
        allow_nesting: false,
        summary: "立项审批+市场评估(L0)",
        tool_allowlist: &[],
    },
    // META ontology (L1+L2)
    TianquanSoulFlavor {
        kind: "schema-architect",
        tier: TianquanTier::MetaOntology,
        allow_nesting: false,
        summary: "本体类层定义+SHACL(L1)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "ontologist",
        tier: TianquanTier::MetaOntology,
        allow_nesting: false,
        summary: "本体补丁+叙事契约(L2)",
        tool_allowlist: &[],
    },
    // WORLD (L3)
    TianquanSoulFlavor {
        kind: "worldsmith",
        tier: TianquanTier::WorldStatic,
        allow_nesting: false,
        summary: "世界实体+运行时态(L3 world:static + loop:state)",
        tool_allowlist: &[],
    },
    // LOOP plot (L5)
    TianquanSoulFlavor {
        kind: "plotter",
        tier: TianquanTier::LoopPlot,
        allow_nesting: false,
        summary: "剧情开槽+弧卡+伏笔(L5 loop:plot)",
        tool_allowlist: &[],
    },
    // LOOP event (L4)
    TianquanSoulFlavor {
        kind: "event-simulator",
        tier: TianquanTier::LoopEvent,
        allow_nesting: false,
        summary: "事件填槽+因果链(L4 loop:event)",
        tool_allowlist: &[],
    },
    // LOOP discourse (L6)
    TianquanSoulFlavor {
        kind: "discourse-planner",
        tier: TianquanTier::LoopDiscourse,
        allow_nesting: false,
        summary: "话语规划+渲染调度(L6 loop:discourse)",
        tool_allowlist: &[],
    },
    // LOOP pack (L7)
    TianquanSoulFlavor {
        kind: "chapter-packer",
        tier: TianquanTier::LoopPack,
        allow_nesting: false,
        summary: "章节打包+Coverage(L7 loop:pack)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "scene-reasoner",
        tier: TianquanTier::LoopPack,
        allow_nesting: false,
        summary: "场景推理+硬门(L7 loop:pack)",
        tool_allowlist: &[],
    },
    // LOOP prose (L8)
    TianquanSoulFlavor {
        kind: "novelist",
        tier: TianquanTier::LoopProse,
        allow_nesting: false,
        summary: "正文创作(P1-P12 大白话+铁律+VERIFY 15 项,L8 loop:prose)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "auditor",
        tier: TianquanTier::LoopProse,
        allow_nesting: false,
        summary: "审计(13 硬检查+review-gate,L8 loop:prose)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "committer",
        tier: TianquanTier::LoopProse,
        allow_nesting: false,
        summary: "提交(9 commit-gate+CommitRecord,L8 loop:prose)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "chapter-reviewer",
        tier: TianquanTier::LoopProse,
        allow_nesting: false,
        summary: "章节复核(L8 loop:prose)",
        tool_allowlist: &[],
    },
    TianquanSoulFlavor {
        kind: "polisher",
        tier: TianquanTier::LoopProse,
        allow_nesting: false,
        summary: "润色(L8 loop:prose)",
        tool_allowlist: &[],
    },
];

/// 按 kind 查 flavor。
pub fn lookup_soul_flavor(kind: &str) -> Option<&'static TianquanSoulFlavor> {
    TIANQUAN_SOUL_FLAVORS.iter().find(|f| f.kind == kind)
}

/// 16 SOUL 的 SpawnSubagentFlavorDescriptor catalog(给 LLM 看可派哪些子 agent)。
pub fn tianquan_flavor_catalog() -> Vec<ironclaw_loop_host::SpawnSubagentFlavorDescriptor> {
    TIANQUAN_SOUL_FLAVORS
        .iter()
        .map(|f| ironclaw_loop_host::SpawnSubagentFlavorDescriptor {
            id: ironclaw_loop_host::SubagentKindId::new(f.kind).expect("valid SubagentKindId"), // safety: TIANQUAN_SOUL_FLAVORS kinds are compile-time-constant valid
            summary: f.summary.to_string(),
        })
        .collect()
}

/// 合并的 flavor catalog:4 内置(general/explorer/coder/planner)+ 16 天权 SOUL(novelist/auditor 等)。
///
/// P0-② 修复(spec 2026-07-20-novel-studio-skill-e2e-broken-fixes-design.md):
/// ironclaw_runner runtime.rs:691 原硬编码 `flavors::builtin_flavor_catalog()`(4 内置),
/// LLM schema enum 不含 16 SOUL,派不出 novelist/auditor 等子 agent。
/// 本函数返回 4+16=20 个 flavor,由 reborn_composition 注入 DefaultPlannedRuntimeParts.subagent_flavor_catalog,
/// 让 LLM 看到完整 20 个 subagent_type enum 值。
///
/// 顺序:4 内置在前(保持 ironclaw 默认行为兼容),16 SOUL 在后(按 TIANQUAN_SOUL_FLAVORS 表序)。
pub fn merged_flavor_catalog() -> Vec<ironclaw_loop_host::SpawnSubagentFlavorDescriptor> {
    let mut catalog = ironclaw_runner::subagent::flavors::builtin_flavor_catalog();
    catalog.extend(tianquan_flavor_catalog());
    catalog
}

/// 解析天权 SOUL kind(返回是否是天权 16 kind 之一)。
pub fn is_tianquan_kind(kind: &str) -> bool {
    TIANQUAN_SOUL_KINDS.contains(&kind)
}

/// 取 flavor 的工具白名单(BTreeSet<CapabilityId>)。
pub fn allowed_capabilities_for(
    kind: &str,
) -> Result<std::collections::BTreeSet<CapabilityId>, String> {
    let flavor =
        lookup_soul_flavor(kind).ok_or_else(|| format!("unknown tianquan soul kind: {kind}"))?;
    flavor
        .tool_allowlist
        .iter()
        .map(|id| CapabilityId::new(*id).map_err(|e| format!("invalid capability id {id}: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_souls_present() {
        assert_eq!(TIANQUAN_SOUL_FLAVORS.len(), 16);
        assert_eq!(TIANQUAN_SOUL_KINDS.len(), 16);
    }

    #[test]
    fn kinds_match_flavors() {
        for kind in TIANQUAN_SOUL_KINDS {
            assert!(
                lookup_soul_flavor(kind).is_some(),
                "flavor missing for kind {kind}"
            );
        }
    }

    #[test]
    fn flavor_catalog_has_16_entries() {
        let catalog = tianquan_flavor_catalog();
        assert_eq!(catalog.len(), 16);
    }

    #[test]
    fn is_tianquan_kind_recognizes_novelist() {
        assert!(is_tianquan_kind("novelist"));
        assert!(is_tianquan_kind("worldsmith"));
        assert!(!is_tianquan_kind("general")); // ironclaw 内置,非天权
        assert!(!is_tianquan_kind("nonexistent"));
    }

    #[test]
    fn allowed_capabilities_for_novelist() {
        // 天权铁律:创作 agent 无任何 coding 能力(读文件/写文件/列目录/检索/patch/shell
        // 全部移除,2026-09-17 用户钦定"所有能力都围绕创作小说/创作虚拟世界")。
        // 创作资产只能经 MCP 工具/ArtifactStore 坐标读写,不暴露任意文件路径。
        let caps = allowed_capabilities_for("novelist").unwrap();
        for capability in [
            "builtin.write_file",
            "builtin.read_file",
            "builtin.list_dir",
            "builtin.glob",
            "builtin.grep",
            "builtin.apply_patch",
            "builtin.shell",
        ] {
            assert!(
                !caps.contains(&CapabilityId::new(capability).unwrap()),
                "novelist 不应拥有 {capability}"
            );
        }
    }

    #[test]
    fn allowed_capabilities_unknown_kind_errors() {
        assert!(allowed_capabilities_for("nonexistent").is_err());
    }

    #[test]
    fn tier_as_str_matches_skill_adapter() {
        assert_eq!(TianquanTier::LoopProse.as_str(), "loop:prose");
        assert_eq!(TianquanTier::MetaGreenlight.as_str(), "meta:greenlight");
    }
}
