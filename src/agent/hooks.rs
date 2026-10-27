//! 把 hook 装进各 agent 的配置文件。
//!
//! # 为什么必须走 `terminalSequence` 而不是往 stdout 打 OSC
//!
//! Claude Code 的 hook **没有控制终端**。官方 hooks 文档（*Hook input and output* 节）：
//!
//! > On macOS and Linux, command hooks run in their own session without a
//! > controlling terminal. The hook process and any child processes can't open
//! > `/dev/tty` or send escape sequences directly to the Claude Code interface.
//!
//! 所以「hook 里 `printf '\e]777;...\a'`」这条路是死的——hook 的 stdout 被
//! Claude Code 收进 debug log，永远流不到 pty。唯一可用的是 hook JSON 输出里的
//! `terminalSequence` 字段，**由 Claude Code 自己**把字节写进它自己的终端。
//!
//! 该字段的**白名单**（同一文档 *Emit terminal notifications* 节）：
//!
//! > - OSC `0`, `1`, `2`: window and icon titles
//! > - OSC `9`: iTerm2, ConEmu, Windows Terminal, and WezTerm notifications,
//! >   including `9;4` taskbar progress
//! > - OSC `99`: Kitty notifications
//! > - OSC `777`: urxvt, Ghostty, and Warp notifications
//! > - Bare BEL
//! >
//! > Sequences may be terminated with BEL or with ST. Anything outside the
//! > allowlist, including CSI cursor and color sequences, OSC palette
//! > sequences, OSC 8 hyperlinks, OSC 52 clipboard writes, and **OSC 1337**,
//! > is rejected and the field is ignored.
//!
//! 两个直接后果：
//!
//! 1. **OSC 777 在白名单里**，所以 [`crate::agent::AGENT_EVENT_SENTINEL`] 那套
//!    现有信道可以原样复用，本模块不需要新协议。
//! 2. **OSC 1337 被明确拒绝**，所以**不能**照抄 shell 集成里
//!    `OSC 1337;Proc=` 的写法（`assets/shell-integration/zsh/deepterm-integration:126`）。
//!    这是最容易踩的坑。
//!
//! 越界即**整段字段被丢弃**（不是截断），所以一个 hook 只能发一种序列。
//!
//! # CodeBuddy 为什么不在本模块的安装范围内
//!
//! CodeBuddy 的 hook 系统与 Claude Code 兼容（同样的 settings.json 形状、同样的
//! 事件名），但**没有 `terminalSequence`**，因此它的 hook 无法发出任何 OSC 序列。
//! 已验证三次：官方文档通篇没有该字段；`grep -rl terminalSequence` 扫遍
//! v2.151.0 的整个 bundle 零命中；其自身的通知路径还有实现缺陷。
//!
//! CodeBuddy 不在 tty7 的 hook 适配集合中，保留现有解释弹窗。
//! 其余适配支持范围由 [`INSTALLABLE_AGENTS`] 明确列出。

use crate::agent::{AGENT_EVENT_SENTINEL, AgentEventKind, CLIAgent};

/// 安装计划被拒绝的原因。
///
/// 只有 `Unparsable` 一种，且它是**刻意的**：用户文件可能正被别的工具写、
/// 或本来就有 JSONC 注释，我们唯一能保证安全的动作是**什么都不做**。
/// 写坏别人的配置是这里最严重的失败模式，所以宁可拒绝也不「尽力而为」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPlanError {
    /// 用户现有的 settings.json 不是合法 JSON。
    ///
    /// 调用方应当把这条**如实告诉用户**（带文件名），而不是静默失败——
    /// 用户需要知道该去修哪个文件。
    Unparsable,
    /// 顶层不是 JSON 对象（是数组、字符串、数字…）。
    NotAnObject,
}

/// deepterm 在 Claude Code 里安装的 hook 事件集合。
///
/// # 为什么是这几个
///
/// - `PreToolUse` 装，但脚本里手写分支：只有 `AskUserQuestion` 上报成
///   `question-asked`，普通工具调用直接丢掉。它是唯一不进权限提示的提问
///   通道，漏了它 tab 会一直显示"工作中"而 agent 其实在等用户回答。
/// - `StopFailure` 必须装：用户按 Esc 打断时 Claude 发的是它而不是 `Stop`，
///   不装的话状态点会**永远卡在 Working/蓝色**，看起来就是个 bug。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookEventSpec {
    /// Claude Code 侧的事件名（`~/.claude/settings.json` 里 `hooks` 的键）。
    pub claude_event: &'static str,
    /// 该事件上报成 deepterm 的哪种 `AgentEventKind`。
    ///
    /// `StopFailure` 映射成 [`AgentEventKind::Stop`] 而不是
    /// [`AgentEventKind::Notification`]，这是**经过推理的**而非随手选的：
    /// `apply_event` 里 `Notification` 只在当前是 `Working` 时才转 `Waiting`
    /// （见 `cli_agent.rs`），若用户是在权限弹窗（`Waiting`）上按 Esc 打断，
    /// 状态会**停在 `Waiting`**，状态点依旧不归位。映射成 `Stop`（→`Done`）
    /// 才在所有起手状态下都正确收尾——用户打断了，这个回合确实结束了。
    pub kind: AgentEventKind,
}

impl HookEventSpec {
    const fn plain(claude_event: &'static str, kind: AgentEventKind) -> Self {
        Self { claude_event, kind }
    }
}

/// deepterm 为 Claude Code 安装的全部 hook 事件。
pub const CLAUDE_HOOK_EVENTS: &[HookEventSpec] = &[
    HookEventSpec::plain("SessionStart", AgentEventKind::SessionStart),
    HookEventSpec::plain("UserPromptSubmit", AgentEventKind::PromptSubmit),
    HookEventSpec::plain("PermissionRequest", AgentEventKind::PermissionRequest),
    HookEventSpec::plain("PreToolUse", AgentEventKind::QuestionAsked),
    HookEventSpec::plain("PostToolUse", AgentEventKind::ToolComplete),
    HookEventSpec::plain("Stop", AgentEventKind::Stop),
    // 用户按 Esc 打断。没有它，状态点会卡在蓝色不归位。
    HookEventSpec::plain("StopFailure", AgentEventKind::Stop),
    HookEventSpec::plain("SessionEnd", AgentEventKind::SessionEnd),
];

/// 哪些 agent 支持「装一个东西就能上报状态」。
///
/// 两个**不同**的机制，但对用户是同一件事（点一下安装，然后状态点就亮了）：
///
/// - **Claude Code**：写 `~/.claude/settings.json` 的 hook。它没有控制终端，
///   所以 hook 只能把序列交给宿主的 `terminalSequence` 字段代发。
/// - **opencode / pi**：写一个插件/扩展文件。它们跑在 agent 自己的进程里，
///   子进程可以自己写 `/dev/tty`，所以上报交给自带的**上报脚本**
///   （[`agent_report_script`]）完成——纯 shell、本地/远端同一份，
///   不要求 `deepterm` 在 PATH 里。
///
/// 与 tty7 的 hook 适配器一致；识别支持不代表存在状态事件源。
pub const INSTALLABLE_AGENTS: &[CLIAgent] = &[
    CLIAgent::Claude,
    CLIAgent::Codex,
    CLIAgent::TraeCode,
    CLIAgent::Copilot,
    CLIAgent::OpenCode,
    CLIAgent::Pi,
    CLIAgent::Grok,
    CLIAgent::OhMyPi,
    CLIAgent::Gemini,
    CLIAgent::Droid,
    CLIAgent::Qwen,
    CLIAgent::Goose,
    CLIAgent::Kimi,
    CLIAgent::QoderCLI,
    CLIAgent::QoderCLICn,
    CLIAgent::Crush,
    CLIAgent::Empryo,
    CLIAgent::Cursor,
    CLIAgent::Jcode,
];

/// 该不该弹「装 hook」的窗。
///
/// 三条必须**同时**满足，缺一不可：
///
/// - **agent 在 [`INSTALLABLE_AGENTS`] 里**：其余 agent 没有可装的机制。
///   CodeBuddy 是特例——它两边都不支持（见模块文档），调用方对它走解释窗
///   （无安装按钮），不要在这里返回 `true`。
/// - **这次进程里还没问过**：用户选「暂不」就不再骚扰。状态只存在内存里，
///   重启 deepterm 后允许再问一次（这是用户明确要求的语义）。
/// - **当前还没装**：装好了就没什么可问的。
pub fn should_prompt_hook_install(
    agent: Option<CLIAgent>,
    already_asked: bool,
    installed: bool,
) -> bool {
    agent.is_some_and(|a| INSTALLABLE_AGENTS.contains(&a)) && !already_asked && !installed
}

/// 一个 agent 要装的东西：写到哪、写什么、怎么判断装过了。
///
/// `installed_marker` 是**文件内容里必须出现**的字符串。用它判「装过了」而不是
/// 只看文件在不在：这几个文件（`settings.json`、`plugins/`）都是用户/别的工具
/// 也会写的共享位置，文件存在不代表我们那份在里面。
pub struct HookInstallPlan {
    /// 相对 `$HOME` 的文件路径。
    pub rel_path: &'static str,
    /// 文件内容。
    pub content: String,
    /// 已安装判定用的标记。
    pub installed_marker: &'static str,
    /// 可执行位？脚本类需要，插件/配置类不需要。
    pub executable: bool,
}

/// 算出某 agent 的安装计划；这个 agent 没有可装的东西时返回 `None`。
///
/// 纯函数——只算内容，不碰文件系统。读写由调用方做（本地/远程两条路径
/// 共用同一份计划），这与 `plan_install` 的分工一致。
pub fn hook_install_plan(agent: CLIAgent) -> Option<HookInstallPlan> {
    match agent {
        // Claude 的「两件事」——settings.json 里挂钩子、以及钩子要跑的那个
        // 脚本——没有塞进一个 `HookInstallPlan`，因为它们是两个不同路径的
        // 文件、且 settings.json 需要**合并**而非覆盖。Claude 走的仍是
        // `plan_install` 那条老路，见 `terminal_view::install_claude_hook`。
        CLIAgent::Claude => None,
        CLIAgent::OpenCode => Some(HookInstallPlan {
            rel_path: OPENCODE_PLUGIN_REL_PATH,
            content: opencode_plugin_js(),
            installed_marker: OPENCODE_PLUGIN_MARKER,
            executable: false,
        }),
        CLIAgent::Pi => Some(HookInstallPlan {
            rel_path: PI_EXTENSION_REL_PATH,
            content: pi_extension_ts(),
            installed_marker: PI_EXTENSION_MARKER,
            executable: false,
        }),
        other => adapter_plan(other),
    }
}

/// tty7 的事件表；不要把缺失的回合/权限事件猜测为存在。
fn adapter_events(agent: CLIAgent) -> &'static [(&'static str, &'static str)] {
    use CLIAgent::*;
    match agent {
        // Codex 的 `PermissionRequest` 无输出应答时会落进它自己的审批提示，
        // 于是"在等用户"这件事能被报出来而不替它做决定。`PostToolUse` 负责
        // 把授权通过后的状态从 Waiting 拉回 Working；`PreToolUse` 兜住
        // `request_user_input`——那个工具不走权限审批。
        Codex => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PermissionRequest", "permission-request"),
            ("PreToolUse", "tool-start"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
        ],
        TraeCode => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PermissionRequest", "permission-request"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("SessionEnd", "session-end"),
        ],
        // Qwen 是唯一有一等权限事件的 agent，所以既不需要 `Notification` 嗅探、
        // 也**不该**装 `Notification`——那只会把专属事件已经精确报出的状态搅浑。
        // 早先它与 TraeCode 共用一张表，白白多了一条噪音源。
        Qwen => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PermissionRequest", "permission-request"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("SessionEnd", "session-end"),
        ],
        Gemini => &[
            ("SessionStart", "session-start"),
            ("BeforeAgent", "prompt-submit"),
            ("Notification", "notification"),
            ("AfterTool", "tool-complete"),
            ("AfterAgent", "stop"),
            ("SessionEnd", "session-end"),
        ],
        Droid | Grok => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("Notification", "notification"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("SessionEnd", "session-end"),
        ],
        Goose => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("SessionEnd", "session-end"),
        ],
        Kimi => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PermissionRequest", "permission-request"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("Interrupt", "stop"),
            ("StopFailure", "stop"),
            ("SessionEnd", "session-end"),
        ],
        QoderCLI => QODER_EVENTS,
        Crush => &[("PreToolUse", "tool-complete")],
        Copilot => &[
            ("sessionStart", "session-start"),
            ("userPromptSubmitted", "prompt-submit"),
            ("agentStop", "stop"),
            ("sessionEnd", "session-end"),
            ("notification", "notification"),
        ],
        // 中国大陆版 Qoder 与国际版同一套 hook 形状，只是配置目录不同
        // （`~/.qoder-cn`），事件表完全共用。
        QoderCLICn => QODER_EVENTS,
        // Cursor 的 hooks.json 是扁平的（`command`/`matcher` 在顶层，不嵌套
        // `hooks` 数组），且事件名是小驼峰。`stop` 的载荷带 `status`
        // （completed/aborted/error），三种都算回合结束。
        //
        // 打印模式（`cursor-agent -p`）不发 `beforeSubmitPrompt` 也不发
        // `stop`（Cursor 官方确认的缺口），所以 `-p` 会话能记下 session id
        // 但没有状态点。
        Cursor => &[
            ("sessionStart", "session-start"),
            ("beforeSubmitPrompt", "prompt-submit"),
            ("postToolUse", "tool-complete"),
            ("stop", "stop"),
            ("sessionEnd", "session-end"),
        ],
        // Empryo 用 Claude 兼容的生命周期钩子（同款事件名、同款
        // `~/.empryo/hooks.json`）。`StopFailure` 必须装：不装的话出错的
        // 回合会把状态点永久留在蓝色。
        Empryo => &[
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PostToolUse", "tool-complete"),
            ("Stop", "stop"),
            ("StopFailure", "stop"),
            ("SessionEnd", "session-end"),
        ],
        // jcode 的钩子是 `~/.jcode/config.toml` 里 `[hooks]` 下的条目，
        // 事件名是蛇形；它以分离的观察者身份跑钩子，事件描述走
        // `JCODE_HOOK_*` 环境变量而不是 stdin。
        Jcode => &[
            ("session_start", "session-start"),
            ("turn_start", "prompt-submit"),
            ("post_tool", "tool-complete"),
            ("turn_end", "stop"),
            ("session_end", "session-end"),
        ],
        _ => &[],
    }
}

/// Qoder 国际版与中国大陆版共用的 hook 事件表。
///
/// `Elicitation` 映射成 `question-asked`：一个已授权的 MCP 工具仍可能在
/// 调用中途停下问用户，那不是权限问题，是提问。
const QODER_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("PermissionRequest", "permission-request"),
    ("Elicitation", "question-asked"),
    ("PostToolUse", "tool-complete"),
    ("Stop", "stop"),
    ("StopFailure", "stop"),
    ("SessionEnd", "session-end"),
];

