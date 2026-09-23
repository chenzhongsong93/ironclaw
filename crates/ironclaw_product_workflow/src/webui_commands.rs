//! WebChat 可执行的只读命令目录与执行共用同一注册表。
use crate::{
    RebornGetRunStateRequest, RebornServicesApi, RebornServicesError, RebornTimelineRequest,
    WebUiAuthenticatedCaller,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WebUiCommandDescriptor {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Clone, Copy)]
enum ReadCommand {
    Help,
    Version,
    Ping,
    Status,
    Skills,
}

const REGISTRY: &[(ReadCommand, WebUiCommandDescriptor)] = &[
    (
        ReadCommand::Help,
        WebUiCommandDescriptor {
            name: "help",
            aliases: &[],
            description: "查看当前可执行命令",
        },
    ),
    (
        ReadCommand::Version,
        WebUiCommandDescriptor {
            name: "version",
            aliases: &[],
            description: "查看命令服务组件版本",
        },
    ),
    (
        ReadCommand::Ping,
        WebUiCommandDescriptor {
            name: "ping",
            aliases: &[],
            description: "检查命令服务连通性",
        },
    ),
    (
        ReadCommand::Status,
        WebUiCommandDescriptor {
            name: "status",
            aliases: &["progress"],
            description: "查看当前会话最近一次运行状态",
        },
    ),
    (
        ReadCommand::Skills,
        WebUiCommandDescriptor {
            name: "skills",
            aliases: &[],
            description: "查看当前运行时技能目录",
        },
    ),
];

pub fn webui_command_descriptors() -> Vec<WebUiCommandDescriptor> {
    REGISTRY
        .iter()
        .map(|(_, descriptor)| descriptor.clone())
        .collect()
}

pub async fn execute_runtime_read_command(
    services: &dyn RebornServicesApi,
    caller: WebUiAuthenticatedCaller,
    name: &str,
    arguments: &str,
    thread_id: Option<String>,
) -> Result<Option<String>, RebornServicesError> {
    let Some((command, _)) = REGISTRY
        .iter()
        .find(|(_, descriptor)| descriptor.name == name || descriptor.aliases.contains(&name))
    else {
        return Ok(None);
    };
    if !arguments.trim().is_empty() {
        return Ok(Some(format!("`/{name}` 不接受参数。请输入 `/{name}`。")));
    }
    let output = match command {
        ReadCommand::Help => REGISTRY
            .iter()
            .map(|(_, descriptor)| format!("- `/{}` — {}", descriptor.name, descriptor.description))
            .collect::<Vec<_>>()
            .join("\n"),
        ReadCommand::Version => format!("IronClaw 命令服务组件 v{}", env!("CARGO_PKG_VERSION")),
        ReadCommand::Ping => "pong".to_string(),
        ReadCommand::Skills => {
            let skills = services.list_skills(caller).await?;
            if skills.skills.is_empty() {
                "当前运行时没有可用技能。".to_string()
            } else {
                skills
                    .skills
                    .iter()
                    .map(|skill| format!("- `${}` — {}", skill.name, skill.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        ReadCommand::Status => {
            let Some(thread_id) = thread_id else {
                return Ok(Some(
                    "尚未选择会话。发送创作消息或从历史记录选择一个会话后，可查看运行状态。"
                        .to_string(),
                ));
            };
            let timeline = services
                .get_timeline(
                    caller.clone(),
                    RebornTimelineRequest {
                        thread_id: thread_id.clone(),
                        limit: Some(200),
                        cursor: None,
                    },
                )
                .await?;
            let latest = timeline
                .messages
                .iter()
                .filter(|message| message.turn_run_id.is_some())
                .max_by_key(|message| message.sequence);
            match latest.and_then(|message| message.turn_run_id.clone()) {
                Some(run_id) => {
                    let state = services
                        .get_run_state(caller, RebornGetRunStateRequest { thread_id, run_id })
                        .await?;
                    format!(
                        "- 最近运行：`{}`\n- 运行状态：`{:?}`\n- 接收时间：{}",
                        state.run_id, state.status, state.received_at
                    )
                }
                None => "当前会话尚无运行记录。".to_string(),
            }
        }
    };
    Ok(Some(output))
}
