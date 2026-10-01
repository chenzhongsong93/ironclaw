//! Untrusted child->parent text framing, moved out of the deleted
//! `completion_observer.rs` verbatim (§4.6: "only edit to that file beyond
//! §3's extraction: `wrap_untrusted_subagent_text` -> `pub(crate)`" — since
//! the whole file is deleted, its one surviving item gets its own small
//! module rather than an empty-shell file).

use ironclaw_threads::ToolResultSafeSummary;
use ironclaw_turns::run_profile::sanitize_model_visible_text;

/// Wrap untrusted subagent-authored strings in explicit `|||...|||`
/// delimiters before they enter the capability result store or the parent's
/// transcript. `sanitize_tool_result_summary` already strips structural
/// characters, but downstream consumers that surface the field into model
/// context gain defense-in-depth framing against prompt-injection payloads.
pub(crate) fn wrap_untrusted_subagent_text(value: String) -> String {
    // Pipe delimiters survive `sanitize_tool_result_summary` (which strips
    // `< > { } [ ] \` and similar structural chars). Without that property
    // the wrapper would be silently erased by the final re-sanitization
    // step in `parent_result_summary`.
    format!("|||{}|||", value)
}

/// 仅用于父scope拥有的可读结果体；保留格式与全文，不作为安全摘要。
/// 凭据标记过滤沿用模型可见文本策略，数据仍封装为不可信子输出。
pub(crate) fn complete_untrusted_subagent_result(value: &str) -> String {
    wrap_untrusted_subagent_text(sanitize_model_visible_text(value.to_owned()))
}

pub(crate) fn sanitize_untrusted_terminal_reason(value: &str) -> String {
    let mut safe = sanitize_untrusted_text_body(value);
    if safe.len() > 512 {
        truncate_to_char_boundary(&mut safe, 512);
    }
    wrap_untrusted_subagent_text(safe)
}

pub(crate) fn sanitize_tool_result_summary(value: String) -> String {
    let mut safe = sanitize_untrusted_text_body(&value);
    if safe.len() > 512 {
        truncate_to_char_boundary(&mut safe, 512);
    }
    if ToolResultSafeSummary::new(safe.clone()).is_ok() {
        safe
    } else {
        "Subagent result available".to_string()
    }
}

fn sanitize_untrusted_text_body(value: &str) -> String {
    let sanitized = sanitize_model_visible_text(value.to_string())
        .chars()
        .map(|character| match character {
            '{' | '}' | '[' | ']' | '`' | '<' | '>' | '/' | '\\' => ' ',
            character if character == '\0' || character.is_control() => ' ',
            character => character,
        })
        .collect::<String>();
    let mut collapsed = String::new();
    for part in sanitized.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(part);
    }
    collapsed
}

fn truncate_to_char_boundary(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_result_retains_json_and_line_breaks_but_filters_credentials() {
        let text = "{\"refs\":[\"urn:one\"]}\n正文\nraw_provider_secret";
        let result = complete_untrusted_subagent_result(text);
        assert!(result.starts_with("|||"));
        assert!(result.ends_with("|||"));
        assert!(result.contains("{\"refs\":[\"urn:one\"]}\n正文\n"));
        assert!(!result.contains("raw_provider_secret"));
        assert!(result.contains("[redacted]"));
    }

    #[test]
    fn wraps_in_pipe_delimiters() {
        assert_eq!(wrap_untrusted_subagent_text("x".to_string()), "|||x|||");
    }

    #[test]
    fn sanitize_strips_structural_characters_and_collapses_whitespace() {
        let out = sanitize_tool_result_summary("hello <script>  world\t\n".to_string());
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert_eq!(out, "hello script world");
    }
}