fn adapter_plan(agent: CLIAgent) -> Option<HookInstallPlan> {
    use CLIAgent::*;
    // 每个共享配置文件只服务一个 agent，所以清理标记用 REPORTER_MARKER
    // （不带 slug）就够：删光本文件里我们所有条目再重写，幂等。
    let rel_path = match agent {
        Codex => ".codex/hooks.json",
        TraeCode => ".trae/cli/hooks.json",
        Copilot => ".copilot/hooks/deepterm.json",
        Grok => ".grok/hooks/deepterm.json",
        OhMyPi => ".omp/agent/extensions/deepterm/index.ts",
        Gemini => ".gemini/settings.json",
        Droid => ".factory/settings.json",
        Qwen => ".qwen/settings.json",
        Goose => ".agents/plugins/deepterm/hooks/hooks.json",
        Kimi => ".kimi-code/config.toml",
        QoderCLI => ".qoder/settings.json",
        QoderCLICn => ".qoder-cn/settings.json",
        Crush => ".config/crush/crush.json",
        Empryo => ".empryo/hooks.json",
        Cursor => ".cursor/hooks.json",
        Jcode => ".jcode/config.toml",
        _ => return None,
    };
    let content = if agent == OhMyPi {
        pi_extension_for("omp", "@oh-my-pi/pi-coding-agent")
    } else if agent == Kimi {
        merge_kimi_hooks("", "").expect("empty TOML is valid")
    } else if agent == Jcode {
        merge_jcode_hooks("").expect("empty TOML is valid")
    } else {
        // Cursor 的 hooks.json 是扁平的：`command`/`matcher` 直接在条目顶层，
        // 不嵌套 `hooks` 数组（Crush 也是这个形状）。它还会忽略没有
        // `"version": 1` 根键的文件，所以必须补上。
        let flat = matches!(agent, Cursor | Crush);
        let mut hooks = serde_json::Map::new();
        for &(event, wire) in adapter_events(agent) {
            let command = reporter_command(agent.slug(), wire);
            let entry = if agent == Copilot {
                serde_json::json!({"type": "command", "bash": command, "timeoutSec": if event == "agentStop" { 10 } else { 5 }})
            } else if flat {
                serde_json::json!({"command": command})
            } else {
                let mut group =
                    serde_json::json!({"hooks": [{"type": "command", "command": command}]});
                if agent == Grok {
                    group["hooks"][0]["timeout"] = 10.into();
                    if event == "Notification" {
                        group["matcher"] = "elicitation_dialog".into();
                    }
                }
                group
            };
            hooks.insert(event.into(), serde_json::json!([entry]));
        }
        let mut root = serde_json::json!({"hooks": hooks});
        // Cursor 要求 version 字段；Copilot 的 hooks.json 同样带 version。
        if matches!(agent, Cursor | Copilot) {
            root["version"] = 1.into();
        }
        serde_json::to_string_pretty(&root).expect("hook JSON serializes")
    };
    Some(HookInstallPlan {
        rel_path,
        installed_marker: REPORTER_MARKER,
        content,
        executable: false,
    })
}

fn uses_shared_json(agent: CLIAgent) -> bool {
    matches!(
        agent,
        CLIAgent::Codex
            | CLIAgent::TraeCode
            | CLIAgent::Gemini
            | CLIAgent::Droid
            | CLIAgent::Qwen
            | CLIAgent::QoderCLI
            | CLIAgent::QoderCLICn
            | CLIAgent::Crush
            | CLIAgent::Empryo
            | CLIAgent::Cursor
    )
}

/// 只替换 deepterm 自己的 handler，保留同一 matcher 下第三方 handler。
///
/// 清理时同时匹配**新版**标记（上报脚本）与**旧版**标记（`deepterm
/// agent-hook` 命令）：升级安装的用户配置里可能还留着旧条目，不清掉会
/// 新旧两套同时上报，事件翻倍。
pub fn merge_bridge_config(
    agent: CLIAgent,
    plan: &HookInstallPlan,
    existing: &[u8],
) -> Result<String, String> {
    let legacy = format!("{} {}", LEGACY_CLI_PREFIX, agent.slug());
    let is_ours = |s: &str| s.contains(plan.installed_marker) || s.contains(&legacy);
    if agent == CLIAgent::Kimi {
        return merge_kimi_hooks(
            std::str::from_utf8(existing).map_err(|e| e.to_string())?,
            &legacy,
        );
    }
    if agent == CLIAgent::Jcode {
        return merge_jcode_hooks(std::str::from_utf8(existing).map_err(|e| e.to_string())?);
    }
    if !uses_shared_json(agent) {
        return Ok(plan.content.clone());
    }
    let mut root: serde_json::Value = if existing.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(existing).map_err(|e| format!("配置不是合法 JSON：{e}"))?
    };
    let generated: serde_json::Value =
        serde_json::from_str(&plan.content).map_err(|e| e.to_string())?;
    // Cursor 会**整个忽略**没有根级 `version` 的 hooks.json，所以合并时必须
    // 把 version 补上（哪怕是首次安装、配置原本是空的）。
    if let Some(version) = generated.get("version").cloned() {
        root.as_object_mut()
            .ok_or("配置顶层不是对象")?
            .insert("version".into(), version);
    }
    let hooks = root
        .as_object_mut()
        .ok_or("配置顶层不是对象")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("hooks 不是对象")?;
    for entries in hooks.values_mut() {
        let entries = entries.as_array_mut().ok_or("hook 事件不是数组")?;
        entries.retain_mut(|entry| {
            if entry
                .get("command")
                .and_then(|v| v.as_str())
                .is_some_and(&is_ours)
            {
                return false;
            }
            if let Some(handlers) = entry.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                let before = handlers.len();
                handlers.retain(|h| {
                    !h.get("command")
                        .and_then(|v| v.as_str())
                        .is_some_and(&is_ours)
                });
                return before == handlers.len() || !handlers.is_empty();
            }
            true
        });
    }
    for (event, entries) in generated["hooks"]
        .as_object()
        .ok_or("生成的 hooks 不是对象")?
    {
        hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or("hook 事件不是数组")?
            .extend(
                entries
                    .as_array()
                    .ok_or("生成的 hook 事件不是数组")?
                    .iter()
                    .cloned(),
            );
    }
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

fn merge_kimi_hooks(existing: &str, legacy: &str) -> Result<String, String> {
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("配置不是合法 TOML：{e}"))?;
    if doc
        .get("hooks")
        .and_then(|v| v.as_array())
        .is_some_and(|v| v.is_empty())
    {
        doc.remove("hooks");
    }
    let list = doc
        .entry("hooks")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ))
        .as_array_of_tables_mut()
        .ok_or("hooks 不是 TOML 表数组")?;
    list.retain(|entry| {
        let cmd = entry.get("command").and_then(|v| v.as_str());
        cmd.is_none_or(|s| !(s.contains(REPORTER_MARKER) || s.contains(legacy)))
    });
    for &(event, wire) in adapter_events(CLIAgent::Kimi) {
        let mut entry = toml_edit::Table::new();
        entry["event"] = toml_edit::value(event);
        entry["command"] = toml_edit::value(reporter_command("kimi", wire));
        list.push(entry);
    }
    Ok(doc.to_string())
}

/// jcode 的钩子合并：`[hooks]` 下的**键值对**，每个键的值是一条命令或
/// 一个命令列表（与 Kimi 的 `[[hooks]]` 表数组是两种形状）。
///
/// jcode 以分离的观察者身份跑这些钩子，事件描述走 `JCODE_HOOK_*` 环境变量
/// 而不是 stdin，所以上报脚本的 stdin 读取会自然落空——载荷由环境变量补，
/// 这件事在 `agent_report_script` 侧处理。
fn merge_jcode_hooks(existing: &str) -> Result<String, String> {
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("配置不是合法 TOML：{e}"))?;
    let table = doc
        .entry("hooks")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or("hooks 不是 TOML 表")?;
    let is_ours = |s: &str| s.contains(REPORTER_MARKER) || s.contains(LEGACY_CLI_PREFIX);
    // 清掉我们自己的旧条目，保留用户与第三方的。
    //
    // `toml_edit` 把 TOML 数组也归到 `Item::Value`（`Value::Array`），而 jcode
    // 对「一条命令或一个命令列表」两种形态都接受。字符串直接删键；数组只删
    // 我们那几个元素，删空了的键才整个删掉——否则会把用户并排挂的第三方
    // 通知一起抹掉。
    let stale: Vec<String> = table
        .iter()
        .filter(|(_, value)| {
            let toml_edit::Item::Value(v) = value else {
                return false;
            };
            match v {
                toml_edit::Value::String(s) => is_ours(s.value()),
                toml_edit::Value::Array(arr) => arr
                    .iter()
                    .any(|item| matches!(item, toml_edit::Value::String(s) if is_ours(s.value()))),
                _ => false,
            }
        })
        .map(|(k, _)| k.to_string())
        .collect();
    for key in stale {
        let keep_foreign = table
            .get(&key)
            .and_then(|v| v.as_array())
            .is_some_and(|arr| {
                arr.iter()
                    .any(|item| !matches!(item, toml_edit::Value::String(s) if is_ours(s.value())))
            });
        if keep_foreign {
            // 只摘掉我们的元素，保留其余。
            if let Some(arr) = table.get_mut(&key).and_then(|v| v.as_array_mut()) {
                arr.retain(
                    |item| !matches!(item, toml_edit::Value::String(s) if is_ours(s.value())),
                );
            }
        } else {
            table.remove(&key);
        }
    }
    for &(event, wire) in adapter_events(CLIAgent::Jcode) {
        // 键已存在时**追加**，别把用户/第三方的命令覆盖掉：数组直接追加元素，
        // 字符串则升级成「用户命令 + 我方命令」的数组（jcode 两种形态都接受）。
        let command = reporter_command("jcode", wire);
        match table.get(event).cloned() {
            Some(toml_edit::Item::Value(toml_edit::Value::Array(mut arr))) => {
                arr.push(toml_edit::Value::from(command));
                table[event] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
            }
            Some(toml_edit::Item::Value(toml_edit::Value::String(old))) => {
                let mut arr = toml_edit::Array::new();
                arr.push(toml_edit::Value::String(old));
                arr.push(toml_edit::Value::from(command));
                table[event] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
            }
            _ => {
                table[event] = toml_edit::value(command);
            }
        }
    }
    Ok(doc.to_string())
}

/// 与 tty7 相同的本地配置目录覆盖；远端不使用本机环境变量。
pub fn local_bridge_path(agent: CLIAgent, home: &std::path::Path, rel: &str) -> std::path::PathBuf {
    let value = |key| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    let xdg = || value("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    match agent {
        CLIAgent::Claude => value("CLAUDE_CONFIG_DIR").map(|p| p.join("settings.json")),
        CLIAgent::OpenCode => Some(xdg().join("opencode/plugins/deepterm.js")),
        CLIAgent::TraeCode => value("TRAECLI_HOME")
            .map(|p| p.join("hooks.json"))
            .or_else(|| value("TRAE_HOME").map(|p| p.join("cli/hooks.json"))),
        CLIAgent::Kimi => value("KIMI_CODE_HOME").map(|p| p.join("config.toml")),
        CLIAgent::QoderCLI => value("QODER_CONFIG_DIR").map(|p| p.join("settings.json")),
        CLIAgent::Crush => Some(
            value("CRUSH_GLOBAL_CONFIG")
                .unwrap_or_else(|| xdg().join("crush"))
                .join("crush.json"),
        ),
        _ => None,
    }
    .unwrap_or_else(|| home.join(rel))
}

/// 完整性检测：共享配置允许第三方条目，缺少任一必需事件则需要重装。
pub fn bridge_installed(agent: CLIAgent, plan: &HookInstallPlan, existing: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(existing) else {
        return false;
    };
    if agent == CLIAgent::Kimi {
        return merge_kimi_hooks(text, &format!("{} kimi", LEGACY_CLI_PREFIX))
            .is_ok_and(|merged| merged == text);
    }
    if agent == CLIAgent::Jcode {
        return jcode_hooks_present(text);
    }
    if !uses_shared_json(agent) {
        return text == plan.content;
    }
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Ok(expected) = serde_json::from_str::<serde_json::Value>(&plan.content) else {
        return false;
    };
    // Cursor 会忽略没有根级 `version` 的 hooks.json，所以版本也是完整性的一部分。
    let version_ok = expected
        .get("version")
        .is_none_or(|wanted| root.get("version") == Some(wanted));
    version_ok
        && expected["hooks"].as_object().is_some_and(|hooks| {
            hooks.iter().all(|(event, entries)| {
                root["hooks"][event].as_array().is_some_and(|actual| {
                    entries
                        .as_array()
                        .is_some_and(|wanted| wanted.iter().all(|entry| actual.contains(entry)))
                })
            })
        })
}

/// jcode 的完整性检测：只要我们的事件命令都还在就算装好了。
///
/// 不能整段文本比较——用户的 `config.toml` 里还有自己的模型/权限设置，
/// 合并结果必然不同于我们生成的模板。
fn jcode_hooks_present(text: &str) -> bool {
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    let Some(hooks) = doc.get("hooks").and_then(|v| v.as_table()) else {
        return false;
    };
    adapter_events(CLIAgent::Jcode)
        .iter()
        .all(|&(event, wire)| {
            let wanted = reporter_command("jcode", wire);
            match hooks.get(event).and_then(|v| v.as_value()) {
            Some(toml_edit::Value::String(s)) => s.value().as_str() == wanted.as_str(),
            Some(toml_edit::Value::Array(arr)) => arr.iter().any(|item| {
                matches!(item, toml_edit::Value::String(s) if s.value().as_str() == wanted.as_str())
            }),
            _ => false,
        }
        })
}

/// 「装 hook」系统弹窗里显示的**精简文案**。
///
/// # 为什么不列文件清单
///
/// 安装提示走系统原生弹窗（NSAlert），适合一到两句话：用户关心的是
/// 「装了能得到什么、装完还要做什么」，而不是实现细节（写到哪个文件）。
/// 文件路径与写入方式在安装实现（`hook_install_plan` / `plan_install`）里
/// 都有权威定义，弹窗里复述一份只会多一处会过期的数据。
///
/// # 「要不要用户去确认」这件事因 agent 而异
///
/// - Claude 写两个文件（脚本 + `settings.json`），而且**必须**去跑一次
///   `/hooks` 才生效——这是 Claude Code 的信任门（见模块文档）。
/// - opencode / pi 只写一个插件文件，**没有**确认步骤，重启 agent 即生效。
/// - Codex 要写 `hooks.json`，并额外开一次 `features enable hooks`；那一步由
///   安装流程自己完成，用户同样只需要重启。
///
/// 早先这段文案是硬编码的 Claude 版，于是给 opencode 弹窗时会告诉用户去运行
/// `/hooks`——那是错的，opencode 里没有这个命令。把它做成纯函数是因为
/// 「有没有确认步」正是最容易写错又最好断言的部分。
pub struct HookInstallCopy {
    /// 标题。例：`为 OpenCode 启用状态指示？`
    pub title: String,
    /// 一句话说明。例：`安装插件后，终端标签上会实时显示 ... 的状态：...`。
    pub blurb: String,
    /// 「还需要做点什么」那行；没有后续步骤时为 `None`（opencode / pi）。
    pub after_install: Option<String>,
}

