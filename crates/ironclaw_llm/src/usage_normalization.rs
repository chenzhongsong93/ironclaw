//! 通用provider解析边界候选；不含TQ金额/账户/账本，不重释旧存量usage。
#[cfg(test)]
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Serialize, Deserialize))]
#[cfg_attr(test, serde(rename_all = "camelCase"))]
pub(crate) enum UsageNormalizationVersion {
    InputTotalIncludesCacheV1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Serialize, Deserialize))]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub(crate) struct NormalizedUsage {
    version: UsageNormalizationVersion,
    cache_details_observed: bool,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageUnknown {
    MissingOrInvalidField,
    Overflow,
    ConflictingCacheFields,
    UnsupportedLegacy,
}
fn field(v: &Value, key: &str) -> Result<u32, UsageUnknown> {
    let n = v
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(UsageUnknown::MissingOrInvalidField)?;
    u32::try_from(n).map_err(|_| UsageUnknown::Overflow)
}
fn checked(
    input: u32,
    output: u32,
    read: u32,
    create: u32,
) -> Result<NormalizedUsage, UsageUnknown> {
    if read.checked_add(create).ok_or(UsageUnknown::Overflow)? > input {
        return Err(UsageUnknown::ConflictingCacheFields);
    }
    Ok(NormalizedUsage {
        version: UsageNormalizationVersion::InputTotalIncludesCacheV1,
        cache_details_observed: true,
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: read,
        cache_creation_input_tokens: create,
    })
}
/// Only the actual Anthropic Messages parser may call this. The three ORIGINAL
/// input buckets are independent; do not pass rig's already-normalized token_usage.
pub(crate) fn parse_anthropic_messages_usage(
    raw_usage: &Value,
) -> Result<NormalizedUsage, UsageUnknown> {
    let uncached = field(raw_usage, "input_tokens")?;
    let read = field(raw_usage, "cache_read_input_tokens")?;
    let create = field(raw_usage, "cache_creation_input_tokens")?;
    let input = uncached
        .checked_add(read)
        .and_then(|n| n.checked_add(create))
        .ok_or(UsageUnknown::Overflow)?;
    checked(input, field(raw_usage, "output_tokens")?, read, create)
}
/// Only a factory-bound DeepSeek Chat response parser may call this. prompt_tokens
/// already includes cache hits. Native and OpenAI-details aliases must agree.
pub(crate) fn parse_deepseek_chat_usage(
    raw_usage: &Value,
) -> Result<NormalizedUsage, UsageUnknown> {
    let total = field(raw_usage, "prompt_tokens")?;
    let native = raw_usage
        .get("prompt_cache_hit_tokens")
        .map(|_| field(raw_usage, "prompt_cache_hit_tokens"))
        .transpose()?;
    let details = raw_usage
        .get("prompt_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .map(|v| {
            v.as_u64()
                .ok_or(UsageUnknown::MissingOrInvalidField)
                .and_then(|n| u32::try_from(n).map_err(|_| UsageUnknown::Overflow))
        })
        .transpose()?;
    let read = match (native, details) {
        (Some(a), Some(b)) if a != b => return Err(UsageUnknown::ConflictingCacheFields),
        (Some(a), _) | (_, Some(a)) => a,
        (None, None) => return Err(UsageUnknown::MissingOrInvalidField),
    };
    if raw_usage.get("prompt_cache_miss_tokens").is_some()
        && field(raw_usage, "prompt_cache_miss_tokens")?.checked_add(read) != Some(total)
    {
        return Err(UsageUnknown::ConflictingCacheFields);
    }
    checked(total, field(raw_usage, "completion_tokens")?, read, 0)
}
impl NormalizedUsage {
    pub(crate) fn cache_details_observed(self) -> bool {
        self.cache_details_observed
    }
    pub(crate) fn parts(self) -> (u32, u32, u32, u32, Option<u16>) {
        (
            self.input_tokens,
            self.output_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
            Some(match self.version {
                UsageNormalizationVersion::InputTotalIncludesCacheV1 => 1,
            }),
        )
    }
}
/// Rig's actual associated raw response TYPE selects the protocol contract.
/// Model names, server-provided type strings and today's provider settings are
/// never used to reinterpret old stored usage.
pub(crate) fn from_rig_response<R: Serialize + 'static>(
    raw: &R,
) -> Result<NormalizedUsage, UsageUnknown> {
    use std::any::TypeId;
    let value = serde_json::to_value(raw).map_err(|_| UsageUnknown::MissingOrInvalidField)?;
    let usage = value
        .get("usage")
        .ok_or(UsageUnknown::MissingOrInvalidField)?;
    if TypeId::of::<R>()
        == TypeId::of::<rig::providers::anthropic::completion::CompletionResponse>()
    {
        return parse_anthropic_messages_usage(usage);
    }
    if TypeId::of::<R>() == TypeId::of::<rig::providers::deepseek::CompletionResponse>() {
        return parse_deepseek_chat_usage(usage);
    }
    if TypeId::of::<R>() == TypeId::of::<rig::providers::openai::CompletionResponse>() {
        let input = field(usage, "prompt_tokens")?;
        // rig-core 0.33's ACTUAL OpenAI raw Usage retains prompt+total,
        // not completion_tokens. The protocol total is input+output; reject
        // underflow instead of manufacturing zero or relying on SDK defaults.
        let output = field(usage, "total_tokens")?
            .checked_sub(input)
            .ok_or(UsageUnknown::ConflictingCacheFields)?;
        let retained_cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|v| v.get("cached_tokens"))
            .map(|v| {
                v.as_u64()
                    .ok_or(UsageUnknown::MissingOrInvalidField)
                    .and_then(|n| u32::try_from(n).map_err(|_| UsageUnknown::Overflow))
            })
            .transpose()?;
        let read = retained_cache_read.unwrap_or(0);
        // OpenAI prompt_tokens is already total input. An unknown extension
        // claiming an independent creation bucket is not silently reclassified.
        if usage
            .get("cache_creation_input_tokens")
            .is_some_and(|v| v.as_u64() != Some(0))
        {
            return Err(UsageUnknown::ConflictingCacheFields);
        }
        return checked(input, output, read, 0).map(|mut value| {
            // rig PromptTokensDetails.cached_tokens has serde(default). Zero
            // cannot distinguish omitted from explicitly reported; positive
            // retained count is a real presence witness. Total remains Known.
            value.cache_details_observed = retained_cache_read.is_some_and(|n| n > 0);
            value
        });
    }
    Err(UsageUnknown::UnsupportedLegacy)
}
impl NormalizedUsage {
    /// Same version persists into the original run row AND terminal event.
    /// All four original dimensions remain available; cache is never added twice.
    #[cfg(test)]
    pub(crate) fn checked_accumulate(self, next: Self) -> Result<Self, UsageUnknown> {
        checked(
            self.input_tokens
                .checked_add(next.input_tokens)
                .ok_or(UsageUnknown::Overflow)?,
            self.output_tokens
                .checked_add(next.output_tokens)
                .ok_or(UsageUnknown::Overflow)?,
            self.cache_read_input_tokens
                .checked_add(next.cache_read_input_tokens)
                .ok_or(UsageUnknown::Overflow)?,
            self.cache_creation_input_tokens
                .checked_add(next.cache_creation_input_tokens)
                .ok_or(UsageUnknown::Overflow)?,
        )
    }
}
/// This is the runtime accumulator state, not a persisted old-value guess.
/// Only the original absence of any call is Unobserved. Some(0) legacy is Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(crate) enum UsageObservation {
    Unobserved,
    Unknown,
    Known(NormalizedUsage),
}
#[cfg(test)]
impl UsageObservation {
    pub(crate) fn observe(&mut self, next: Result<NormalizedUsage, UsageUnknown>) {
        *self = match (*self, next) {
            (Self::Unknown, _) | (_, Err(_)) => Self::Unknown,
            (Self::Unobserved, Ok(value)) => Self::Known(value),
            (Self::Known(old), Ok(value)) => old
                .checked_accumulate(value)
                .map(Self::Known)
                .unwrap_or(Self::Unknown),
        };
    }
}
/// Missing historical normalization provenance is not zero and is not inferred
/// from today's provider id, cache amounts, file timestamp, or current code version.
#[cfg(test)]
pub(crate) fn decode_versioned_usage(stored: &Value) -> Result<NormalizedUsage, UsageUnknown> {
    if stored.get("version").is_none() {
        return Err(UsageUnknown::UnsupportedLegacy);
    }
    let value: NormalizedUsage =
        serde_json::from_value(stored.clone()).map_err(|_| UsageUnknown::MissingOrInvalidField)?;
    checked(
        value.input_tokens,
        value.output_tokens,
        value.cache_read_input_tokens,
        value.cache_creation_input_tokens,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn anthropic_buckets_are_independent_and_creation_can_exceed_uncached() {
        let raw = json!({"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":100});
        let n = parse_anthropic_messages_usage(&raw).unwrap();
        assert_eq!(
            (
                n.input_tokens,
                n.output_tokens,
                n.cache_read_input_tokens,
                n.cache_creation_input_tokens
            ),
            (109, 3, 7, 100)
        );
    }
    #[test]
    fn deepseek_hits_are_subset_not_added_twice_and_aliases_match() {
        let raw = json!({"prompt_tokens":100,"completion_tokens":3,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30,"prompt_tokens_details":{"cached_tokens":70}});
        let n = parse_deepseek_chat_usage(&raw).unwrap();
        assert_eq!(
            (
                n.input_tokens,
                n.output_tokens,
                n.cache_read_input_tokens,
                n.cache_creation_input_tokens
            ),
            (100, 3, 70, 0)
        );
        let mut bad = raw;
        bad["prompt_tokens_details"]["cached_tokens"] = json!(71);
        assert_eq!(
            parse_deepseek_chat_usage(&bad),
            Err(UsageUnknown::ConflictingCacheFields)
        );
    }
    #[test]
    fn missing_negative_overflow_and_historical_unknown_are_not_zero() {
        assert_eq!(
            decode_versioned_usage(&json!({"input_tokens":0,"output_tokens":0})),
            Err(UsageUnknown::UnsupportedLegacy)
        );
        assert!(
            parse_anthropic_messages_usage(&json!({"input_tokens":0,"output_tokens":0})).is_err()
        );
        assert!(parse_anthropic_messages_usage(&json!({"input_tokens":-1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})).is_err());
        assert!(parse_anthropic_messages_usage(&json!({"input_tokens":4294967295_u64,"output_tokens":0,"cache_read_input_tokens":1,"cache_creation_input_tokens":0})).is_err());
    }
    #[test]
    fn unknown_zero_call_cannot_be_reset_by_later_known_usage() {
        let zero = parse_anthropic_messages_usage(&json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})).unwrap();
        let mut fresh = UsageObservation::Unobserved;
        fresh.observe(Ok(zero));
        assert_eq!(fresh, UsageObservation::Known(zero));
        let mut legacy = UsageObservation::Unknown;
        legacy.observe(Ok(zero));
        assert_eq!(legacy, UsageObservation::Unknown);
        fresh.observe(Err(UsageUnknown::UnsupportedLegacy));
        fresh.observe(Ok(zero));
        assert_eq!(fresh, UsageObservation::Unknown);
    }
    #[test]
    fn explicit_zero_is_known_and_checked_accumulation_retains_version() {
        let zero=parse_anthropic_messages_usage(&json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})).unwrap();
        let n = parse_deepseek_chat_usage(
            &json!({"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":4}),
        )
        .unwrap();
        assert_eq!(zero.checked_accumulate(n).unwrap(), n);
        assert_eq!(
            decode_versioned_usage(&serde_json::to_value(n).unwrap()).unwrap(),
            n
        );
    }

    #[test]
    fn actual_rig_anthropic_raw_type_keeps_legacy_four_and_normalizes_once() {
        let raw:rig::providers::anthropic::completion::CompletionResponse=serde_json::from_value(json!({
            "id":"fixture-message","model":"fixture-model","role":"assistant","type":"message",
            "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":100}
        })).unwrap();
        let original: rig::completion::CompletionResponse<_> = raw.try_into().unwrap();
        assert_eq!(original.usage.input_tokens, 2); // Actual locked SDK conversion, not a mock DTO.
        let canonical = from_rig_response(&original.raw_response).unwrap();
        assert_eq!(canonical.parts(), (109, 3, 7, 100, Some(1)));
        assert_eq!(original.usage.input_tokens, 2);
    }
    #[test]
    fn actual_rig_openai_usage_requires_present_totals_and_never_double_adds_cache() {
        fn raw(usage: Value) -> rig::providers::openai::CompletionResponse {
            serde_json::from_value(json!({"id":"fixture-completion","object":"chat.completion","created":0,
                "model":"fixture-model","system_fingerprint":null,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop","logprobs":null}],"usage":usage})).unwrap()
        }
        let original = raw(
            json!({"prompt_tokens":100,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":70}}),
        );
        assert_eq!(
            from_rig_response(&original).unwrap().parts(),
            (100, 3, 70, 0, Some(1))
        );
        assert!(from_rig_response(&raw(Value::Null)).is_err());
        for usage in [
            json!({"prompt_tokens":100}),
            json!({"total_tokens":103}),
            json!({"prompt_tokens":null,"total_tokens":103}),
        ] {
            assert!(serde_json::from_value::<rig::providers::openai::CompletionResponse>(json!({"id":"fixture-completion","object":"chat.completion","created":0,"model":"fixture-model","system_fingerprint":null,"choices":[],"usage":usage})).is_err());
        }
        // SDK drops vendor hit/miss extensions. Their absence from retained
        // details leaves breakdown Unknown, without invalidating observed prompt/output totals.
        let sdk_dropped_vendor_details = from_rig_response(&raw(json!({"prompt_tokens":100,"total_tokens":103,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30}))).unwrap();
        assert_eq!(sdk_dropped_vendor_details.parts(), (100, 3, 0, 0, Some(1)));
        assert!(!sdk_dropped_vendor_details.cache_details_observed());
        let known_total_no_breakdown =
            from_rig_response(&raw(json!({"prompt_tokens":0,"total_tokens":0}))).unwrap();
        assert!(!known_total_no_breakdown.cache_details_observed());
        let defaulted_cache = from_rig_response(&raw(
            json!({"prompt_tokens":100,"total_tokens":103,"prompt_tokens_details":{}}),
        ))
        .unwrap();
        assert_eq!(defaulted_cache.parts(), (100, 3, 0, 0, Some(1)));
        assert!(!defaulted_cache.cache_details_observed());
        assert_eq!(from_rig_response(&raw(json!({"prompt_tokens":0,"total_tokens":0,"prompt_tokens_details":{"cached_tokens":0}}))).unwrap().parts(), (0,0,0,0,Some(1)));
        assert!(from_rig_response(&raw(json!({"prompt_tokens":100,"total_tokens":99}))).is_err());
    }
}
