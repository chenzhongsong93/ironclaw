//! 通用宿主HTTP/MCP有限等待预算，不包含产品或创作方法论。

use std::time::Duration;

const MAX_TIMEOUT_MS: u32 = 900_000;
const HOST_TIMEOUT_ENV: &str = "IRONCLAW_REBORN_HOST_HTTP_TIMEOUT_MS";
const MCP_TIMEOUT_ENV: &str = "IRONCLAW_REBORN_MCP_TIMEOUT_MS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostHttpTimeouts {
    pub host_timeout_ms: u32,
    pub mcp_timeout_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("宿主HTTP/MCP等待预算必须是1至900000毫秒的整数")]
pub struct HostHttpTimeoutError;

impl HostHttpTimeouts {
    pub fn parse(host: Option<&str>, mcp: Option<&str>) -> Result<Self, HostHttpTimeoutError> {
        Ok(Self {
            host_timeout_ms: parse_limit(host, 30_000)?,
            mcp_timeout_ms: parse_limit(mcp, 60_000)?,
        })
    }

    pub fn from_process_env() -> Result<Self, HostHttpTimeoutError> {
        let host = read_optional_env(HOST_TIMEOUT_ENV)?;
        let mcp = read_optional_env(MCP_TIMEOUT_ENV)?;
        Self::parse(host.as_deref(), mcp.as_deref())
    }

    pub fn host_timeout(self) -> Duration {
        Duration::from_millis(u64::from(self.host_timeout_ms))
    }

    pub fn effective_mcp_timeout_ms(self) -> u32 {
        self.host_timeout_ms.min(self.mcp_timeout_ms)
    }
}

fn parse_limit(value: Option<&str>, default: u32) -> Result<u32, HostHttpTimeoutError> {
    let Some(value) = value else {
        return Ok(default);
    };
    let limit = value.parse::<u32>().map_err(|_| HostHttpTimeoutError)?;
    if !(1..=MAX_TIMEOUT_MS).contains(&limit) {
        return Err(HostHttpTimeoutError);
    }
    Ok(limit)
}

fn read_optional_env(name: &str) -> Result<Option<String>, HostHttpTimeoutError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(HostHttpTimeoutError),
    }
}