/// 算出某个 agent 的弹窗文案；这个 agent 不该弹窗时返回 `None`。
pub fn hook_install_copy(agent: CLIAgent) -> Option<HookInstallCopy> {
    let name = agent.display_name();
    match agent {
        CLIAgent::Claude => Some(HookInstallCopy {
            title: format!("为 {name} 启用状态指示？"),
            blurb: format!(
                "安装 hook 后，终端标签上会实时显示 {name} 的状态：\
                 工作中 / 等待授权 / 已完成。"
            ),
            // Claude Code 的信任门：不跑 `/hooks` 不生效。这一行不能省，
            // 否则用户会以为装完就没反应、以为是 deepterm 坏了。
            after_install: Some(format!(
                "写入后需要在 {name} 里运行 /hooks 确认一次才会生效。"
            )),
        }),
        // opencode / pi：只写一个插件文件，没有确认步骤。
        other => {
            // 仍要求有安装计划：不可安装的 agent 不该拿到弹窗文案。
            hook_install_plan(other)?;
            Some(HookInstallCopy {
                title: format!("为 {name} 启用状态指示？"),
                blurb: match other {
                    CLIAgent::Crush => {
                        "Crush 目前只有工具活动事件，不报告完整回合或等待状态。".into()
                    }
                    CLIAgent::Codex | CLIAgent::Goose => format!(
                        "安装后显示 {name} 的工作中 / 已完成状态；上游适配不提供权限等待事件。"
                    ),
                    _ => format!(
                        "安装后按 {name} 提供的事件同步回合状态；部分版本不提供权限等待事件。"
                    ),
                },
                // Codex 的 `hooks` 特性由安装流程自己开（见
                // `codex_enable_hooks_command`），不需要用户再跑命令。
                after_install: Some(format!("重启 {name} 后生效。")),
            })
        }
    }
}

/// 扫描一份 settings.json，判断 deepterm 的 hook 是否已经装上。
///
/// 只看 `hooks.<Event>[].hooks[].command`，**不碰**文件里其它任何字段——
/// 用户的 settings.json 里装着大量我们不该理解的内容（`permissions`、`model`、
/// 别的工具写的标记如 Otty 的 `_otty`）。
///
/// 解析失败一律返回 `false`（＝未安装）：不能因为读不懂用户的文件就断言装过，
/// 那会让用户永远看不到安装引导。这里的误判方向是「多问一次」，代价可接受。
pub fn has_deepterm_hook(settings: &[u8]) -> bool {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(settings) else {
        return false;
    };
    let Some(hooks) = root.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    hooks.values().any(|groups| {
        groups
            .as_array()
            .map(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|handlers| {
                            handlers.iter().any(|handler| {
                                handler
                                    .get("command")
                                    .and_then(|c| c.as_str())
                                    // 命令里出现哨兵即为我们所装。哨兵同时也出现在
                                    // hook 脚本的正文里，所以这一条对整个 command
                                    // 字符串做子串匹配就够了。
                                    .is_some_and(|c| c.contains(AGENT_EVENT_SENTINEL))
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// 把 deepterm 的 hook 并进用户的 settings.json，返回**合并后**的完整文件内容。
///
/// # 三条硬约束
///
/// 1. **解析不了就拒绝写**（[`HookPlanError`]）。写坏别人的配置是这里最严重的
///    失败模式。
/// 2. **只动 `hooks` 这一个键**，其余原样保留。本机实测
///    `~/.claude/settings.json` 里有 Otty 装的 hook（带 `_otty` 标记），
///    整段覆盖会把别人的集成干掉。
/// 3. **幂等**：已经有 deepterm 的 entry 就**替换**它，不追加第二份。重复 entry
///    会让同一个事件跑两次 hook——不只是浪费，还会让状态机收到重复事件。
///
/// 返回 `Vec<u8>` 而非 `String` 是为了让远程路径可以原样复用：远程要把它
/// 当作**字节**经 stdin 送给 `cat > file`，多一次 UTF-8 编解码就多一个失败点。
pub fn plan_install(settings: &[u8]) -> Result<Vec<u8>, HookPlanError> {
    let mut root: serde_json::Value =
        serde_json::from_slice(settings).map_err(|_| HookPlanError::Unparsable)?;
    let obj = root.as_object_mut().ok_or(HookPlanError::NotAnObject)?;

    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    // `hooks` 存在但不是对象（用户写成了数组）——同样拒绝，因为我们唯一能做的
    // 「修复」是把它覆盖掉，那是在毁用户的数据。
    let hooks = hooks.as_object_mut().ok_or(HookPlanError::NotAnObject)?;

    for spec in CLAUDE_HOOK_EVENTS {
        let groups = hooks
            .entry(spec.claude_event)
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let groups = groups.as_array_mut().ok_or(HookPlanError::NotAnObject)?;

        // 命令必须**带上哨兵作为参数**，有两重作用：
        //
        // 1. 它让这条命令可以被识别为「deepterm 装的」——`has_deepterm_hook` 与
        //    下面的去重都靠这个子串。缺了它，重复安装会不断累积 entry，
        //    同一个事件于是跑两次 hook。
        // 2. 让用户在 `settings.json` 里一眼看出这行是谁写的。
        //
        // 事件名**作为字面量直接写进命令**，不能靠 `"$1"`：Claude Code 执行的是
        // 命令字符串本身，不会额外传参数，`$1` 在那里是空的。脚本按 `$1` 分派，
        // 所以这一处写错会表现为「所有事件都落到未知分支」——静默无状态。
        //
        // `$HOME` 用引号包住，避免主目录含空格时被拆词；`[ -x ]` 守卫让脚本
        // 被删掉时 hook 静默退出，不会让 Claude Code 报错。
        let script_path = format!("$HOME/{HOOK_SCRIPT_REL_DIR}/{HOOK_SCRIPT_FILE_NAME}");
        let command = format!(
            "[ -x \"{script_path}\" ] && \"{script_path}\" {} {AGENT_EVENT_SENTINEL}",
            spec.claude_event
        );
        let entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command,
            }]
        });

        // 先摘掉我们自己以前写的所有 entry（按 `command` 里是否含哨兵识别），
        // 再 push 一份新的。这样重复安装不会累积，且**只**影响我们的 entry。
        let is_ours = |group: &serde_json::Value| -> bool {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|handlers| {
                    handlers.iter().any(|handler| {
                        handler
                            .get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.contains(AGENT_EVENT_SENTINEL))
                    })
                })
                .unwrap_or(false)
        };
        groups.retain(|group| !is_ours(group));
        groups.push(entry);
    }

    serde_json::to_vec_pretty(&root).map_err(|_| HookPlanError::Unparsable)
}

/// 本机 `settings.json` 的路径（相对用户主目录）。
pub const SETTINGS_REL_PATH: &str = ".claude/settings.json";

/// hook 脚本放哪（相对用户主目录）。
///
/// 放在 `~/.deepterm/hooks/` 而不是 `~/.claude/` 里，是为了让「卸载」只需删
/// settings.json 里的 entry——不必去翻 Claude 自己的目录。
pub const HOOK_SCRIPT_REL_DIR: &str = ".deepterm/hooks";

/// hook 脚本文件名。
pub const HOOK_SCRIPT_FILE_NAME: &str = "claude-hook.sh";

/// 生成 hook 脚本的完整内容。
///
/// # 为什么是一个 sh 脚本而不是一行命令
///
/// 有两处噪声只能靠**读 hook 的 stdin JSON** 才能区分，而读 stdin 就不是
/// 一行 `printf` 能做的事了：
///
/// 1. **subagent 让状态点提前变绿**。Claude 的主 agent 让出、后台 subagent
///    还在跑时**也会发一次 `Stop`**，此时回合并没结束。Stop 的载荷里会列出
///    仍在运行的 subagent（`background_tasks` 中 `"type":"subagent"` +
///    `"status":"running"`）。只装 `Stop`、不装 `SubagentStop` **不能**解决
///    ——作祟的就是 `Stop` 本身。
/// 2. **`StopFailure` 需要单独映射**（见 [`CLAUDE_HOOK_EVENTS`] 的说明）。
///
/// 这两条是同款参考实现（Otty 的 `otty-hook.sh`）里踩过的坑，注释写得很清楚：
///
/// > A Task subagent runs in the background: while it works, the main agent
/// > often yields and Claude fires a Stop … even though the turn isn't really
/// > over … Reporting that as idle made Otty fire a completion notification
/// > (and idle badge) for every subagent.
///
/// # 脚本里的 JSON 为什么是手写的而不是让 Rust 生成
///
/// 脚本要跑在**远程主机**上，远端不一定有 deepterm 二进制。所以载荷里的
/// 定值部分（`v` / `agent` / `event`）直接写成字面量，而
/// [`claude_terminal_sequence`] 用同一份数据在 Rust 侧生成一份，
/// `hook_script_payloads_round_trip` 测试保证两者一致。
pub fn hook_script() -> String {
    let mut cases = String::new();
    // 去重**必须按事件名**做，不能靠字符串 `contains` 判断——`Stop` 是
    // `StopFailure` 的子串，任何子串式的判重都会让 `StopFailure` 意外命中。
    // 这里用显式的已见集合。
    let mut seen: Vec<&'static str> = Vec::new();
    // `Stop` **必须排除**：它在下面脚本模板里已经手写了一个带
    // `background_tasks` 判定的分支。再生成一个就成了重复的 case 模式，
    // 后一个永远不可达，而且会让 `case "^Stop$"` 的语义变得依赖顺序。
    // `PreToolUse` 同理：它要按工具名挑，只有提问才上报。
    let hand_written = ["Stop", "PreToolUse"];
    for spec in CLAUDE_HOOK_EVENTS {
        if hand_written.contains(&spec.claude_event) {
            continue;
        }
        if seen.contains(&spec.claude_event) {
            continue;
        }
        seen.push(spec.claude_event);
        let payload = esc_for_shell_single_quoted(&claude_terminal_sequence(spec.kind));
        // 每个分支都自带结尾 `;;`，包括最后一个 `*)` 之前的那一个——
        // POSIX sh 里 `;;` 是分支终止符，漏掉会报语法错。
        cases.push_str(&format!(
            "\n  {event})\n    emit '{payload}' ;;\n",
            event = spec.claude_event
        ));
    }

    format!(
        r#"#!/bin/sh
# deepterm — Claude Code hook。
#
# 把 agent 的回合状态报回终端，让 Tab 上的图标能显示 工作中/等待授权/已完成。
# 由 deepterm 自动写入，可安全删除（删掉后 deepterm 不会再引导安装）。
#
# 上报方式必须用 hook JSON 的 terminalSequence 字段：hook 没有控制终端，
# 直接 printf 到 stdout 会被 Claude Code 收进 debug log，到不了终端。
# 该字段的白名单含 OSC 777，但不含 OSC 1337。
#
# Claude Code 把 hook 载荷以 JSON 从 stdin 送入；本脚本按事件名（$1）分派。

event="$1"

# 先读掉 stdin：不读的话 Claude Code 写管道会在我们退出前收到 SIGPIPE。
# 顺带用于下面判定 Stop 是否真的收尾（subagent 的情况）。
input=$(cat)

# 生成 {{"terminalSequence":"..."}}。OSC 777 的载荷形状必须与
# parse_agent_event 的硬剥前缀一致：`777;notify;<sentinel>;<json>` + BEL。
#
# 实参 `$1` 是**纯文本**序列（控制字符写成 shell 的 \033 / \007）。这里分三步：
#   1. `printf '%b'` 把 \033 / \007 展开成**真实字节**；
#   2. `sed` 把这些真实字节换成 JSON 转义 `\u001b` / `\u0007`，
#      并把内层 `"` 转义成 `\"`——因为整段要作为**外层 JSON 的字符串值**，
#      JSON 不允许字符串里出现裸控制字符或未转义的引号；
#   3. 拼进 `{{"terminalSequence":"..."}}`。
#
# 顺序很关键：转义必须发生在 `%b` 展开**之后**。先前试过让实参直接带
# `\"` 再由 `%b` 展开，结果 `%b` 把 `\"` 原样吐出、内层 JSON 出现
# `{{\"v\":1}}` 这种脏数据——`%b` 只处理反斜杠转义，不会理解 JSON 语义。
#
# 为什么不引 jq：hook 要在**远程主机**上跑，不能假设远端装了 jq。
# `printf` + `sed` 是 POSIX 最低公共分母。
#
# 为什么 ESC/BEL 要经变量传给 sed 而不是写 `\x1b`：macOS 的 BSD sed
# **不支持** `\x` 转义，模式匹配不到任何东西、ESC 原样穿透，产出非法
# JSON（Linux 的 GNU sed 才认 `\x`）。把真实字节经 shell 变量拼进 sed
# 脚本，两边都工作。
emit() {{
  dt_esc=$(printf '\033')
  dt_bel=$(printf '\007')
  seq=$(printf '%b' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' \
        -e "s/$dt_esc/\\\\u001b/g" -e "s/$dt_bel/\\\\u0007/g")
  printf '{{"terminalSequence":"%s"}}' "$seq"
}}

case "$event" in
  Stop)
    # 主 agent 让出但后台 subagent 还在跑时也会发 Stop。此时报「完成」会让
    # 每个 subagent 都点亮一次绿点，所以这种 Stop 直接吞掉。
    # 先把所有空白删掉再匹配，以兼容 pretty-printed JSON 和两种键序。
    # 老版本 Claude 没有 background_tasks 字段 → 不匹配 → 照常上报，
    # 行为与没有这段判断时一致。
    compact=$(printf '%s' "$input" | tr -d ' \t\n')
    case "$compact" in
      *'"type":"subagent","status":"running"'*) exit 0 ;;
      *'"status":"running","type":"subagent"'*) exit 0 ;;
    esac
    emit '{stop_payload}'
    ;;
  PreToolUse)
    # 只有 AskUserQuestion 会挡住 agent 等用户回答，而且它**不走权限提示**，
    # 所以它是唯一能告诉我们「在等提问」的事件。别的工具调用只是中间态，
    # 直接丢掉，否则状态点会在 Working/Waiting 之间乱抖。
    # 先删掉所有空白再匹配，以兼容 pretty-printed JSON。
    compact=$(printf '%s' "$input" | tr -d ' \t\n')
    case "$compact" in
      *'"tool_name":"AskUserQuestion"'*) emit '{question_payload}' ;;
      *) exit 0 ;;
    esac
    ;;
{cases}  *)
    # 未知事件（例如 Claude Code 新加的事件被这里漏掉）静默忽略。
    ;;
esac
"#,
        stop_payload = esc_for_shell_single_quoted(&claude_terminal_sequence(AgentEventKind::Stop)),
        question_payload =
            esc_for_shell_single_quoted(&claude_terminal_sequence(AgentEventKind::QuestionAsked),),
        cases = cases,
    )
}

/// 生成**上报脚本**（`~/.deepterm/hooks/agent-report.sh`）的内容。
///
/// 这是所有非 Claude agent 的 hook 命令的实际执行者（`reporter_command`）：
/// 读 stdin 上的事件 JSON → 过滤/路由 → 拼 OSC 777 序列 → 写进控制终端。
/// 它是纯 POSIX shell——
///
/// # 为什么必须是纯 shell
///
/// 上报曾经由一个 `deepterm agent-hook` 子命令完成，那要求 `deepterm` 在
/// PATH 里。本地没问题（deepterm 给自己的 PTY 注入了二进制所在目录），但
/// **远端**的 hook 跑在 agent 的会话里，而非交互 SSH shell 的 PATH 不含用户
/// 安装目录，且「远程装 deepterm」本身就是个不该有的前置条件。脚本随安装写到
/// `$HOME` 固定相对位置，本地/远端同一份，hook 命令引用 `~/…` 由执行方 shell
/// 展开，PATH 从此无关。那条子命令已删除。
///
/// # 与旧通路的取舍
///
/// - 事件白名单、`DEEPTERM` 门控、stdin 上限、unknown 静默：一一保留。
/// - 事件路由（qodercli compact 抑制、四家 agent 的 Notification→
///   PermissionRequest 嗅探）：`case` 重写，语义一致。
/// - 字段提取从 serde_json 解析降级为 best-effort 的 `grep`/`sed`：取首个
///   `"key":"value"`，**嵌不进载荷的值直接丢弃**（引号/反斜杠/控制字节）。
///   每个字段都是可选的，坏载荷比缺字段更糟——终端解析器会整条丢弃序列。
/// - `/dev/tty` 失败后沿 `ps` 找祖先 tty：同样照旧通路移植。
pub fn agent_report_script() -> String {
    format!(
        r#"#!/bin/sh
# deepterm agent-report — generated by deepterm, do not edit.
#
# Reads a hook-event JSON payload on stdin and reports it to the controlling
# terminal as an OSC 777 sequence. Pure POSIX shell: no deepterm binary, no
# jq — works wherever the agent runs, local or remote.
#
# argv: <agent-slug> <event-name>
# Inert outside deepterm: gated on the $DEEPTERM variable, which deepterm
# sets on every pane it owns (local and SSH).

[ -n "$DEEPTERM" ] || exit 0

agent="$1"
event="$2"

# Unknown slugs/events are silently ignored: agent upgrades add new event
# names, and an old reporter must stay quiet rather than error out.
case "$event" in
  session-start|prompt-submit|permission-request|question-asked|tool-complete|notification|stop|session-end|tool-start) ;;
  *) exit 0 ;;
