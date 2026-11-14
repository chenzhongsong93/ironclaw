//! Explicit per-request headers for the Anthropic Bearer transport.
use crate::error::LlmError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;

enum Value {
    Literal(HeaderValue),
    Metadata(String),
}

pub(super) struct RequestHeaders(Vec<(HeaderName, Value)>);

fn invalid(code: &'static str) -> LlmError {
    LlmError::InvalidRequest {
        provider: "anthropic_oauth".into(),
        code,
    }
}

fn header_value(value: &str) -> Result<HeaderValue, LlmError> {
    let mut value =
        HeaderValue::from_str(value).map_err(|_| invalid("invalid_request_header_value"))?;
    // Debug output must not expose request identity or configured private values.
    value.set_sensitive(true);
    Ok(value)
}

impl RequestHeaders {
    pub(super) fn new(entries: &[(String, String)]) -> Result<Self, LlmError> {
        let mut parsed = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("invalid_request_header_name"))?;
            if matches!(
                name.as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "cookie"
                    | "set-cookie"
                    | "x-api-key"
                    | "api-key"
                    | "x-goog-api-key"
                    | "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
            ) {
                return Err(invalid("protected_request_header"));
            }
            let value = if value.contains("{metadata:") {
                let key = value
                    .strip_prefix("{metadata:")
                    .and_then(|s| s.strip_suffix('}'))
                    .filter(|key| {
                        !key.is_empty()
                            && key.len() <= 64
                            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .ok_or_else(|| invalid("invalid_request_header_template"))?;
                Value::Metadata(key.to_owned())
            } else {
                Value::Literal(header_value(value)?)
            };
            parsed.push((name, value));
        }
        Ok(Self(parsed))
    }

    pub(super) fn resolve(
        &self,
        metadata: &HashMap<String, String>,
    ) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.0 {
            let value = match value {
                Value::Literal(value) => value.clone(),
                Value::Metadata(key) => {
                    let value = metadata
                        .get(key)
                        .filter(|v| !v.trim().is_empty())
                        .ok_or_else(|| invalid("missing_request_header_metadata"))?;
                    header_value(value)?
                }
            };
            headers.insert(name, value);
        }
        Ok(headers)
    }
}
