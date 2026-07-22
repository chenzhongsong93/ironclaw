//! TianQuan 16 SOUL direction prompts(对齐 ironclaw_runner::subagent::directions 的 include_str! 机制)。
//!
//! 16 SOUL.md 从姊妹仓 novel-studio-plugin/hermes-plugin/profiles/ 迁移,
//! 编译时 include_str! 嵌入二进制。每个 SOUL 是对应 subagent kind 的 persona 正文。

/// 按 SOUL kind 返回 direction prompt(persona 正文)。
/// kind 必须是 16 天权 SOUL 之一(连字符命名),否则返 None。
pub fn direction_prompt_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "market-researcher" => Some(include_str!("directions/market-researcher.md")),
        "story-architect" => Some(include_str!("directions/story-architect.md")),
        "market-evaluator" => Some(include_str!("directions/market-evaluator.md")),
        "schema-architect" => Some(include_str!("directions/schema-architect.md")),
        "ontologist" => Some(include_str!("directions/ontologist.md")),
        "worldsmith" => Some(include_str!("directions/worldsmith.md")),
        "plotter" => Some(include_str!("directions/plotter.md")),
        "event-simulator" => Some(include_str!("directions/event-simulator.md")),
        "discourse-planner" => Some(include_str!("directions/discourse-planner.md")),
        "chapter-packer" => Some(include_str!("directions/chapter-packer.md")),
        "scene-reasoner" => Some(include_str!("directions/scene-reasoner.md")),
        "novelist" => Some(include_str!("directions/novelist.md")),
        "auditor" => Some(include_str!("directions/auditor.md")),
        "committer" => Some(include_str!("directions/committer.md")),
        "chapter-reviewer" => Some(include_str!("directions/chapter-reviewer.md")),
        "polisher" => Some(include_str!("directions/polisher.md")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavors::TIANQUAN_SOUL_KINDS;

    #[test]
    fn all_sixteen_kinds_have_direction_prompt() {
        for kind in TIANQUAN_SOUL_KINDS {
            let prompt = direction_prompt_for_kind(kind)
                .unwrap_or_else(|| panic!("missing direction prompt for kind {kind}"));
            assert!(!prompt.trim().is_empty(), "empty direction prompt for kind {kind}");
        }
    }

    #[test]
    fn unknown_kind_returns_none() {
        assert!(direction_prompt_for_kind("nonexistent").is_none());
        assert!(direction_prompt_for_kind("general").is_none()); // ironclaw 内置,非天权
    }

    #[test]
    fn novelist_direction_contains_p1_discipline() {
        let prompt = direction_prompt_for_kind("novelist").unwrap();
        // novelist SOUL 应含 P1-P12 大白话原则(探查确认)
        assert!(
            prompt.contains("P1") || prompt.contains("大白话") || prompt.contains("VERIFY"),
            "novelist direction should contain P1-P12/大白话/VERIFY discipline"
        );
    }
}