esac
case "$agent" in
  codex|traecli|copilot|opencode|pi|grok|omp|gemini|droid|qwen|goose|kimi|qodercli|qoderclicn|crush|empryo|cursor|jcode) ;;
  *) exit 0 ;;
esac

# stdin cap: the terminal tokenizer abandons an overlong OSC entirely rather
# than truncating it, so read less rather than more. And when stdin IS the
# terminal (the hook piped nothing), reading would block the agent.
if [ -t 0 ]; then
  input=""
else
  input=$(head -c 65536 2>/dev/null)
fi

# jcode leaves stdin closed and describes the event in the environment
# instead: the JSON mirror it exports as JCODE_HOOK_PAYLOAD (session_id, cwd,
# and the event's own fields). Reading stdin would block forever, so the
# payload comes from there.
case "$agent:$event" in
  jcode:*)
    case "$input" in
      "") input="${{JCODE_HOOK_PAYLOAD:-}}" ;;
    esac
    ;;
esac

# Event routing that used to live in the `deepterm agent-hook` binary.
#
# Codex's request_user_input stops a turn to ask the user something without
# going through a permission prompt, so `tool-start` is the only sign the turn
# is now waiting on the user. Without this the tab stays on "working" while
# the agent sits blocked on a question. (Claude's AskUserQuestion is handled
# in Claude's own hook script, which never reaches this one.)
#
# Every other tool drops the event entirely: `tool-start` is an intermediate,
# not a status, and `parse_agent_event` refuses it by design — emitting it
# unchanged would leave a payload the terminal cannot read.
case "$agent:$event" in
  codex:tool-start)
    compact=$(printf '%s' "$input" | tr -d ' \t\n')
    case "$compact" in
      *'"tool_name":"request_user_input"'*) event="question-asked" ;;
      *) exit 0 ;;
    esac
    ;;
esac

case "$agent:$event" in
  qodercli:session-start)
    # Context compaction also fires SessionStart; it is not a new session.
    case "$input" in *'"source":"compact"'*) exit 0 ;; esac
    ;;
  copilot:notification|grok:notification|droid:notification|gemini:notification)
    # These agents signal permission prompts through their generic
    # Notification event; every other notification is noise.
    case "$input" in
      *elicitation_dialog*) event="permission-request" ;;
      *permission_prompt*)
        case "$agent" in copilot|droid) event="permission-request" ;; *) exit 0 ;; esac ;;
      *ToolPermission*)
        case "$agent" in gemini) event="permission-request" ;; *) exit 0 ;; esac ;;
      *) exit 0 ;;
    esac ;;
esac

# Best-effort extraction of optional string fields, without jq: the first
# "key":"value" match. Values that could not be embedded safely (embedded
# quotes, backslashes, control bytes) are dropped rather than escaped —
# every field here is optional, and a broken payload is worse than a
# missing one: the terminal parser abandons the whole sequence.
json_str() {{
  value=$(printf '%s' "$input" | grep -o "\"$1\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -n 1 | sed "s/^\"$1\"[[:space:]]*:[[:space:]]*\"//;s/\"\$//")
  case "$value" in ""|*'"'*|*'\'*) return 1 ;; esac
  case "$value" in *[[:cntrl:]]*) return 1 ;; esac
  printf '%s' "$value"
}}

body="{{\"v\":1,\"agent\":\"$agent\",\"event\":\"$event\""
sid=$(json_str session_id || json_str sessionId || json_str sessionID)
[ -n "$sid" ] && body="$body,\"session_id\":\"$sid\""
cwd=$(json_str cwd || json_str working_dir || json_str directory)
[ -n "$cwd" ] && body="$body,\"cwd\":\"$cwd\""
msg=$(json_str message)
[ -n "$msg" ] && body="$body,\"message\":\"$msg\""
prompt=$(json_str prompt || json_str userPrompt || json_str user_prompt)
case "$prompt" in ?*) prompt=$(printf '%s' "$prompt" | cut -c 1-200) ;; esac
[ -n "$prompt" ] && body="$body,\"prompt\":\"$prompt\""
body="$body}}"

tty_write() {{
  payload="$1"
  # Overridable so tests (and debugging) can capture the bytes without a tty.
  target="${{DEEPTERM_REPORT_TARGET:-/dev/tty}}"
  if printf '%s' "$payload" > "$target" 2>/dev/null; then return 0; fi
  # /dev/tty can fail when the hook runs detached from its session; walk up
  # the process tree (bounded) to find an ancestor's terminal device.
  pid=$(ps -o ppid= -p $$ 2>/dev/null | tr -d ' ')
  hop=0
  while [ "$hop" -lt 8 ]; do
    case "$pid" in ""|*[!0-9]*|0|1) return 1 ;; esac
    set -- $(ps -o tty= -o ppid= -p "$pid" 2>/dev/null)
    tty_name="$1"
    ppid="$2"
    case "$tty_name" in ""|"?"|"??") pid="$ppid"; hop=$((hop + 1)); continue ;; esac
    if printf '%s' "$payload" > "/dev/$tty_name" 2>/dev/null; then return 0; fi
    pid="$ppid"
    hop=$((hop + 1))
  done
  return 1
}}

osc=$(printf '\033]777;notify;{sentinel};%s\007' "$body")
tty_write "$osc"
# Best effort by design: an unwritable terminal is not the hook's failure,
# and a non-zero exit would surface as an error inside the agent's output.
exit 0
"#,
        sentinel = AGENT_EVENT_SENTINEL,
    )
}

/// 构造某个事件对应的 `terminalSequence` 值（含 OSC 777 前缀与 BEL 终止符）。
///
/// 形状与 [`crate::agent::parse_agent_event`] 的三段硬剥前缀**一一对应**：
/// `parse_agent_event` 先剥 `777;notify;`、再剥 [`AGENT_EVENT_SENTINEL`]、
/// 再剥 `;`。任何一处不一致都会让它返回 `None`，而失败是静默的。
///
/// 用 `serde_json` 生成 JSON 而不是手拼字符串：会话 id、cwd 之类的值将来若
/// 进入载荷，一个引号就能把手拼的 JSON 拆坏。
pub fn claude_terminal_sequence(kind: AgentEventKind) -> String {
    let wire = serde_json::json!({
        "v": 1,
        "agent": CLIAgent::Claude.slug(),
        "event": kind.wire_name(),
    });
    format!(
        "\u{1b}]777;notify;{sentinel};{wire}\u{7}",
        sentinel = AGENT_EVENT_SENTINEL
    )
}

/// opencode 插件文件的位置（相对用户主目录）。
///
/// 本地路径尊重 `$XDG_CONFIG_HOME`，未设置时回落到 `~/.config`。
/// 此处是默认相对路径；覆盖由 [`local_bridge_path`] 统一处理。
pub const OPENCODE_PLUGIN_REL_PATH: &str = ".config/opencode/plugins/deepterm.js";

/// 上报脚本的位置（相对 `$HOME`）。本地与远端**同一位置、同一内容**：
/// 它是纯 POSIX shell，不依赖 deepterm 二进制，所以远端不需要装任何东西。
pub const REPORTER_REL_PATH: &str = ".deepterm/hooks/agent-report.sh";

/// 判定「桥接已装」的标记：任何引用了上报脚本的 hook 命令都算我们的。
///
/// 对共享 JSON/TOML 配置，它同时也是**清理标记**——重装时先删掉所有引用
/// 该脚本的条目再写入新版，保证幂等。旧版桥接用的 `deepterm agent-hook`
/// 命令由合并逻辑按 `legacy_marker` 一并清掉（见 [`merge_bridge_config`]）。
pub const REPORTER_MARKER: &str = ".deepterm/hooks/agent-report.sh";

/// 旧版桥接的命令前缀（`deepterm agent-hook <slug> <event>`）。
///
/// 它曾被所有非 Claude agent 的 hook 命令引用，依赖 `deepterm` 在 PATH 里——
/// 远端非交互 shell 找不到它，这就是「远程安装失败」的根源。已由上报脚本
/// 取代；保留常量只为合并时清理旧条目。
pub const LEGACY_CLI_PREFIX: &str = "deepterm agent-hook";

/// hook 命令里引用上报脚本的写法。`~` 由执行 hook 的 shell 展开。
pub fn reporter_command(agent: &str, event: &str) -> String {
    format!("~/{REPORTER_REL_PATH} {agent} {event}")
}

/// 远端启用 Codex `hooks` 特性的完整命令。
///
/// `${PATH:-…}` 而不是裸 `$PATH`：`PATH` 为空时末尾会留下一个空段，POSIX
/// 把空段解释为**当前目录**——那等于允许 `codex` 从远端 cwd 里被找到。
///
/// 目录表来自 [`crate::utils::path::USER_BIN_DIRS`]（「为什么要显式补进
/// `PATH`」的论证也在那里，本地那侧走
/// [`crate::utils::path::local_spawn_path`]）。
pub fn codex_enable_hooks_command() -> String {
    let dirs = crate::utils::path::USER_BIN_DIRS
        .iter()
        .map(|dir| format!("\"$HOME/{dir}\""))
        .collect::<Vec<_>>()
        .join(":");
    format!("PATH={dirs}:\"${{PATH:-/usr/bin:/bin}}\"; export PATH; codex features enable hooks")
}

/// 判定 opencode 插件「已安装」的标记。
pub const OPENCODE_PLUGIN_MARKER: &str = ".deepterm/hooks/agent-report.sh opencode";

/// pi 扩展文件的位置（相对用户主目录）。
///
/// pi 的扩展是**目录 + `index.ts`**，不是单个文件。目录名（`deepterm`）用作
/// 安装标记，所以卸载时删目录即可。
pub const PI_EXTENSION_REL_PATH: &str = ".pi/agent/extensions/deepterm/index.ts";

/// 判定 pi 扩展「已安装」的标记。
pub const PI_EXTENSION_MARKER: &str = ".deepterm/hooks/agent-report.sh pi";

/// 生成 opencode 插件的内容。
///
/// # 为什么这些 agent 需要**插件**而不是 hook 脚本
///
/// Claude Code 的 hook 没有控制终端，所以只能靠 `terminalSequence` 让宿主
/// 代写字节。opencode / pi 没有那个机制，但它们的**插件跑在 agent 自己的
/// 进程里**——spawn 出来的子进程继承同一个终端会话，可以自己写 `/dev/tty`。
/// 于是上报交给自带的**上报脚本**（[`agent_report_script`]）完成：它是纯
/// shell，本地/远端同一份，不需要 `deepterm` 在 PATH 里。
///
/// # 形状是 OpenCode **V2** 的插件契约
///
/// V2 要求 default 导出一个带 `id` + `setup(ctx)` 的定义；V1 那种
/// 「具名导出一个返回 hooks 对象的函数」在 V2 下**连加载都不会发生**——
/// 报 `Plugin must export a default definition with an id and an effect or
/// setup function`，然后整个桥接静默失效。V1/V2 的逐项对应关系：
///
/// | V1                         | V2                                    |
/// | -------------------------- | ------------------------------------- |
/// | 返回的 hooks 对象          | `setup` 里用 `ctx.*.hook(...)` 注册   |
/// | `event` 回调               | `ctx.event.subscribe()` 异步循环      |
/// | 返回的 `dispose`           | `setup` 返回的清理函数                |
/// | `tool.execute.before`      | `ctx.tool.hook("execute.before", …)`  |
/// | `permission.ask`           | 事件流里的 `permission.asked`         |
/// | 事件的 `properties`        | 事件的 `data`（V2 改名了）            |
/// | `$`（Bun shell helper）    | 自己 import `node:child_process` 管   |
///
/// `session.updated` 在 V2 里不存在了；指明父会话的只剩 `session.created`
/// 的 `parentID`，所以子会话只能在那里被记住。
///
/// # 两处不显然的处理（与 tty7 同构）
///
/// 1. **`DEEPTERM` 门控**。插件文件是长期装着的，不在 deepterm 里时必须
///    什么都不做，否则用户换别的终端会看到一堆错误。
/// 2. **subagent 抑制**。子 agent 跑在**同一个事件流**的子会话上，它的事件
///    与主会话的事件无法区分——只有 `session.created` 会指明父会话。所以
///    记住子会话 id，避免子 agent 让整块面板提前变「完成」。
pub fn opencode_plugin_js() -> String {
    // 命令前缀。`~` 由 `sh -c` 展开——写死绝对路径会在换机器/换用户后失效，
    // 走 PATH 查找 `deepterm` 又会重蹈「远程非交互 shell 找不到二进制」的覆辙；
    // 上报脚本随安装落在固定相对位置，是唯一两边都成立的引用方式。
    let prefix = &reporter_command("opencode", "");
    let prefix = prefix.trim_end();
    format!(
        r#"// deepterm agent-report opencode bridge — generated by deepterm, do not edit.
// Bridges OpenCode plugin events onto the deepterm reporter script
// (`{prefix} <event>`), which is inert outside deepterm (gated on the
// DEEPTERM env var).
//
// OpenCode 2 shape: V2 only loads a default-exported definition carrying an
// `id` and a `setup(ctx)`, so there is no V1 function export here.
//
// No plugin-package import: that bare specifier does not resolve from the
// global `~/.config/opencode/plugins/` directory, and `Plugin.define` is a
// plain identity function anyway.
import {{ spawn }} from "node:child_process"

export default {{
  id: "deepterm",
  async setup(ctx) {{
    if (!process.env["DEEPTERM"]) return
    const cmd = "{prefix} "
    let sessionId = ""
    let announced = ""
    // A subagent runs in a child session on the *same* event stream, and its
    // status events are indistinguishable from the pane's own — only
    // `session.created` names a parent. Remember the children, so a task tool
    // cannot call the pane done when only the subagent is.
    const children = new Set()
    // V1 handed us a Bun `$` shell helper; V2 has none, so spawn `sh` directly.
    // stdin carries the session id (the reporter reads it with `head -c`), and
    // it must be closed even when there is no id yet or the script would block.
    const emit = (event) =>
      new Promise((resolve) => {{
        // Every event carries the session id, so a pane that restarts can resume
        // the same session with `opencode --session`.
        const child = spawn("sh", ["-c", cmd + event], {{
          stdio: [sessionId ? "pipe" : "ignore", "ignore", "ignore"],
        }})
        child.on("error", () => resolve())
        child.on("close", () => resolve())
        if (sessionId && child.stdin) child.stdin.end(JSON.stringify({{ session_id: sessionId }}))
      }})
    // The session id is only known once opencode creates the session (the
    // `session.created` event); the session-start report rides on the first
    // event that names one.
    const capture = async (id) => {{
      if (id && !children.has(id)) sessionId = id
      if (sessionId && sessionId !== announced) {{
        announced = sessionId
        await emit("session-start")
      }}
    }}
    const ACTION = {{
      "session.status.busy": "prompt-submit",
      "session.status.idle": "stop",
      "session.idle": "stop",
      "permission.asked": "permission-request",
      "permission.replied": "prompt-submit",
    }}

    // V2 events carry their payload in `data` — V1 called it `properties` — and
    // `session.created` is the only event left that names a parent session.
    const controller = new AbortController()
    void (async () => {{
      for await (const event of ctx.event.subscribe({{ signal: controller.signal }})) {{
        const data = event.data ?? {{}}
        if (event.type === "session.created" && data.parentID) children.add(data.sessionID)
        if (data.sessionID && children.has(data.sessionID)) continue
        await capture(data.sessionID)
        const key = event.type === "session.status" ? `session.status.${{data.status?.type}}` : event.type
        const action = ACTION[key]
        if (action) await emit(action)
      }}
    }})()

    // V1's `tool.execute.before` hook: a running tool means the pane is busy.
    await ctx.tool.hook("execute.before", async (event) => {{
      await capture(event.sessionID)
      await emit("prompt-submit")
    }})

    // Unload: stop the subscription first so teardown cannot be outrun by a
    // late event, then report the pane as done.
    return async () => {{
      controller.abort()
      await emit("session-end")
    }}
  }},
}}
"#
    )
}

