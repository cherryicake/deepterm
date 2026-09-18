//! CLI agent 支持：识别跑在前台的是哪个 agent、跟踪它的回合状态、
//! 并通过 hook 让它把事件报回来。
//!
//! 模块划分：
//!
//! - [`cli_agent`]：agent 识别、状态机、OSC 777 事件协议。纯数据层，无副作用。
//! - [`hooks`]：把 hook 装进 agent 的配置文件。同样是纯数据层——它只负责算出
//!   「文件该长什么样」，真正的读写由调用方（`terminal_view`）做，本地与远程
//!   两条路径共用同一份计划逻辑。非 Claude 适配器的 hook / 插件统一调用它生成
//!   的**上报脚本**（纯 POSIX shell，写进 `$HOME` 固定位置）来归一化事件并写入
//!   `/dev/tty`；Claude 保留宿主代发 `terminalSequence` 的独立传输方式。

pub mod cli_agent;
pub mod hooks;

pub use cli_agent::{
    AGENT_EVENT_SENTINEL, AgentEvent, AgentEventKind, AgentSessionState, AgentStatus, CLIAgent,
    command_argv, parse_agent_event,
};