/// 生成 pi 扩展的内容。
///
/// 与 [`opencode_plugin_js`] 同理，上报交给自带的**上报脚本**
/// （[`agent_report_script`]），经 `/bin/sh -c` 调用。
///
/// 三处从 tty7 抄来的细节：
///
/// 1. **`DEEPTERM` 门控**（同 opencode）。
/// 2. **每个 `pi.on` 单独 try/catch**。pi 的各个事件是分版本加的，一个
///    不认识的事件名会抛异常——若不各自包裹，它会连带把**已经工作**的那几个
///    一起弄挂。
/// 3. **`ui_prompt_end` 恢复 `turn` 而不是一律 `prompt-submit`**。pi 在空闲时
///    也会弹窗（`/model`、命令的选项列表），一律报 `prompt-submit` 会让一个
///    已经结束的面板永远停在「工作中」，因为**没有回合能把它结束掉**。
pub fn pi_extension_ts() -> String {
    pi_extension_for("pi", "@mariozechner/pi-coding-agent")
}

fn pi_extension_for(slug: &str, package: &str) -> String {
    // pi 的 `ExtensionAPI` 类型来自它自己的包。用 `import type` 是刻意的：
    // 类型位置导入不会在运行期产生 `require`，所以 pi 没装某个包时不会崩。
    let reporter = reporter_command(slug, "");
    let reporter = reporter.trim_end();
    format!(
        r#"/* deepterm agent-report {slug} bridge — generated by deepterm, do not edit. */
import type {{ ExtensionAPI }} from "{package}";
import {{ spawnSync }} from "node:child_process";

// The reporter script is installed (and kept executable) by deepterm's hook
// installer; `sh -c` expands the leading `~`, so no absolute path is baked in.
const REPORT_CMD = "{reporter} ";

/** The slice of Pi's handler context we read — structural, so this bridge does
 *  not depend on the context type staying exported. */
type SessionCtx = {{ sessionManager?: {{ getSessionId?(): string | undefined }} }};

function emit(event: string, ctx?: SessionCtx): void {{
  try {{
    let payload = "";
    try {{
      const id = ctx?.sessionManager?.getSessionId?.();
      if (id) payload = JSON.stringify({{ session_id: id }});
    }} catch {{}}
    // Nothing to send → leave stdin closed rather than handing the emitter a
    // pipe it has to read to EOF.
    if (payload) {{
      spawnSync("/bin/sh", ["-c", REPORT_CMD + event], {{ input: payload, stdio: ["pipe", "ignore", "ignore"] }});
    }} else {{
      spawnSync("/bin/sh", ["-c", REPORT_CMD + event], {{ stdio: ["ignore", "ignore", "ignore"] }});
    }}
  }} catch {{}}
}}

export default function (pi: ExtensionAPI) {{
  if (!process.env["DEEPTERM"]) return;
  // Extension load = the agent is running in this pane. No context here yet,
  // so the id rides on session_start instead.
  emit("session-start");
  // What the pane showed before a UI prompt put it on "waiting", so closing
  // the prompt can put it back.
  let turn = "session-start";
  pi.on("agent_start", (_event, ctx) => emit((turn = "prompt-submit"), ctx));
  pi.on("agent_end", (_event, ctx) => emit((turn = "stop"), ctx));
  pi.on("session_shutdown", (_event, ctx) => emit("session-end", ctx));
  // Last, and guarded: the three above already worked, so a Pi build that
  // rejects this event name must not take them — or the whole extension —
  // down with it.
  try {{
    pi.on("session_start", (_event, ctx) => emit("session-start", ctx));
  }} catch {{}}
  // Each UI-prompt hook is guarded on its own for the same reason. Without
  // these the pane never reports "waiting for you" while a dialog is open.
  try {{
    pi.on("ui_prompt_start", (event, ctx) => {{
      emit(event.kind === "confirm" ? "permission-request" : "question-asked", ctx);
    }});
  }} catch {{}}
  // The dialog closed: restore what it interrupted. Not a blanket
  // prompt-submit — Pi also prompts while idle, and that would leave a
  // finished pane reading "working" with no turn to ever end it.
  try {{
    pi.on("ui_prompt_end", (_event, ctx) => emit(turn, ctx));
  }} catch {{}}
}}
"#
    )
}

///
/// 产出的是**纯文本**序列，其中 ESC/BEL 写成 shell 的 `\033` / `\007`
/// 转义而不是裸控制字节（裸字节会让脚本正文难以编辑与传输）。
/// **不做 JSON 转义**——`emit` 负责把结果包成合法 JSON，理由见下。
///
/// # 坑 1：`$(printf ...)` 写在单引号里不会被展开
///
/// 直觉写法：
///
/// ```text
/// emit '$(printf "\033")]777;...'
/// ```
///
/// `$(...)` 在**单引号内不展开**，终端会收到字面量 `$(printf ...)`。更糟的是
/// 为了让内层 `printf` 有引号，`'\033'` 里那个单引号会**提前闭合外层引号**，
/// 于是连反斜杠都保不住。`sh -n` 语法检查**照样通过**，只有真的跑一遍才暴露。
///
/// # 坑 2：不要在这里做 JSON 转义
///
/// 中间试过在这里把引号转成 `\"`、控制字符转成 `\u001b`，再由 `emit` 用
/// `printf %b` 展开。**这是错的**：`%b` 的展开发生在格式串解析**之后**，
/// 展开结果被**原样**塞进 JSON 字符串，于是 `\"` 变成「反斜杠 + 引号」两个
/// 字符，内层 JSON 出现 `{\"v\":1}` 这种脏数据，OSC 载荷被破坏
/// （`generated_script_run_by_real_sh_produces_parsable_bytes` 抓到的就是它）。
///
/// 而且内层引号本来**就不该**转义：它们属于 OSC 载荷，不是外层 JSON 的。
/// 正确分工是——这里只管把控制字符写成 shell 转义，`emit` 负责让最终的
/// 那行 JSON 合法（用 `printf` 的格式串写 `\u001b`，让 Claude Code 收到
/// JSON 规范要求的转义形式）。
fn esc_for_shell_single_quoted(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            // 交给 `emit` 的 `printf %b` 展开成真实字节。
            '\u{1b}' => out.push_str("\\033"),
            '\u{7}' => out.push_str("\\007"),
            // 单引号：按 POSIX 规则断开重接，避免截断 shell 字面量。
            '\'' => out.push_str("'\\''"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::parse_agent_event;
    // 只有跑真实 `/bin/sh` 的用例需要它，那些用例是 `#[cfg(unix)]`。
    #[cfg(unix)]
    use std::process::Command;

    /// 取出 OSC 的**内容**部分（`ESC ]` 与 BEL 之间）。
    #[cfg(unix)]
    fn osc_content(sequence: &[u8]) -> &[u8] {
        let start = sequence
            .strip_prefix(b"\x1b]")
            .expect("序列必须以 ESC ] 起头");
        let end = start
            .iter()
            .rposition(|b| *b == 0x07)
            .expect("序列必须以 BEL 收尾");
        &start[..end]
    }

    /// 远端启用 Codex hooks 的命令必须**自带 PATH 兜底**。
    ///
    /// 这是「远程安装失败」那个 bug 的护栏：deepterm 走 SFTP 会话上的
    /// `channel.exec`，那是非交互、非登录 shell，`~/.profile` / `~/.bashrc`
    /// 都不执行。Codex 的 standalone 安装器把二进制放在 `~/.local/bin`，
    /// 于是裸 `codex features enable hooks` 退出 127，安装中断在写
    /// `hooks.json` 之前——现场只剩一个孤儿上报脚本。
    ///
    /// 断言的是**具体目录出现在命令里**，而不是「命令非空」：后者在命令
    /// 退化成裸 `codex ...` 时照样通过，正是这次要防的回归。
    #[test]
    fn codex_enable_hooks_command_carries_its_own_path() {
        let command = codex_enable_hooks_command();
        for dir in crate::utils::path::USER_BIN_DIRS {
            assert!(
                command.contains(&format!("\"$HOME/{dir}\"")),
                "命令必须补入 ~/{dir}，否则非交互通道找不到 codex：{command}"
            );
        }
        assert!(command.contains("codex features enable hooks"));
        // 空 PATH 兜底：裸 `$PATH` 为空时末尾空段会被 POSIX 解释成当前目录。
        assert!(
            command.contains("${PATH:-/usr/bin:/bin}"),
            "PATH 为空时必须有安全兜底，不能让空段退化成当前目录：{command}"
        );
    }

    /// 远端命令必须是合法 POSIX shell，且真的能在**没有 PATH 继承**的环境里
    /// 找到可执行文件。
    ///
    /// 用真实的 `/bin/sh` 跑一遍：造一个假的 `codex`（记录 argv 后退出 0），
    /// 只放进 `~/.local/bin`，然后把 `PATH` 清空。若命令没自带兜底，这一步会
    /// 以 127 失败——与远端现场一致。
    ///
    /// 被验的是**远端**（POSIX）命令串，故只在 unix 上跑。
    #[test]
    #[cfg(unix)]
    fn codex_enable_hooks_command_finds_codex_without_inherited_path() {
        let home =
            std::env::temp_dir().join(format!("deepterm-codex-path-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = home.join("argv.txt");
        let fake = bin.join("codex");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > {}\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(codex_enable_hooks_command())
            .env("HOME", &home)
            .env("PATH", "")
            .output()
            .expect("命令必须能被 /bin/sh 执行");

        assert!(
            output.status.success(),
            "命令必须自带 PATH 兜底才能在非交互通道跑通：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "features enable hooks",
            "必须真的调到 codex 且参数正确"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn shared_hooks_preserve_foreign_handlers_and_reject_invalid_shapes() {
        let plan = hook_install_plan(CLIAgent::Codex).unwrap();
        let existing = br#"{"permissions":{"allow":["Read"]},"hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"third-party-notify"},{"type":"command","command":"deepterm agent-hook codex stop"}]}]}}"#;
        let merged = merge_bridge_config(CLIAgent::Codex, &plan, existing).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["permissions"]["allow"][0], "Read");
        assert_eq!(parsed["hooks"]["Stop"][0]["matcher"], "*");
        assert_eq!(
            parsed["hooks"]["Stop"][0]["hooks"],
            serde_json::json!([{"type":"command","command":"third-party-notify"}])
        );
        assert_eq!(
            merge_bridge_config(CLIAgent::Codex, &plan, merged.as_bytes()).unwrap(),
            merged
        );
        assert!(bridge_installed(CLIAgent::Codex, &plan, merged.as_bytes()));
        let mut partial = parsed;
        partial["hooks"].as_object_mut().unwrap().remove("Stop");
        assert!(!bridge_installed(
            CLIAgent::Codex,
            &plan,
            &serde_json::to_vec(&partial).unwrap()
        ));
        for invalid in [
            b"{broken".as_slice(),
            br#"{"hooks":[]}"#,
            br#"{"hooks":{"Stop":{}}}"#,
        ] {
            assert!(merge_bridge_config(CLIAgent::Codex, &plan, invalid).is_err());
        }
    }

    #[test]
    fn kimi_merge_preserves_comments_and_foreign_hooks() {
        let plan = hook_install_plan(CLIAgent::Kimi).unwrap();
        let existing = "# provider comment\nmodel = 'fixture'\n[[hooks]]\nevent = 'Stop'\ncommand = 'third-party-notify'\n";
        let merged = merge_bridge_config(CLIAgent::Kimi, &plan, existing.as_bytes()).unwrap();
        assert!(merged.contains("# provider comment"));
        let doc = merged.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("fixture"));
        assert!(
            doc["hooks"]
                .as_array_of_tables()
                .unwrap()
                .iter()
                .any(|h| h["command"].as_str() == Some("third-party-notify"))
        );
        assert_eq!(
            merge_bridge_config(CLIAgent::Kimi, &plan, merged.as_bytes()).unwrap(),
            merged
        );
        assert!(merge_bridge_config(CLIAgent::Kimi, &plan, b"hooks = []").is_ok());
        assert!(merge_bridge_config(CLIAgent::Kimi, &plan, b"hooks = 'invalid'").is_err());
    }

    /// 本机 `~/.claude/settings.json` 的**真实形状**（含 Otty 装的 entry）。
    /// 用来验证我们不会踩到别人的配置。
    const REAL_WORLD_SETTINGS: &[u8] = br#"{
      "hooks": {
        "PreToolUse": [
          {
            "_otty": true,
            "hooks": [
              {
                "command": "/Applications/Otty.app/Contents/Resources/agent-integration/claude/otty-hook.sh processing \"$PPID\"",
                "type": "command"
              }
            ]
          }
        ]
      },
      "permissions": { "defaultMode": "fullAccess" },
      "model": "some-model"
    }"#;

    /// 跨进程契约：hook 脚本里那个 `terminalSequence` 必须能被终端侧解析出来。
    ///
    /// 这条一次锁住四处容易写错的地方：OSC 777 前缀、哨兵、JSON 字段名、
    /// 事件名的 kebab-case。任何一处错都会让链路静默失效（状态点永不出现）。
    #[test]
    fn hook_script_payloads_round_trip() {
        for spec in CLAUDE_HOOK_EVENTS {
            let seq = claude_terminal_sequence(spec.kind);
            // 模拟终端收到 OSC：内容是 ESC ] 与终止符之间的部分。
            let inner = seq
                .strip_prefix("\u{1b}]")
                .expect("必须以 OSC 引导符开头")
                .strip_suffix('\u{7}')
                .expect("必须以 BEL 结尾");

            let event = parse_agent_event(inner.as_bytes())
                .unwrap_or_else(|| panic!("{:?} 的载荷解析失败：{inner}", spec.claude_event));

            assert_eq!(event.kind, spec.kind, "{}", spec.claude_event);
            assert_eq!(event.agent, Some(CLIAgent::Claude));
        }
    }

    /// 脚本正文里嵌入的载荷也必须能解析——测试不能只测 Rust 侧生成的那份，
    /// 否则脚本里的字面量写错了照样过。
    ///
    /// 这里刻意取 `Stop)` 分支之后的那个载荷（脚本里 `Stop` 分支的 emit 带一层
    /// 缩进、且前面还有一个 `case` 缩进块），验证的是**脚本生成逻辑**而不是
    /// `claude_terminal_sequence` 本身。
    #[test]
    fn hook_script_embeds_parsable_payloads() {
        let script = hook_script();
        // 取 `Stop)` 分支之后第一个 emit 的载荷。
        let payload = script
            .split("\n  Stop)")
            .nth(1)
            .and_then(|rest| rest.split("emit '").nth(1))
            .and_then(|rest| rest.split('\'').next())
            .expect("脚本的 Stop 分支里应当有 emit '...' 调用");

        // 脚本里的载荷是纯文本，控制字符写成 shell 转义 `\033` / `\007`。
        let expanded = payload.replace("\\033", "\u{1b}").replace("\\007", "\u{7}");
        let inner = expanded
            .strip_prefix("\u{1b}]")
            .unwrap_or_else(|| panic!("必须由 OSC 引导：{expanded:?}"))
            .strip_suffix('\u{7}')
            .unwrap_or_else(|| panic!("必须以 BEL 结尾：{expanded:?}"));

        let event = parse_agent_event(inner.as_bytes()).expect("脚本里的载荷应当可解析");
        assert_eq!(event.agent, Some(CLIAgent::Claude));
        assert_eq!(event.kind, AgentEventKind::Stop);
    }

    /// 脚本必须按 `$1` 覆盖到每一个我们安装的事件名。
    ///
    /// `plan_install` 写进 settings.json 的字面量事件名与脚本里的 `case` 分支
    /// 是**两份独立数据**，对不上就会静默落到「未知事件」分支——状态点不亮，
    /// 没有任何报错。这条把它们绑死。
    #[test]
    fn hook_script_has_a_branch_for_every_installed_event() {
        let script = hook_script();
        for spec in CLAUDE_HOOK_EVENTS {
            assert!(
                script.contains(&format!("\n  {})", spec.claude_event)),
                "脚本缺少 {} 的分支——该事件会静默失效",
                spec.claude_event
            );
        }
    }

    /// opencode 插件必须在 `DEEPTERM` 缺席时**直接返回**。
    ///
    /// 这是「装好的插件在别的终端里无害」的唯一保证。少了它，用户换回
    /// 别的终端跑 opencode 会每次事件都去 spawn 一个不存在的命令。
    #[test]
    fn opencode_plugin_is_gated_on_the_env_marker() {
        let js = opencode_plugin_js();
        assert!(
            js.contains("process.env[\"DEEPTERM\"]"),
            "插件必须以 DEEPTERM 环境变量门控"
        );
        // 门控必须是「取反后提前返回」，而不是「存在时才做某件事」的弱形式。
        assert!(
            js.contains("if (!process.env[\"DEEPTERM\"]) return"),
            "门控必须是提前返回"
        );
    }

    /// pi 扩展同样必须门控，且要能跑在 pi 的扩展契约里。
    #[test]
    fn pi_extension_is_gated_and_exports_a_factory() {
        let ts = pi_extension_ts();
        assert!(
            ts.contains("if (!process.env[\"DEEPTERM\"]) return;"),
            "扩展必须以 DEEPTERM 环境变量门控"
        );
        assert!(
            ts.contains("export default function (pi: ExtensionAPI)"),
            "pi 扩展必须是默认导出的工厂函数"
        );
    }

    /// 两个桥接文件都必须调用**本仓库真实的**上报脚本形状。
    ///
    /// 写成别的形状（如指向别的脚本名）不会有任何编译错误，只会在用户机器上
    /// 静默不工作——所以这里把脚本调用契约钉死在测试里。
    #[test]
    fn bridges_invoke_the_reporter_script() {
        for src in [
            opencode_plugin_js(),
            pi_extension_ts(),
            pi_extension_for("omp", "@oh-my-pi/pi-coding-agent"),
        ] {
            assert!(
                src.contains(REPORTER_REL_PATH),
                "桥接必须引用上报脚本 {REPORTER_REL_PATH}"
            );
            assert!(
                !src.contains(LEGACY_CLI_PREFIX),
                "桥接不得再依赖 `deepterm` 在 PATH 里（远程装不了）"
            );
        }
        let js = opencode_plugin_js();
        assert!(
            js.contains(&format!("{REPORTER_REL_PATH} opencode ")),
            "opencode 桥接必须以「上报脚本 + opencode」起头"
        );
    }

    /// 上报脚本必须覆盖全部可安装 agent 的 slug，且事件白名单与
    /// `parse_event` 一致——模板是字符串，漏一个就是静默不工作。
    #[test]
    fn reporter_script_covers_every_installable_slug_and_event() {
        let script = agent_report_script();
        // 取 `case ... in` 的下一行（白名单本体），按 `|` 拆开断言成员。
        let case_items = |header: &str| -> Vec<String> {
            let idx = script.find(header).expect("脚本必须有白名单 case");
            let rest = &script[idx..];
            let line = rest.lines().nth(1).expect("case 头之后必须有白名单行");
            line.trim_end()
                .trim_end_matches([')', ';', ' '])
                .split('|')
                .map(str::trim)
                .map(str::to_string)
                .collect()
        };
        let slugs = case_items("case \"$agent\" in");
        let events = case_items("case \"$event\" in");
        for agent in INSTALLABLE_AGENTS
            .iter()
            .filter(|a| **a != CLIAgent::Claude)
        {
            assert!(
                slugs.iter().any(|s| s == agent.slug()),
                "上报脚本必须认得 agent `{}`（当前白名单：{slugs:?}）",
                agent.slug()
            );
        }
        for kind in AgentEventKind::ALL {
            let event = kind.wire_name();
            assert!(
                events.iter().any(|e| e == event),
                "上报脚本必须认得事件 `{event}`（当前白名单：{events:?}）"
            );
        }
    }

    /// 上报脚本的 E2E：真实 `/bin/sh` 跑一遍，产出必须能被终端侧解析器读回。
    ///
    /// 脚本写 tty 的目标可用 `DEEPTERM_REPORT_TARGET` 覆盖（给测试留的口子），
    /// 这里指到临时文件再解析。
    #[test]
    #[cfg(unix)]
    fn reporter_script_run_by_real_sh_produces_parsable_bytes() {
        let dir =
            std::env::temp_dir().join(format!("deepterm-reporter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent-report.sh");
        let target = dir.join("osc.out");
        std::fs::write(&script, agent_report_script()).unwrap();

        for (agent, event, stdin_json, expected_kind) in [
            (
                "opencode",
                "prompt-submit",
                r#"{"sessionID":"ses_1","cwd":"/tmp/x"}"#,
                AgentEventKind::PromptSubmit,
            ),
            ("codex", "stop", "", AgentEventKind::Stop),
        ] {
            let _ = std::fs::remove_file(&target);
            let output = Command::new("/bin/sh")
                .arg(&script)
                .args([agent, event])
                .env("DEEPTERM", "true")
                .env("DEEPTERM_REPORT_TARGET", &target)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child
                        .stdin
                        .take()
                        .unwrap()
                        .write_all(stdin_json.as_bytes())?;
                    child.wait_with_output()
                })
                .expect("上报脚本必须能被 /bin/sh 执行");
            assert!(
                output.status.success(),
                "脚本必须以 0 退出：{:?}",
                output.stderr
            );
            let bytes = std::fs::read(&target).expect("OSC 必须写到目标文件");
            let parsed = crate::agent::parse_agent_event(osc_content(&bytes))
                .expect("脚本产出的序列必须能被解析器读回");
            assert_eq!(parsed.kind, expected_kind);
            if agent == "opencode" {
                assert_eq!(
                    parsed.session_id.as_deref(),
                    Some("ses_1"),
                    "session_id 别名必须覆盖 opencode 拼法"
                );
                assert_eq!(parsed.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// jcode 关着 stdin 跑钩子，事件描述放在 `JCODE_HOOK_PAYLOAD` 里。
    ///
    /// 读 stdin 会永久阻塞，所以脚本必须从这个环境变量取载荷。这条守的是
    /// 「装了 hook 但状态点永远不亮」——那种失败在用户机器上是静默的。
    #[test]
    #[cfg(unix)]
    fn jcode_reports_from_its_environment_payload() {
        let dir = std::env::temp_dir().join(format!("deepterm-jcode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent-report.sh");
        let target = dir.join("osc.out");
        std::fs::write(&script, agent_report_script()).unwrap();
        let _ = std::fs::remove_file(&target);

        let output = Command::new("/bin/sh")
            .arg(&script)
            .args(["jcode", "prompt-submit"])
            .env("DEEPTERM", "true")
            .env(
                "JCODE_HOOK_PAYLOAD",
                r#"{"session_id":"jc-1","cwd":"/repo"}"#,
            )
            .env("DEEPTERM_REPORT_TARGET", &target)
            // stdin 关掉：真实环境里 jcode 就是这么跑的。
            .stdin(std::process::Stdio::null())
            .output()
            .expect("上报脚本必须能被 /bin/sh 执行");
        assert!(
            output.status.success(),
            "脚本必须以 0 退出：{:?}",
            output.stderr
        );

        let bytes = std::fs::read(&target).expect("OSC 必须写到目标文件");
        let parsed = crate::agent::parse_agent_event(osc_content(&bytes))
            .expect("脚本产出的序列必须能被解析器读回");
        assert_eq!(parsed.kind, AgentEventKind::PromptSubmit);
        assert_eq!(
            parsed.agent,
            Some(CLIAgent::Jcode),
            "事件必须归属到 jcode，否则 tab 上认不出这个 agent"
        );
        assert_eq!(parsed.session_id.as_deref(), Some("jc-1"));
        assert_eq!(parsed.cwd.as_deref(), Some(std::path::Path::new("/repo")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `json_str` 对嵌不进载荷的值（引号/反斜杠/控制字节/缺失）必须返回非零，
    /// 对干净值原样输出——否则整条序列会被终端解析器丢弃，事件全部丢失。
    #[test]
    #[cfg(unix)]
    fn reporter_script_json_str_rejects_unsafe_values() {
        let script = agent_report_script();
        let start = script.find("json_str() {").expect("必须有 json_str");
        let body = &script[start..];
        let end = body.find("\n}\n").expect("函数体必须有边界");
        let func = format!("{}\n", &body[..end + 2]);
        let dir = std::env::temp_dir().join(format!("deepterm-reporter-fn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fn.sh");
        std::fs::write(&path, func).unwrap();

        let run = |json: &str| {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    ". '{}'; input='{}'; json_str session_id",
                    path.display(),
                    json
                ))
                .output()
                .unwrap()
        };
        // 干净值原样返回；opencode 拼法之外的别名取不到 → 失败。
        assert_eq!(run("{\"session_id\":\"ses_1\"}").stdout, b"ses_1");
        assert!(!run("{\"sessionId\":\"ses_2\"}").status.success());
        // 会破坏载荷的值一律拒绝。
        assert!(!run("{\"session_id\":\"bad\\\"quote\"}").status.success());
        assert!(!run("{\"session_id\":\"back\\\\slash\"}").status.success());
        assert!(!run("{\"session_id\":\"ctrl\\u0001x\"}").status.success());
        assert!(!run("{}").status.success());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上报脚本的**事件路由**必须与旧 CLI 通路语义一致。
    ///
    /// 这段逻辑（qodercli 的 compact 抑制、四家 agent 的
    /// Notification→PermissionRequest 嗅探）原先由 Rust 的 `effective_event`
    /// 承担、有逐条断言；改成 shell 之后**没有任何编译期检查**，写错就是静默
    /// 失效——最典型的症状是 qodercli 每次上下文压缩都被当成新会话、或
    /// copilot 的普通通知被误报成「等待授权」。
    ///
    /// 断言的是**最终产出的事件名**（端到端经真实 `/bin/sh`），不是脚本文本。
    #[test]
    #[cfg(unix)]
    fn reporter_script_routes_events_like_the_old_cli() {
        let dir =
            std::env::temp_dir().join(format!("deepterm-reporter-routing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent-report.sh");
        std::fs::write(&script, agent_report_script()).unwrap();
        let target = dir.join("osc.out");

        // 跑一次，返回产出的事件名；脚本没写任何东西（静默忽略）时返回 `None`。
        let run = |agent: &str, event: &str, json: &str| -> Option<AgentEventKind> {
            let _ = std::fs::remove_file(&target);
            let output = Command::new("/bin/sh")
                .arg(&script)
                .args([agent, event])
                .env("DEEPTERM", "true")
                .env("DEEPTERM_REPORT_TARGET", &target)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    // 未知事件/未知 agent 会被脚本**先退出、再读 stdin**（设计如此：
                    // 不能阻塞 agent）。因此写 stdin 与子进程退出天然存在竞争，
                    // `BrokenPipe` 是合法结果，不能当成测试失败。
                    if let Some(mut stdin) = child.stdin.take()
                        && let Err(e) = stdin.write_all(json.as_bytes())
                    {
                        assert_eq!(
                            e.kind(),
                            std::io::ErrorKind::BrokenPipe,
                            "写入上报脚本 stdin 失败：{e}"
                        );
                    }
                    child.wait_with_output()
                })
                .expect("上报脚本必须能被 /bin/sh 执行");
            assert!(
                output.status.success(),
                "脚本必须以 0 退出：{:?}",
                output.stderr
            );
            let bytes = std::fs::read(&target).ok()?;
            let parsed = crate::agent::parse_agent_event(osc_content(&bytes))
                .expect("脚本产出的序列必须能被解析器读回");
            assert_eq!(parsed.agent.map(|a| a.slug()), Some(agent));
            Some(parsed.kind)
        };

        use AgentEventKind::*;

        // qodercli：compact 触发的 SessionStart 不是新会话，必须静默。
        assert_eq!(
            run("qodercli", "session-start", r#"{"source":"compact"}"#),
            None
        );
        assert_eq!(
            run("qodercli", "session-start", r#"{"source":"startup"}"#),
            Some(SessionStart),
            "真正的启动仍要上报"
        );

        // 四家 agent 的 Notification：只有权限提示才转成 PermissionRequest，
        // 其余一律静默（否则每个普通通知都会把面板翻成「等待授权」）。
        for agent in ["copilot", "grok", "droid", "gemini"] {
            assert_eq!(
                run(agent, "notification", r#"{"message":"just an update"}"#),
                None,
                "{agent} 的普通通知必须静默"
            );
            assert_eq!(
                run(agent, "notification", "elicitation_dialog"),
                Some(PermissionRequest),
                "{agent} 的 elicitation_dialog 是权限提示"
            );
        }
        for agent in ["copilot", "droid"] {
            assert_eq!(
                run(agent, "notification", "permission_prompt"),
                Some(PermissionRequest)
            );
        }
        for agent in ["grok", "gemini"] {
            assert_eq!(
                run(agent, "notification", "permission_prompt"),
                None,
                "{agent} 的 permission_prompt 不是它的权限信号，必须静默"
            );
        }
        assert_eq!(
            run("gemini", "notification", "ToolPermission"),
            Some(PermissionRequest)
        );
        assert_eq!(
            run("copilot", "notification", "ToolPermission"),
            None,
            "ToolPermission 只对 gemini 有意义"
        );

        // `tool-start` 是 PreToolUse 的中间态：只有问用户问题的工具才转成
        // question-asked，其余整个丢掉——所以装 PreToolUse 不会让状态点抖。
        assert_eq!(
            run(
                "codex",
                "tool-start",
                r#"{"tool_name":"request_user_input"}"#
            ),
            Some(QuestionAsked),
            "Codex 的 request_user_input 是提问，必须点亮等待"
        );
        assert_eq!(
            run(
                "codex",
                "tool-start",
                "{\n  \"tool_name\": \"request_user_input\"\n}"
            ),
            Some(QuestionAsked),
            "pretty-printed 的载荷也要认出来"
        );
        assert_eq!(
            run("codex", "tool-start", r#"{"tool_name":"shell"}"#),
            None,
            "普通工具调用必须静默，否则状态点会抖"
        );

        // 白名单：未知事件 / 未知 agent / 无 DEEPTERM 一律静默。
        assert_eq!(run("opencode", "some-future-event", "{}"), None);
        assert_eq!(
            run("claude", "stop", "{}"),
            None,
            "claude 不在脚本白名单里（走 terminalSequence）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 桥接里出现的每个事件名都必须是上报脚本能认的。
    ///
    /// 桥接文件是**字符串模板**，拼错一个事件名（如 `prompt-submitted`）不会
    /// 有任何编译期检查，只会在运行时被脚本的 `case "$event"` 白名单静默丢弃。
    /// 脚本的白名单又由 [`AgentEventKind::ALL`] 断言（见
    /// `reporter_script_covers_every_installable_slug_and_event`），所以这条
    /// 与那条合起来把「模板 → 脚本 → 终端解析器」三处串成一条链。
    #[test]
    fn bridge_event_names_are_all_recognised_by_the_reporter() {
        let known: Vec<&str> = AgentEventKind::ALL.iter().map(|k| k.wire_name()).collect();
        let sources = [opencode_plugin_js(), pi_extension_ts()];
        for src in sources {
            for name in [
                "session-start",
                "prompt-submit",
                "permission-request",
                "question-asked",
                "tool-complete",
                "stop",
                "session-end",
            ] {
                if src.contains(&format!("\"{name}\"")) {
                    assert!(
                        known.contains(&name),
                        "桥接里用到了 {name}，但上报脚本的白名单不认它"
                    );
                }
            }
        }
    }

    /// opencode 的 subagent 抑制必须还在。
    ///
    /// 子会话与主会话共用一条事件流，不记住子会话 id 的话，一个后台 subagent
    /// 干完就会把整块面板报成「完成」。
    #[test]
    fn opencode_plugin_suppresses_subagent_sessions() {
        let js = opencode_plugin_js();
        assert!(js.contains("const children = new Set()"), "必须记住子会话");
        assert!(
            js.contains("children.has(data.sessionID)"),
            "子会话的事件必须被跳过"
        );
        // 抑制的入口是 `session.created` 的 parentID——V2 里唯一指明父会话的
        // 事件（V1 还有 session.updated，V2 已删）。
        assert!(
            js.contains(r#"event.type === "session.created" && data.parentID"#),
            "必须在 session.created 上记下子会话"
        );
    }

    /// opencode 插件必须是 **V2 契约**：default 导出一个 `id` + `setup` 的定义。
    ///
    /// V2 对形状不符的插件是**直接拒绝加载**（`SchemaError(Missing key at
    /// ["default"])`），而不是降级运行——于是桥接静默失效，面板状态不再更新，
    /// 日志里也只有一条 WARN。这里把入口形状钉死，防止模板被改回 V1 那种
    /// 具名函数导出。
    #[test]
    fn opencode_plugin_exports_a_v2_definition() {
        let js = opencode_plugin_js();
        assert!(
            js.contains("export default {"),
            "V2 只认 default 导出的定义"
        );
        assert!(
            js.contains(r#"id: "deepterm""#),
            "每个 V2 插件都需要一个稳定 id"
        );
        assert!(js.contains("async setup(ctx) {"), "必须有 setup 函数");
        assert!(
            !js.contains("export const DeeptermPresence"),
            "V1 的具名函数导出在 V2 下根本不会被加载"
        );
        // V2 事件的载荷字段是 `data`；照抄 V1 的 `properties` 读到的是
        // undefined，所有上报会静默消失（不报错）。
        assert!(js.contains("event.data"), "V2 事件的载荷字段是 data");
        assert!(
            !js.contains("event.properties"),
            "V1 的 properties 在 V2 事件上不存在"
        );
        // V1 送我们一个 Bun `$` shell helper，V2 的插件上下文里没有它——
        // 用了就是运行时 ReferenceError。
        assert!(
            js.contains(r#"import { spawn } from "node:child_process""#),
            "必须自己 import 进程 API"
        );
        assert!(!js.contains(r#"$`sh -c"#), "不得再依赖 V1 的 `$` helper");
        // `@opencode/plugin` 在全局 `~/.config/opencode/plugins/` 下**解析不到**
        // （实测 `ResolveMessage`），而 `Plugin.define` 只是恒等函数——所以
        // 入口必须是裸对象，import 了反而加载失败。
        assert!(
            !js.contains("@opencode/plugin"),
            "不得 import 解析不到的 @opencode/plugin"
        );
    }

    /// V1 的四个 hook 在 V2 里各有新的落点，少一个就是那类事件不再上报。
    #[test]
    fn opencode_plugin_registers_every_v2_registration_point() {
        let js = opencode_plugin_js();
        for needle in [
            // V1 `event` 回调 → 事件流订阅
            "ctx.event.subscribe({ signal: controller.signal })",
            // V1 `tool.execute.before` → 工具域 hook
            r#"await ctx.tool.hook("execute.before""#,
            // V1 `permission.ask` → 事件流里的 permission.asked
            r#""permission.asked": "permission-request""#,
            // V1 返回的 `dispose` → setup 返回的清理函数
            "controller.abort()",
            r#"await emit("session-end")"#,
        ] {
            assert!(js.contains(needle), "缺少 {needle}——那条通路会静默失效");
        }
    }

    /// pi 的 `ui_prompt_end` 必须恢复被打断前的 `turn`，而不是一律
    /// `prompt-submit`。
    ///
    /// pi 在空闲时也会弹窗（`/model`、命令的选项列表）。一律报
    /// `prompt-submit` 会让一个已经结束的面板永远停在「工作中」——**没有回合
    /// 能把它结束掉**。
    #[test]
    fn pi_extension_restores_the_interrupted_turn() {
        let ts = pi_extension_ts();
        assert!(
            ts.contains(r#"emit((turn = "prompt-submit"), ctx)"#),
            "agent_start 必须记住当前回合"
        );
        assert!(
            ts.contains(r#"pi.on("ui_prompt_end", (_event, ctx) => emit(turn, ctx))"#),
            "ui_prompt_end 必须恢复 turn，不能一律报 prompt-submit"
        );
    }

    /// 安装命令必须把事件名写成**字面量**。
    ///
    /// Claude Code 执行的是命令字符串本身、不会额外传参，所以 `"$1"` 在那里是
    /// 空的。这条用真实事件名去匹配，正面确认字面量在。
    #[test]
    fn install_command_passes_the_event_name_literally() {
        let planned = plan_install(b"{}").unwrap();
        let root: serde_json::Value = serde_json::from_slice(&planned).unwrap();
        for spec in CLAUDE_HOOK_EVENTS {
            let groups = root["hooks"][spec.claude_event].as_array().unwrap();
            let command = groups[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.contains(spec.claude_event),
                "{} 的命令里没有把事件名写成字面量：{command}",
                spec.claude_event
            );
            assert!(
                command.contains(AGENT_EVENT_SENTINEL),
                "{} 的命令里缺哨兵，识别与去重都会失效：{command}",
                spec.claude_event
            );
            assert!(
                !command.contains("\"$1\""),
                "{} 的命令用了 \"$1\"，但 Claude Code 不会传参：{command}",
                spec.claude_event
            );
        }
    }

    #[test]
    fn has_deepterm_hook_ignores_unrelated_content() {
        // 别人的 entry（Otty）不算我们装过。
        assert!(!has_deepterm_hook(REAL_WORLD_SETTINGS));
        // 非法 JSON 不能 panic，且判为「未安装」。
        assert!(!has_deepterm_hook(b"{ not json"));
        // 没有 hooks 键。
        assert!(!has_deepterm_hook(b"{}"));
        // 装过之后应当认出来。
        let installed = plan_install(REAL_WORLD_SETTINGS).unwrap();
        assert!(has_deepterm_hook(&installed));
    }

    #[test]
    fn plan_install_preserves_user_settings() {
        let planned = plan_install(REAL_WORLD_SETTINGS).unwrap();
        let root: serde_json::Value = serde_json::from_slice(&planned).unwrap();

        // 用户原有的顶层键逐字保留。
        assert_eq!(root["model"], "some-model");
        assert_eq!(root["permissions"]["defaultMode"], "fullAccess");

        // Otty 的 entry 必须还在——整段覆盖别人的 hook 是最严重的失败模式。
        let pre_tool = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre_tool.iter().any(|g| g.get("_otty").is_some()),
            "不能删掉 Otty 的 hook：{pre_tool:?}"
        );

        // 我们的事件都装上了。
        for spec in CLAUDE_HOOK_EVENTS {
            let groups = root["hooks"][spec.claude_event]
                .as_array()
                .unwrap_or_else(|| panic!("{} 没装上", spec.claude_event));
            assert!(
                groups.iter().any(|g| {
                    g["hooks"][0]["command"]
                        .as_str()
                        .is_some_and(|c| c.contains(AGENT_EVENT_SENTINEL))
                }),
                "{} 的 entry 形状不对：{groups:?}",
                spec.claude_event
            );
        }
    }

    /// 幂等：第二次安装必须与第一次**完全相等**，而不是「又追加一份」。
    ///
    /// 重复 entry 会让同一个事件跑两次 hook，状态机收到重复事件。
    #[test]
    fn plan_install_is_idempotent() {
        let once = plan_install(REAL_WORLD_SETTINGS).unwrap();
        let twice = plan_install(&once).unwrap();
        assert_eq!(
            String::from_utf8(once).unwrap(),
            String::from_utf8(twice).unwrap()
        );
    }

    #[test]
    fn plan_install_refuses_unparsable_input() {
        assert_eq!(plan_install(b"{ not json"), Err(HookPlanError::Unparsable));
        // 顶层是数组而非对象。
        assert_eq!(plan_install(b"[]"), Err(HookPlanError::NotAnObject));
        // hooks 存在但不是对象。
        assert_eq!(
            plan_install(br#"{"hooks": []}"#),
            Err(HookPlanError::NotAnObject)
        );
    }

    #[test]
    fn should_prompt_hook_install_table() {
        let cases = [
            // 三种可安装的 agent 都要触发。
            (Some(CLIAgent::Claude), false, false, true),
            (Some(CLIAgent::OpenCode), false, false, true),
            (Some(CLIAgent::Pi), false, false, true),
            // 问过了就不再骚扰（用户选「暂不」的语义）——对每个 agent 独立成立。
            (Some(CLIAgent::Claude), true, false, false),
            (Some(CLIAgent::OpenCode), true, false, false),
            (Some(CLIAgent::Pi), true, false, false),
            // 已经装好了没什么可问的。
            (Some(CLIAgent::Claude), false, true, false),
            (Some(CLIAgent::OpenCode), false, true, false),
            (Some(CLIAgent::Pi), false, true, false),
            // 前台没有 agent。
            (None, false, false, false),
            // CodeBuddy 两边都不支持（走解释窗，由调用方处理），
            // 其余 agent 静默——没有可装的机制。
            (Some(CLIAgent::CodeBuddy), false, false, false),
            (Some(CLIAgent::Codex), false, false, true),
            (Some(CLIAgent::Qwen), false, false, true),
        ];
        for (agent, asked, installed, expected) in cases {
            assert_eq!(
                should_prompt_hook_install(agent, asked, installed),
                expected,
                "agent={agent:?} asked={asked} installed={installed}"
            );
        }
    }

    /// 每个可安装的 agent 抛出「可安装」之后，必须真的**算得出**安装计划。
    ///
    /// 这两处是两份独立数据（[`INSTALLABLE_AGENTS`] 与 [`hook_install_plan`] 的
    /// match 分支），不一致的失败模式是：弹窗出现了、用户点了「安装」、
    /// 然后什么都没发生。
    #[test]
    fn every_installable_agent_has_a_plan() {
        for agent in INSTALLABLE_AGENTS {
            // Claude 走 settings.json 合并那条独立路径，见其文档。
            if *agent == CLIAgent::Claude {
                continue;
            }
            let plan = hook_install_plan(*agent)
                .unwrap_or_else(|| panic!("{agent:?} 可安装，但没有安装计划"));
            assert!(
                plan.content.contains(plan.installed_marker),
                "{agent:?} 的安装标记在内容里找不到——会永远被判为「未安装」，\
                 于是反复弹窗。标记={:?}",
                plan.installed_marker
            );
            assert!(!plan.rel_path.is_empty(), "{agent:?} 的路径为空");
        }
    }

    /// tty7 有、早先缺失的适配器：装上之后必须真的能产出对应形状的配置。
    ///
    /// 这几个的失败模式是静默的——文件写下去了、agent 也读了，但事件名或
    /// 形状不对，状态点永远不亮，用户只会以为"这东西没用"。
    #[test]
    fn the_adapters_added_for_tty7_parity_produce_usable_configs() {
        // Cursor：扁平形状 + 根级 version。没有 version 它连 hooks 一起忽略。
        let cursor = hook_install_plan(CLIAgent::Cursor).unwrap();
        assert_eq!(cursor.rel_path, ".cursor/hooks.json");
        let doc: serde_json::Value = serde_json::from_str(&cursor.content).unwrap();
        assert_eq!(doc["version"], 1, "Cursor 会忽略没有 version 的文件");
        let before = &doc["hooks"]["beforeSubmitPrompt"];
        assert!(
            before[0].get("command").is_some() && before[0].get("hooks").is_none(),
            "Cursor 的条目是扁平的：{before}"
        );

        // Qoder 中国大陆版：事件表与国际版一致，只是路径不同。
        let cn = hook_install_plan(CLIAgent::QoderCLICn).unwrap();
        assert_eq!(cn.rel_path, ".qoder-cn/settings.json");
        let global = hook_install_plan(CLIAgent::QoderCLI).unwrap();
        assert_eq!(
            adapter_events(CLIAgent::QoderCLICn),
            adapter_events(CLIAgent::QoderCLI),
            "两个 Qoder 变体必须共用同一张事件表"
        );
        assert_ne!(cn.rel_path, global.rel_path);

        // Empryo：Claude 兼容形状，含 StopFailure（否则出错回合卡在 Working）。
        let empryo = hook_install_plan(CLIAgent::Empryo).unwrap();
        assert_eq!(empryo.rel_path, ".empryo/hooks.json");
        let doc: serde_json::Value = serde_json::from_str(&empryo.content).unwrap();
        assert!(
            doc["hooks"]["StopFailure"].is_array(),
            "Empryo 必须装 StopFailure：{}",
            empryo.content
        );

        // jcode：TOML 键值对形状，不是表数组。
        let jcode = hook_install_plan(CLIAgent::Jcode).unwrap();
        assert_eq!(jcode.rel_path, ".jcode/config.toml");
        let doc = jcode.content.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["hooks"]["turn_start"].as_str(),
            Some(&reporter_command("jcode", "prompt-submit")[..]),
            "jcode 的钩子是 [hooks] 下的键值对"
        );
    }

    /// Cursor 的 hooks.json 必须带根级 `version`——它会**整个忽略**没有
    /// version 的文件，所以合并时必须把它补上（哪怕是空配置首次安装）。
    #[test]
    fn cursor_merge_keeps_the_version_key() {
        let plan = hook_install_plan(CLIAgent::Cursor).unwrap();
        let merged = merge_bridge_config(CLIAgent::Cursor, &plan, b"").unwrap();
        let doc: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            doc["version"], 1,
            "Cursor 会忽略没有 version 的文件：{merged}"
        );
        assert!(
            bridge_installed(CLIAgent::Cursor, &plan, merged.as_bytes()),
            "装完必须被认成已安装"
        );

        // 已有第三方条目时也要补上 version，且不能动别人的条目。
        let existing = br#"{"hooks":{"beforeSubmitPrompt":[{"command":"third-party"}]}}"#;
        let merged = merge_bridge_config(CLIAgent::Cursor, &plan, existing).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(doc["version"], 1);
        assert!(
            doc["hooks"]["beforeSubmitPrompt"]
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e["command"] == "third-party")),
            "第三方条目必须保留：{merged}"
        );
        // 没有 version 就不算装好（Cursor 不会读这种文件）。
        let broken = merged.replace("\"version\": 1", "\"version\": 2");
        assert!(
            !bridge_installed(CLIAgent::Cursor, &plan, broken.as_bytes()),
            "version 对不上就不算装好"
        );
    }

    /// jcode 合并必须保留用户与第三方条目，且对字符串和字符串数组两种
    /// 命令形态都能清掉我们自己的旧条目（`toml_edit` 把数组也归到 `Value`）。
    ///
    /// 完整性检测也必须是**语义**的：用户的 `config.toml` 里还有自己的模型
    /// 设置，合并结果永远不等于我们生成的模板，整段比较会永远判成「没装」。
    #[test]
    fn jcode_merge_preserves_foreign_entries_and_clears_ours() {
        let plan = hook_install_plan(CLIAgent::Jcode).unwrap();
        let ours = reporter_command("jcode", "stop");
        let existing = format!(
            "model = 'fixture'\n\n[hooks]\nturn_start = '{ours}'\n\
             post_tool = ['{ours}', 'third-party-notify']\nturn_end = 'third-party-stop'\n\
             my_own = 'keep-me'\n"
        );
        assert!(
            !bridge_installed(CLIAgent::Jcode, &plan, existing.as_bytes()),
            "合并前当然不算已安装"
        );
        let merged = merge_bridge_config(CLIAgent::Jcode, &plan, existing.as_bytes()).unwrap();
        let doc = merged.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("fixture"));
        assert_eq!(doc["hooks"]["my_own"].as_str(), Some("keep-me"));
        let post_tool = &doc["hooks"]["post_tool"];
        assert!(
            post_tool
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("third-party-notify"))),
            "第三方命令必须留在原数组里：{post_tool}"
        );
        // 用户自己在同一个事件键上挂的字符串命令不能被覆盖：升级成数组，
        // 第三方在前、我们在后。
        let turn_end = &doc["hooks"]["turn_end"];
        assert!(
            turn_end
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("third-party-stop"))
                    && a.iter().any(|v| v.as_str() == Some(&ours[..]))),
            "字符串形态的第三方命令必须保留：{turn_end}"
        );
        // 装完就算装好了——哪怕配置里还有用户自己的内容。
        assert!(
            bridge_installed(CLIAgent::Jcode, &plan, merged.as_bytes()),
            "合并后的配置必须被认成已安装"
        );
        // 少一个事件就不算装好。
        let broken = merged.replace(&ours, "gone");
        assert!(
            !bridge_installed(CLIAgent::Jcode, &plan, broken.as_bytes()),
            "缺了事件命令就不算装好"
        );
        // 幂等：再合一次不变。
        assert_eq!(
            merge_bridge_config(CLIAgent::Jcode, &plan, merged.as_bytes()).unwrap(),
            merged
        );
    }

    /// 不可安装的 agent 不该算出计划——否则调用方会写一个没有意义的文件。
    #[test]
    fn non_installable_agents_have_no_plan() {
        for agent in [
            CLIAgent::CodeBuddy,
            CLIAgent::Aider,
            CLIAgent::Amp,
            CLIAgent::Auggie,
            CLIAgent::Hermes,
            CLIAgent::Vibe,
            CLIAgent::Antigravity,
            CLIAgent::PrimeAgent,
            CLIAgent::Muse,
        ] {
            assert!(
                hook_install_plan(agent).is_none(),
                "{agent:?} 不该有安装计划"
            );
        }
    }

    /// `StopFailure` 必须映射到 `Stop` 而不是 `Notification`。
    ///
    /// 这是本模块最容易「看起来对、实则错」的一处：`Notification` 只在当前是
    /// `Working` 时才降级为 `Waiting`，所以如果用户在权限弹窗（`Waiting`）上按
    /// Esc 打断，那个映射会让状态**停在 Waiting**，状态点依旧不归位。
    #[test]
    fn stop_failure_maps_to_stop_not_notification() {
        let spec = CLAUDE_HOOK_EVENTS
            .iter()
            .find(|s| s.claude_event == "StopFailure")
            .expect("必须安装 StopFailure，否则 Esc 打断后状态点卡在 Working");
        assert_eq!(spec.kind, AgentEventKind::Stop);

        // 反面确认：这条路径确实能收尾（Waiting → Done 由 apply_event 保证，
        // 见 cli_agent.rs 的 Stop 分支）。
        assert_ne!(spec.kind, AgentEventKind::Notification);
    }

    /// `PreToolUse` 只在 `AskUserQuestion` 时上报成 `question-asked`，
    /// 其余工具调用整个丢掉——它是中间态，不是状态。
    #[test]
    fn pre_tool_use_reports_only_questions() {
        let spec = claude_pre_tool_use_spec();
        assert_eq!(spec.kind, AgentEventKind::QuestionAsked);
    }

    fn claude_pre_tool_use_spec() -> HookEventSpec {
        CLAUDE_HOOK_EVENTS
            .iter()
            .copied()
            .find(|s| s.claude_event == "PreToolUse")
            .expect("PreToolUse 必须装上")
    }

    /// **端到端**：把生成的脚本喂给真实的 `sh` 执行，确认它输出的字节能被
    /// 真实的解析器认出来。
    ///
    /// 这条是整个模块里最有价值的测试，因为它验证的是**别人真正消费的产物**：
    /// Claude Code 拿到的是脚本 stdout 里那行 JSON，终端看到的是其中的
    /// `terminalSequence` 展开后的字节。之前用「字符串里有没有 `$(`」之类的
    /// 断言完全覆盖不到这里——那个 bug 正是 `$(printf '\033')` 写在**单引号内**
    /// 从不展开，脚本语法检查（`sh -n`）照样通过，只有真的跑一遍才会暴露。
    ///
    /// 同时覆盖 [`sh`] 的另一个失败模式：`emit` 把转义后的序列当 `printf` 的
    /// 格式串用，若 `%` 没有翻倍，printf 会吐错误而不是载荷。
    #[cfg(unix)]
    #[test]
    fn generated_script_run_by_real_sh_produces_parsable_bytes() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("deepterm-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let script_path = dir.join("claude-hook.sh");
        std::fs::write(&script_path, hook_script()).expect("写脚本");
        // 脚本要可执行；这里虽然用 `sh <path>` 调用，但保持与实际部署一致。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        // 跑一次脚本，拿 stdout。
        let run = |event: &str, input: &str| -> String {
            let mut child = Command::new("sh")
                .arg(&script_path)
                .arg(event)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("起 sh");
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("写 stdin");
            let out = child.wait_with_output().expect("等 sh");
            assert!(
                out.status.success(),
                "{event} 退出码非零：{:?}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).expect("stdout 是 UTF-8")
        };

        // 每个事件跑一遍，确认都能产出可解析的载荷。
        for spec in CLAUDE_HOOK_EVENTS {
            let stdout = run(spec.claude_event, r#"{"session_id":"s-1","cwd":"/tmp"}"#);
            // `PreToolUse` 只在提问时上报（下面单独验证），普通工具调用无输出。
            if spec.claude_event == "PreToolUse" {
                assert!(
                    stdout.trim().is_empty(),
                    "{} 不该上报普通工具调用：{stdout:?}",
                    spec.claude_event
                );
                continue;
            }
            // 真实场景里 Claude Code 解析的是这行 JSON。
            let wrapper: serde_json::Value =
                serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
                    panic!(
                        "{} 的输出不是合法 JSON（{e}）：{stdout:?}",
                        spec.claude_event
                    )
                });
            let seq = wrapper["terminalSequence"]
                .as_str()
                .unwrap_or_else(|| panic!("{} 缺 terminalSequence 字段", spec.claude_event));

            // 逐字锁住 Claude Code 白名单要求的形状：整个值只含**一种**序列，
            // 以 ESC 引导、以 BEL 终止。这不是格式洁癖——`terminalSequence`
            // 只要含白名单外的内容，Claude Code 会**丢弃整个字段**，
            // 而且不报错。
            assert_eq!(
                seq,
                claude_terminal_sequence(spec.kind),
                "{} 的字节形状与 Rust 侧生成的不一致",
                spec.claude_event
            );
            // 白名单要求：只能含 OSC 777 一种序列。
            assert!(
                seq.starts_with("\u{1b}]777;notify;"),
                "{} 不是 OSC 777：{seq:?}",
                spec.claude_event
            );
            // 终止符必须是 BEL 或 ST——这里用 BEL。
            assert!(
                seq.ends_with('\u{7}'),
                "{} 没有 BEL 终止",
                spec.claude_event
            );
            // 序列里不能出现第二处 ESC：混入 CSI 等白名单外内容会让整段被丢弃。
            assert_eq!(
                seq.matches('\u{1b}').count(),
                1,
                "{} 含多处 ESC，白名单只允许单一序列：{seq:?}",
                spec.claude_event
            );

            let inner = seq
                .strip_prefix("\u{1b}]")
                .and_then(|s| s.strip_suffix('\u{7}'))
                .expect("剥 OSC 引导符与终止符");
            let event = parse_agent_event(inner.as_bytes())
                .unwrap_or_else(|| panic!("{} 的字节解析失败：{inner:?}", spec.claude_event));
            assert_eq!(event.kind, spec.kind, "{}", spec.claude_event);
            assert_eq!(event.agent, Some(CLIAgent::Claude));
            // 脚本**刻意不转发** session_id / cwd：那需要 sed 从 stdin 抽字段，
            // 而 agent 身份已经由脚本自己声明（`"agent":"claude"`），
            // 终端侧的门控建 session 也只要求「事件里带 agent」。
            // 少一个 sed 就少一个在远端 shell 上出岔子的地方。
            assert_eq!(event.session_id, None);
        }

        // `PreToolUse` 只在 AskUserQuestion 时上报成 question-asked——它是不走
        // 权限提示的提问通道，漏了它 tab 会一直显示"工作中"。
        for input in [
            r#"{"tool_name":"AskUserQuestion"}"#,
            "{\n  \"tool_name\": \"AskUserQuestion\"\n}",
        ] {
            let stdout = run("PreToolUse", input);
            let wrapper: serde_json::Value = serde_json::from_str(stdout.trim())
                .unwrap_or_else(|e| panic!("PreToolUse 的输出不是合法 JSON（{e}）：{stdout:?}"));
            let seq = wrapper["terminalSequence"]
                .as_str()
                .expect("缺 terminalSequence 字段");
            let inner = seq
                .strip_prefix("\u{1b}]")
                .and_then(|s| s.strip_suffix('\u{7}'))
                .expect("剥 OSC 引导符与终止符");
            let event = parse_agent_event(inner.as_bytes()).expect("载荷应当可解析");
            assert_eq!(event.kind, AgentEventKind::QuestionAsked);
            assert_eq!(event.agent, Some(CLIAgent::Claude));
        }
        // 别的工具调用只是中间态，直接丢掉。
        assert!(
            run("PreToolUse", r#"{"tool_name":"Bash"}"#)
                .trim()
                .is_empty(),
            "普通工具调用不该上报"
        );

        // subagent 仍在跑时，`Stop` 必须**什么都不输出**——否则主 agent 每次
        // 让出都会点亮一次绿点（每个 subagent 一次）。
        assert!(
            run(
                "Stop",
                r#"{"background_tasks":[{"type":"subagent","status":"running"}]}"#
            )
            .is_empty(),
            "subagent 还在跑时 Stop 不该上报"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 脚本里嵌入的载荷必须是**shell 转义**形式，且不带裸控制字节。
    ///
    /// 裸 ESC/BEL 直接写进脚本正文会让文件难以编辑与传输（不少编辑器和
    /// scp/heredoc 路径会吃掉它们）。
    #[test]
    fn embedded_payload_is_shell_escape_safe() {
        let script = hook_script();
        assert!(
            !script.contains('\u{1b}'),
            "脚本正文里不该出现裸 ESC——应当写成 \\033"
        );
        assert!(
            !script.contains('\u{7}'),
            "脚本正文里不该出现裸 BEL——应当写成 \\007"
        );
        assert!(
            script.contains("\\033]777;notify;"),
            "载荷应当是 shell 转义形式（字面 \\033）"
        );
        // 内层 JSON 的引号在脚本**载荷文本**里应当是原样的；把它们变成 JSON 转义
        // 是运行时 `sed` 的职责（见 `emit` 的注释）。在载荷里预先转义会让
        // `%b` 原样吐出反斜杠，破坏载荷——这正是 E2E 测试抓到过的 bug。
        //
        // 断言只看 `emit '...'` 的载荷，不能对整份脚本做子串否证：
        // `emit` 的 sed 脚本本身就含 `s/"/\\"/g`，那是对的实现，不是缺陷。
        let payload_line = script
            .lines()
            .find(|l| l.trim_start().starts_with("emit '"))
            .expect("脚本里应当有 emit 调用");
        assert!(
            payload_line.contains("{\"v\":1,"),
            "载荷里的内层 JSON 引号应当原样：{payload_line}"
        );
        assert!(
            !payload_line.contains("\\\"v\\\""),
            "载荷里不该预先转义内层引号——%b 会把反斜杠原样吐出：{payload_line}"
        );
    }

    /// printf 格式串的转义：控制字符写成 shell 转义，单引号断开重接。
    #[test]
    fn printf_format_escaping_is_safe() {
        assert_eq!(esc_for_shell_single_quoted("plain"), "plain");
        assert_eq!(esc_for_shell_single_quoted("it's"), "it'\\''s");
        // 引号与反斜杠原样保留：它们属于 OSC 载荷，不该被 JSON 转义。
        assert_eq!(esc_for_shell_single_quoted("\"x\""), "\"x\"");
        assert_eq!(esc_for_shell_single_quoted("\u{1b}x\u{7}"), "\\033x\\007");
        // 真实载荷：ESC 与 BEL 都转义，无裸控制字节；内层引号保持原样。
        let seq = claude_terminal_sequence(AgentEventKind::Stop);
        let escaped = esc_for_shell_single_quoted(&seq);
        assert!(escaped.starts_with("\\033]777;"), "{escaped}");
        assert!(escaped.ends_with("\\007"), "{escaped}");
        assert!(
            escaped.contains("{\"v\":1"),
            "内层引号不该被转义：{escaped}"
        );
        assert!(!escaped.contains('\u{1b}'), "不该有裸 ESC");
        assert!(!escaped.contains('\u{7}'), "不该有裸 BEL");
    }
}
