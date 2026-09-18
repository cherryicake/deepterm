//! CLI agent 的识别、状态与事件协议。
//!
//! 移植自 tty7 的 `crates/tty7-core/src/core/cli_agent.rs`。三条主线：
//!
//! 1. **识别**（[`CLIAgent`]）：从进程 argv 或用户敲的命令行判断前台跑的是哪个 agent。
//!    agent 可能被包装在 `node` / `npx` / `python` 里，所以不是简单的文件名比对。
//! 2. **状态**（[`AgentStatus`] / [`AgentSessionState`]）：agent 的一次「回合」——
//!    提交提示词 → 干活 → 需要授权 → 结束——映射成 Tab 上可见的四个状态。
//! 3. **事件**（[`AgentEvent`] / [`parse_agent_event`]）：agent 通过 hook 发回、
//!    经 OSC 777 承载的载荷。见 [`AGENT_EVENT_SENTINEL`]。
//!
//! 与 tty7 的差异见 [`AgentSessionState`] 的文档。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 支持识别的 CLI agent。
///
/// 新增变体一律**追加到末尾**：这个枚举参与会话状态的序列化，
/// 打乱已有顺序会让旧版本客户端读到错位的判别值。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CLIAgent {
    Claude,
    Codex,
    Gemini,
    Aider,
    Amp,
    OpenCode,
    Copilot,
    Cursor,
    Goose,
    Droid,
    Pi,
    Auggie,
    Hermes,
    Vibe,
    Antigravity,
    Grok,
    Qwen,
    OhMyPi,
    Kimi,
    TraeCode,
    QoderCLI,
    Crush,
    CodeBuddy,
}

impl CLIAgent {
    /// 全部 agent，顺序即 UI 列表顺序。
    pub const ALL: [CLIAgent; 23] = [
        CLIAgent::Claude,
        CLIAgent::Codex,
        CLIAgent::TraeCode,
        CLIAgent::Gemini,
        CLIAgent::Aider,
        CLIAgent::Amp,
        CLIAgent::OpenCode,
        CLIAgent::Copilot,
        CLIAgent::Cursor,
        CLIAgent::Goose,
        CLIAgent::Droid,
        CLIAgent::Pi,
        CLIAgent::Auggie,
        CLIAgent::Hermes,
        CLIAgent::Vibe,
        CLIAgent::Antigravity,
        CLIAgent::Grok,
        CLIAgent::Qwen,
        CLIAgent::OhMyPi,
        CLIAgent::Kimi,
        CLIAgent::QoderCLI,
        CLIAgent::Crush,
        CLIAgent::CodeBuddy,
    ];

    /// 各 agent 在进程名 / 命令名上出现过的全部拼写。
    fn aliases(self) -> &'static [&'static str] {
        match self {
            CLIAgent::Claude => &["claude", "claude-code"],
            CLIAgent::Codex => &["codex", "codex-cli"],
            CLIAgent::TraeCode => &["traecli", "traex"],
            CLIAgent::Gemini => &["gemini", "gemini-cli"],
            CLIAgent::Aider => &["aider", "aider-chat"],
            CLIAgent::Amp => &["amp"],
            CLIAgent::OpenCode => &["opencode"],
            CLIAgent::Copilot => &["copilot"],
            CLIAgent::Cursor => &["cursor-agent"],
            CLIAgent::Goose => &["goose"],
            CLIAgent::Droid => &["droid"],
            CLIAgent::Pi => &["pi"],
            CLIAgent::Auggie => &["auggie"],
            CLIAgent::Hermes => &["hermes"],
            CLIAgent::Vibe => &["vibe", "vibe-acp"],
            // 只认 `agy`。IDE 装的 `antigravity` 是 VS Code 的 `code` 那种启动器
            // 外壳，不是终端 agent；而且这个名字还与 `python3 -m antigravity`
            // 撞车——那是触发 Python 自带彩蛋的标准写法。
            CLIAgent::Antigravity => &["agy"],
            CLIAgent::Grok => &["grok"],
            CLIAgent::Qwen => &["qwen", "qwen-code"],
            // Oh My Pi 是 Pi 的 fork，但它只发自己的二进制、从不装 `pi`，
            // 所以两个名字不会互相干扰。
            CLIAgent::OhMyPi => &["omp"],
            // 独立版 Kimi Code CLI 与遗留的开源 kimi-cli 都装 `kimi`——
            // 同一厂商、同一品牌，一次识别覆盖两者。只有独立版带 hook。
            CLIAgent::Kimi => &["kimi", "kimi-code"],
            // npm 包会装两个二进制，而文档让人跑的是 `qoder`：裸调用、带 flag、
            // 带提示词时它转发给 CLI，只有 `ide`/`chat`/`serve-web`/`tunnel`
            // 或路径存在时才交给 IDE。只认 `qodercli` 会漏掉所有按文档启动的
            // 会话，因为 pty 看到的是那个转发器。代价是 IDE 启动也会被认成
            // CLI——在转发器退出前它会一直挂着 CLI 的头像。
            CLIAgent::QoderCLI => &["qoder", "qodercli"],
            // Charm 的终端 agent。一个二进制，PATH 上的名字就是它的启动名。
            CLIAgent::Crush => &["crush"],
            // 腾讯云代码助手。npm 包 `@tencent-ai/codebuddy-code` 装的命令是
            // `codebuddy`；Homebrew（`brew install codebuddy-code`）与原生二进制
            // 安装脚本还额外提供 `codebuddy-code`。两个都认。
            CLIAgent::CodeBuddy => &["codebuddy", "codebuddy-code"],
        }
    }

    /// 稳定的短标识。用于 hook 命令参数、配置项、序列化。
    pub fn slug(self) -> &'static str {
        match self {
            CLIAgent::Claude => "claude",
            CLIAgent::Codex => "codex",
            CLIAgent::TraeCode => "traecli",
            CLIAgent::Gemini => "gemini",
            CLIAgent::Aider => "aider",
            CLIAgent::Amp => "amp",
            CLIAgent::OpenCode => "opencode",
            CLIAgent::Copilot => "copilot",
            CLIAgent::Cursor => "cursor",
            CLIAgent::Goose => "goose",
            CLIAgent::Droid => "droid",
            CLIAgent::Pi => "pi",
            CLIAgent::Auggie => "auggie",
            CLIAgent::Hermes => "hermes",
            CLIAgent::Vibe => "vibe",
            CLIAgent::Antigravity => "antigravity",
            CLIAgent::Grok => "grok",
            CLIAgent::Qwen => "qwen",
            CLIAgent::OhMyPi => "omp",
            CLIAgent::Kimi => "kimi",
            CLIAgent::QoderCLI => "qodercli",
            CLIAgent::Crush => "crush",
            CLIAgent::CodeBuddy => "codebuddy",
        }
    }

    pub fn from_slug(name: &str) -> Option<CLIAgent> {
        let name = name.trim().to_ascii_lowercase();
        CLIAgent::ALL.into_iter().find(|a| a.slug() == name)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            CLIAgent::Claude => "Claude Code",
            CLIAgent::Codex => "Codex",
            CLIAgent::TraeCode => "TraeCode",
            CLIAgent::Gemini => "Gemini",
            CLIAgent::Aider => "Aider",
            CLIAgent::Amp => "Amp",
            CLIAgent::OpenCode => "OpenCode",
            CLIAgent::Copilot => "Copilot",
            CLIAgent::Cursor => "Cursor",
            CLIAgent::Goose => "Goose",
            CLIAgent::Droid => "Droid",
            CLIAgent::Pi => "Pi",
            CLIAgent::Auggie => "Auggie",
            CLIAgent::Hermes => "Hermes",
            CLIAgent::Vibe => "Vibe",
            CLIAgent::Antigravity => "Antigravity",
            CLIAgent::Grok => "Grok",
            CLIAgent::Qwen => "Qwen Code",
            CLIAgent::OhMyPi => "Oh My Pi",
            CLIAgent::Kimi => "Kimi Code",
            CLIAgent::QoderCLI => "Qoder CLI",
            CLIAgent::Crush => "Crush",
            CLIAgent::CodeBuddy => "CodeBuddy",
        }
    }

    /// Tab 头像圆盘的填充色：agent 的品牌色。
    ///
    /// 取值一律以**官方一手来源**为准（官网 CSS/logo 资源、官方 npm 包内资源、
    /// 官方 favicon），不用配色聚合站。判定 logo 颜色时要区分「标志本身的颜色」
    /// 与「favicon 的底色」——两者常不同，前者才是品牌色。
    ///
    /// 不少 agent 的官方 logo 本就是单色的（OpenAI、xAI、Qoder、Cursor、Factory、
    /// Kimi、OpenCode、Augment），此时取它们的黑白主色，而不是编造一个色相。
    pub fn accent_rgb(self) -> u32 {
        match self {
            // claude.com 品牌 CSS 的 --swatch--clay: #d97757。
            CLIAgent::Claude => 0xD97757,
            // openai.com/favicon.svg 用 fill="currentColor"，官方标志为单色。
            CLIAgent::Codex => 0x000000,
            // TRAE 官方 favicon 底色 #0A0B0D，中心图形是 #32F08C 绿（见 icon_rgb）。
            CLIAgent::TraeCode => 0x0A0B0D,
            // Gemini 官方 sparkle SVG 与 favicon 的主色均为 #3186ff。
            CLIAgent::Gemini => 0x3186FF,
            // aider.chat/assets/logo.svg 的 fill 就是 #14B014。
            CLIAgent::Aider => 0x14B014,
            CLIAgent::Amp => 0xF34E3F,
            // OpenCode 官方 favicon 与 app-icon 的底色 #131010（近黑单色）。
            CLIAgent::OpenCode => 0x131010,
            // brand.github.com 明文「Copilot Purple」#8534F3。
            CLIAgent::Copilot => 0x8534F3,
            // Cursor 官方 favicon 与站内 logo 均无彩色（#EDECEC 灰白系）。
            CLIAgent::Cursor => 0x9AA0A6,
            // block/goose 官方 logo.svg 的 fill #3ECC5F。
            CLIAgent::Goose => 0x3ECC5F,
            // Factory 官网（含 CLI 产品页）唯一可见的品牌色 #d15010。
            CLIAgent::Droid => 0xD15010,
            // pi.dev 官方 logo 的三段色之一，取最暖、辨识度最高的那段。
            CLIAgent::Pi => 0xF09082,
            // augmentcode.com/favicon.svg 为纯黑白，官方标志单色。
            CLIAgent::Auggie => 0x000000,
            CLIAgent::Hermes => 0x8B5CF6,
            // mistral.ai 官方 CSS 的 #fa500f（出现 32 次，为主色）。
            CLIAgent::Vibe => 0xFA500F,
            // antigravity.google 官方 logo.png 的主色 #3087FB。
            CLIAgent::Antigravity => 0x3087FB,
            // x.ai 官方 icon.png 为纯灰度（R=G=B），标志单色。
            CLIAgent::Grok => 0x000000,
            CLIAgent::Qwen => 0x6336E7,
            CLIAgent::OhMyPi => 0xF97316,
            // kimi.com 官方 favicon（light/dark 两版）都是黑白单色标志。
            CLIAgent::Kimi => 0x000000,
            // Qoder 官方站点实际显示的 logo 是纯黑（qoder.com 的 lightOnly
            // 变体 PNG，98.3% 像素为 #0F0D0C）。页面上那个 `theme-color:
            // #8B5CF6` 是残留的 meta 标签，站点里没有任何可见元素用它。
            CLIAgent::QoderCLI => 0x0F0D0C,
            // charm.sh 官方 main.css 的 background-color:#6b50ff。
            CLIAgent::Crush => 0x6B50FF,
            // codebuddy.cn 官方渐变 #6C4DFF → #583ED3，取亮端。
            CLIAgent::CodeBuddy => 0x6C4DFF,
        }
    }

    /// 圆盘上的单色图形。SVG 作为 alpha mask 渲染，品牌底色由 `accent_rgb` 提供。
    ///
    /// 图形颜色由**底色亮度**决定，而不是写死一张表：16px 圆盘里的图形只有约
    /// 8.6px，白字对比度低于 3:1 就糊成一团。所以底色够深时用白，偏亮时翻黑
    /// （Goose 的官方绿 `#3ECC5F` 白字仅 2.1:1，黑字 10.0:1）。这样以后
    /// 再调品牌色，图形会自动跟着适配，不会留下黑压黑/白压白的死角。
    pub fn icon_rgb(self) -> u32 {
        match self {
            // TRAE 的 logo 自带绿色前景（官方 favicon 中心就是 #32F08C），
            // 它不是「单色 mask」而是品牌图形的一部分，所以不走亮度规则。
            CLIAgent::TraeCode => 0x32F08C,
            _ if contrast_against_white(self.accent_rgb()) >= 3.0 => 0xFFFFFF,
            _ => 0x000000,
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            CLIAgent::Claude => "icons/agents/claude.svg",
            CLIAgent::Codex => "icons/agents/codex.svg",
            CLIAgent::TraeCode => "icons/agents/traecli.svg",
            CLIAgent::Gemini => "icons/agents/gemini.svg",
            CLIAgent::Amp => "icons/agents/amp.svg",
            CLIAgent::OpenCode => "icons/agents/opencode.svg",
            CLIAgent::Copilot => "icons/agents/copilot.svg",
            CLIAgent::Cursor => "icons/agents/cursor.svg",
            CLIAgent::Goose => "icons/agents/goose.svg",
            CLIAgent::Droid => "icons/agents/droid.svg",
            CLIAgent::Grok => "icons/agents/grok.svg",
            CLIAgent::Pi => "icons/agents/pi.svg",
            CLIAgent::OhMyPi => "icons/agents/omp.svg",
            CLIAgent::Qwen => "icons/agents/qwen.svg",
            CLIAgent::Kimi => "icons/agents/kimi.svg",
            CLIAgent::QoderCLI => "icons/agents/qodercli.svg",
            CLIAgent::Crush => "icons/agents/crush.svg",
            CLIAgent::CodeBuddy => "icons/agents/codebuddy.svg",
            CLIAgent::Aider
            | CLIAgent::Auggie
            | CLIAgent::Hermes
            | CLIAgent::Vibe
            | CLIAgent::Antigravity => "icons/bot.svg",
        }
    }

    fn match_token(token: &str) -> Option<CLIAgent> {
        CLIAgent::ALL
            .into_iter()
            .find(|a| a.aliases().contains(&token))
    }

    /// 从 argv 判断跑的是哪个 agent。
    ///
    /// `custom` 是用户配置的「包装器名 → agent slug」映射，用来覆盖自定义脚本。
    pub fn detect_from_argv_with(
        argv: &[String],
        custom: &HashMap<String, String>,
    ) -> Option<CLIAgent> {
        let mut rest = argv
            .iter()
            .map(String::as_str)
            .skip_while(|t| is_env_assignment(t));

        let launcher = rest.next()?;
        let launcher_stem = base_stem(launcher);

        if let Some(agent) = CLIAgent::match_token(launcher_stem) {
            return Some(agent);
        }
        if let Some(agent) = custom
            .get(&launcher_stem.to_ascii_lowercase())
            .and_then(|slug| CLIAgent::from_slug(slug))
        {
            return Some(agent);
        }

        // 被解释器包着时（`node .../claude-code/cli.js`、`python -m aider`），
        // 参数里才藏着真正的 agent 名。
        if is_interpreter(launcher_stem) {
            for arg in rest {
                if arg.starts_with('-') {
                    continue;
                }
                for segment in arg.split(['/', '\\']) {
                    if let Some(agent) =
                        CLIAgent::match_token(&base_stem(segment).to_ascii_lowercase())
                    {
                        return Some(agent);
                    }
                }
            }
        }

        None
    }

    /// 从 shell 集成捕获的整条命令行判断。
    ///
    /// 与 [`Self::detect_from_argv_with`] 的区别：输入是**用户敲的那一行**，
    /// 而不是 pty 上的进程 argv，所以首 token 可能是 `cd` 之类的前缀。
    pub fn detect_from_command_with(
        command: &str,
        custom: &HashMap<String, String>,
    ) -> Option<CLIAgent> {
        let argv: Vec<String> = command_argv(command)
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        Self::detect_from_argv_with(&argv, custom)
    }
}

/// 一个 sRGB 十六进制色与**白色**的 WCAG 对比度（1.0 = 同色，21 = 纯黑对纯白）。
///
/// 自带实现而不是复用 `workspace::tab_bar` 的 `contrast_ratio`：那个吃
/// `gpui::Rgba`，而这个模块是纯逻辑层（识别、状态机、事件协议），除测试外
/// 不依赖 gpui。为了一个 3 行的亮度计算把渲染依赖引进核心层不划算。
fn contrast_against_white(hex: u32) -> f32 {
    let linear = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let [_, r, g, b] = hex.to_be_bytes().map(|b| b as f32 / 255.0);
    let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    1.05 / (luminance + 0.05)
}

/// 把 shell 集成捕获的命令行切成 argv，**保留大小写**。
///
/// 保留大小写是为了让它能作为 `launch_argv` 供 resume 时回放 flag。
/// 带空格的引号参数会被切碎；回放逻辑（见 `replay_flags`）宁可整体放弃，
/// 也不会把这种 token 错误地回放出去。
pub fn command_argv(command: &str) -> Vec<String> {
    let mut argv: Vec<String> = command
        .split_whitespace()
        .map(|t| t.trim_matches(['"', '\'']).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if argv.first().is_some_and(|t| t == "&") {
        argv.remove(0);
    }
    argv
}

/// 判断一个 token 是不是 `KEY=value` 形式的环境变量赋值。
///
/// 用来跳过 `FOO=1 claude` 这类前缀，找到真正的启动命令。
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((key, _)) => {
            let mut bytes = key.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
        None => false,
    }
}

/// 从一个路径或命令名里取出「基名去掉扩展名」。
///
/// `node` 包装的 agent 跑的是 `cli.js`，`pi` 之类是原生二进制，
/// 两种都要能还原成 `claude` / `pi` 这样的 token。
fn base_stem(token: &str) -> &str {
    let trimmed = token.trim_end_matches(['/', '\\']);
    let name = match trimmed.rfind(['/', '\\']) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    for ext in [
        ".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".sh", ".exe", ".cmd", ".bat", ".ps1",
    ] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

/// 判断一个 token 是不是「会接收脚本路径」的解释器 / 包运行器。
///
/// 只有它们后面的参数才值得去翻 agent 名——否则 `grep node` 也会被认成 Claude。
fn is_interpreter(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "node"
            | "nodejs"
            | "bun"
            | "deno"
            | "npx"
            | "pnpm"
            | "yarn"
            | "python"
            | "python3"
            | "ruby"
            | "uv"
            | "uvx"
            | "env"
    )
}

/// agent 事件信道的哨兵标识。
///
/// agent hook 把事件包成 `OSC 777;notify;<sentinel>;<json>` 发回来。
/// 借用 `777;notify` 是因为它是终端已广泛支持的桌面通知信道，不需要新协议；
/// 哨兵的作用是把它与真正的用户通知区分开——没有它，每个 agent 事件
/// 都会弹出一条桌面通知。
pub const AGENT_EVENT_SENTINEL: &str = "deepterm://cli-agent";

/// agent 在一次会话里的状态。
///
/// Tab 上只画 [`AgentStatus::Working`] / [`Waiting`] / [`Done`] 三态，
/// `Idle` 不画点（agent 开着但没在干活，图标本身已经说明了它的存在）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    #[default]
    Idle,
    Working,
    Waiting,
    Done,
}

impl AgentStatus {
    /// 状态点颜色；`None` 表示不画点。
    pub fn dot_rgb(self) -> Option<u32> {
        match self {
            AgentStatus::Idle => None,
            AgentStatus::Working => Some(0x3B82F6),
            // Working 与 Done 只差色相（蓝 / 绿），而 Waiting 与 Done——
            // 真正决定「要不要过去看一眼」的那一对——是琥珀与绿，恰好是
            // 红绿色觉最难分辨的一对。所以 Waiting 在渲染层要做成**空心**，
            // 让它是一个形状差异而不只是颜色差异。
            AgentStatus::Waiting => Some(0xF59E0B),
            AgentStatus::Done => Some(0x22C55E),
        }
    }
}

/// 一次 agent 会话的可见状态。
///
/// # 与 tty7 的差异
///
/// tty7 的这个结构体还带 `launch_argv` / `rich` / `turns`，它们服务于
/// 「daemon 跨重启保留 pane 状态」和「未读角标跨视图保持」两件事。
/// deepterm 没有 daemon，重启后由进程检测重新得出结论，所以三者的语义都
/// 无处安放，这里一律不要。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionState {
    #[serde(default = "AgentSessionState::default_status")]
    pub status: AgentStatus,
    /// agent 说的话：等待授权的原因、提问的内容、回合结束的小结。
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// agent 自己的工作目录。Claude 起 worktree 时会和 shell 的 cwd 分叉，
    /// 所以单独记一份，不能拿 shell 的 cwd 代替。
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
}

impl AgentSessionState {
    fn default_status() -> AgentStatus {
        AgentStatus::Idle
    }

    /// 把一条事件并入状态机。
    ///
    /// 状态机的形状是「一次回合」：`PromptSubmit` 起手 → 可能中途 `Waiting`
    /// 等授权 → `ToolComplete` 回到 `Working` → `Stop` 收尾成 `Done`。
    pub fn apply_event(&mut self, ev: &AgentEvent) {
        if let Some(id) = &ev.session_id {
            self.session_id = Some(id.clone());
        }
        if let Some(cwd) = &ev.cwd {
            self.cwd = Some(cwd.clone());
        }
        match ev.kind {
            AgentEventKind::SessionStart => {
                self.status = AgentStatus::Idle;
                self.message = None;
            }
            AgentEventKind::PromptSubmit => {
                self.status = AgentStatus::Working;
                self.message = None;
            }
            AgentEventKind::PermissionRequest | AgentEventKind::QuestionAsked => {
                self.status = AgentStatus::Waiting;
                self.message = ev.message.clone();
            }
            AgentEventKind::Notification => {
                // 只在干活时降级为等待：`Stop` 之后 agent 补发的通知不该把
                // 已完成的回合又翻回 Waiting。
                if self.status == AgentStatus::Working {
                    self.status = AgentStatus::Waiting;
                    self.message = ev.message.clone();
                }
            }
            AgentEventKind::ToolComplete => {
                // 授权通过后工具继续跑，说明回合重新动起来了。
                if self.status == AgentStatus::Waiting {
                    self.status = AgentStatus::Working;
                    self.message = None;
                }
            }
            AgentEventKind::Stop => {
                self.status = AgentStatus::Done;
                self.message = ev.message.clone();
            }
            AgentEventKind::SessionEnd => {
                self.status = AgentStatus::Idle;
                self.message = None;
                self.cwd = None;
            }
        }
    }
}

/// agent hook 报告的事件种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEventKind {
    SessionStart,
    PromptSubmit,
    PermissionRequest,
    QuestionAsked,
    ToolComplete,
    Notification,
    Stop,
    SessionEnd,
}

impl AgentEventKind {
    /// 全部事件种类。桥接模板与上报脚本都是**字符串**，漏一个事件名或拼错
    /// 一个不会有编译错误，只会在用户机器上静默不工作；这个常量让那些模板
    /// 能被逐个断言（见 `hooks::tests`）。
    pub const ALL: [AgentEventKind; 8] = [
        AgentEventKind::SessionStart,
        AgentEventKind::PromptSubmit,
        AgentEventKind::PermissionRequest,
        AgentEventKind::QuestionAsked,
        AgentEventKind::ToolComplete,
        AgentEventKind::Notification,
        AgentEventKind::Stop,
        AgentEventKind::SessionEnd,
    ];

    /// hook 载荷里该写的事件名字符串。
    ///
    /// 必须与 serde 的 `rename_all = "kebab-case"` 逐字一致——这里手写一份是
    /// 为了让 hook 命令的构造方（`agent::hooks`）不必依赖序列化细节。**两者
    /// 不一致的失败模式是静默的**：`parse_agent_event` 认不出 `event` 值会
    /// 整个载荷返回 `None`，界面上只表现为「状态点永远不亮」。
    /// `wire_name_matches_serde` 测试锁住这条。
    pub fn wire_name(self) -> &'static str {
        match self {
            AgentEventKind::SessionStart => "session-start",
            AgentEventKind::PromptSubmit => "prompt-submit",
            AgentEventKind::PermissionRequest => "permission-request",
            AgentEventKind::QuestionAsked => "question-asked",
            AgentEventKind::ToolComplete => "tool-complete",
            AgentEventKind::Notification => "notification",
            AgentEventKind::Stop => "stop",
            AgentEventKind::SessionEnd => "session-end",
        }
    }
}

/// 一条 agent 事件，来自 hook。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEvent {
    /// hook 自己声明的 agent。进程检测失败时的兜底——事件里带着答案就用它。
    pub agent: Option<CLIAgent>,
    pub kind: AgentEventKind,
    pub session_id: Option<String>,
    /// agent 说的话（等待原因 / 提问内容 / 收尾小结）。
    pub message: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    /// 用户在 `PromptSubmit` 时敲的内容。
    ///
    /// 与 `message` 分开：`message` 是 agent 说的，且回合开始时会刻意清空；
    /// 这个是用户输入的，用来给 Tab 做「正在处理：修一下这个 bug」这类提示。
    pub prompt: Option<String>,
}

/// 解析一条 OSC 777 载荷，取出 agent 事件。
///
/// 期望形状：`777;notify;<sentinel>;<json>`。
/// 任何一步不匹配都返回 `None`——这条信道与用户的桌面通知共用，
/// 认不出来的一律交给通知处理，不能吞掉。
pub fn parse_agent_event(payload: &[u8]) -> Option<AgentEvent> {
    let rest = payload.strip_prefix(b"777;notify;")?;
    let rest = rest.strip_prefix(AGENT_EVENT_SENTINEL.as_bytes())?;
    let json = rest.strip_prefix(b";")?;

    #[derive(Deserialize)]
    struct Wire {
        /// 协议版本。当前只有 1，先只做存在性校验，不做分支。
        #[serde(default)]
        #[allow(dead_code)]
        v: u32,
        #[serde(default)]
        agent: Option<String>,
        event: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
    }

    let w: Wire = serde_json::from_slice(json).ok()?;
    let kind = serde_json::from_value::<AgentEventKind>(serde_json::Value::String(w.event)).ok()?;
    // 空串与缺失等价：hook 侧为可选字段填空串比省略 key 省事。
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    Some(AgentEvent {
        agent: w.agent.as_deref().and_then(CLIAgent::from_slug),
        kind,
        session_id: nonempty(w.session_id),
        message: nonempty(w.message),
        cwd: nonempty(w.cwd).map(std::path::PathBuf::from),
        prompt: nonempty(w.prompt),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn detect(parts: &[&str]) -> Option<CLIAgent> {
        CLIAgent::detect_from_argv_with(&argv(parts), &HashMap::new())
    }

    /// 每个 agent 的圆盘图形都必须在自己底色上看得清。
    ///
    /// 图形只有约 8.6px（16px 圆盘的 54%），对比度不足就糊成一团。这条守的是
    /// 「改了品牌色、忘了图形颜色」——黑压黑或白压白会让图标整个消失。
    /// 3:1 是 WCAG 对非文本图形的下限。
    #[test]
    fn every_agent_glyph_is_legible_on_its_disc() {
        let luminance = |hex: u32| {
            let linear = |v: f32| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            let [_, r, g, b] = hex.to_be_bytes().map(|b| b as f32 / 255.0);
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        let contrast = |a: u32, b: u32| {
            let (x, y) = (luminance(a), luminance(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };

        for agent in CLIAgent::ALL {
            let disc = agent.accent_rgb();
            let glyph = agent.icon_rgb();
            let ratio = contrast(disc, glyph);
            assert!(
                ratio >= 3.0,
                "{agent:?} 的图形 #{:06X} 在底色 #{:06X} 上对比度只有 {:.2}:1，看不清",
                glyph, disc, ratio
            );
        }
    }


    #[test]
    fn detects_native_binaries() {
        assert_eq!(detect(&["claude"]), Some(CLIAgent::Claude));
        assert_eq!(
            detect(&["/opt/homebrew/bin/codex", "--model", "o3"]),
            Some(CLIAgent::Codex)
        );
        assert_eq!(
            detect(&["/Users/me/.local/bin/traecli"]),
            Some(CLIAgent::TraeCode)
        );
        assert_eq!(detect(&["traex"]), Some(CLIAgent::TraeCode));
        assert_eq!(detect(&["/usr/local/bin/gemini"]), Some(CLIAgent::Gemini));
        assert_eq!(detect(&["cursor-agent"]), Some(CLIAgent::Cursor));
        // 末尾斜杠不该影响基名提取。
        assert_eq!(detect(&["claude/"]), Some(CLIAgent::Claude));
    }

    #[test]
    fn strips_leading_env_assignments() {
        assert_eq!(
            detect(&["FOO=1", "BAR=baz", "claude"]),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn detects_node_wrapped_claude_by_package_dir() {
        assert_eq!(
            detect(&[
                "node",
                "/Users/x/.npm/_npx/node_modules/@anthropic-ai/claude-code/cli.js",
            ]),
            Some(CLIAgent::Claude)
        );
    }

    /// npm 包装了两个二进制，文档让人跑第一个。两个都是 `#!/usr/bin/env node`
    /// 脚本，所以 pty 上看到的是 node 加 shim 的路径——`qodercli` 这个名字
    /// 出现在那个路径里，而转发器的子进程不是进程组组长，读不到。
    #[test]
    fn qoder_is_detected_through_either_of_its_binaries() {
        for launcher in [
            "qoder",
            "qodercli",
            "/opt/homebrew/bin/qoder",
            "/opt/homebrew/bin/qodercli",
        ] {
            assert_eq!(
                detect(&["node", launcher]),
                Some(CLIAgent::QoderCLI),
                "launcher {launcher}"
            );
        }
    }

    #[test]
    fn detects_npx_package_form() {
        assert_eq!(
            detect(&["npx", "-y", "@google/gemini-cli"]),
            Some(CLIAgent::Gemini)
        );
    }

    /// `python -m pkg.mod` 这种形式**不会**被识别：`base_stem` 只按路径分隔符
    /// 取基名，不做包名拆分，所以 token 是 `aider.main` 而非 `aider`。
    ///
    /// 这是有意的取舍——真正常见的 aider 启动方式是直接跑 `aider` 二进制，
    /// 而放宽到「按 `.` 再切一层」会让 `cli.js`、`foo.bar` 这类 token 大面积误判。
    #[test]
    fn module_style_invocations_are_not_guessed() {
        assert_eq!(detect(&["python3", "-m", "aider.main"]), None);
        // 直接跑二进制才是常规路径。
        assert_eq!(detect(&["python3", "/usr/bin/aider"]), Some(CLIAgent::Aider));
        assert_eq!(detect(&["aider"]), Some(CLIAgent::Aider));
    }

    /// 非解释器的参数不该被翻。否则 `grep node`、`cat claude.txt`
    /// 之类都会被误认成 agent。
    #[test]
    fn non_interpreter_does_not_match_on_arguments() {
        assert_eq!(detect(&["grep", "claude"]), None);
        assert_eq!(detect(&["cat", "/tmp/claude"]), None);
    }

    #[test]
    fn unrelated_commands_are_none() {
        assert_eq!(detect(&["ls"]), None);
        assert_eq!(detect(&["vim", "claude.md"]), None);
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn every_agent_has_metadata() {
        for agent in CLIAgent::ALL {
            assert!(!agent.slug().is_empty(), "{agent:?} 缺 slug");
            assert!(!agent.display_name().is_empty(), "{agent:?} 缺显示名");
            assert!(!agent.icon_path().is_empty(), "{agent:?} 缺图标");
            assert!(!agent.aliases().is_empty(), "{agent:?} 缺别名");
            assert_eq!(
                CLIAgent::from_slug(agent.slug()),
                Some(agent),
                "{agent:?} 的 slug 不能反查"
            );
        }
    }

    /// 品牌色接近纯黑的 agent，靠一道细边才不至于和深色主题背景糊在一起。
    /// 这个测试锁住「哪些是黑的」这个事实，渲染层的描边逻辑依赖它。
    #[test]
    fn black_branded_avatars_keep_their_brand_field() {
        // 官方标志本身就是单色的：OpenAI 的 favicon 用 currentColor、
        // x.ai 的 icon 是纯灰度、Qoder 官网 logo 是 #0F0D0C、
        // OpenCode 的 app-icon 底色是 #131010、TRAE 的 favicon 底色是 #0A0B0D。
        for agent in [
            CLIAgent::Codex,
            CLIAgent::Grok,
            CLIAgent::QoderCLI,
            CLIAgent::OpenCode,
            CLIAgent::Kimi,
            CLIAgent::Auggie,
        ] {
            assert!(
                luminance(agent.accent_rgb()) < 0x20,
                "{agent:?} 的底色 {:#08X} 不再是近黑，描边判定要跟着改",
                agent.accent_rgb()
            );
        }
        // TRAE 是近黑而非纯黑（官方 favicon 底色 #0A0B0D）。
        assert_eq!(CLIAgent::TraeCode.accent_rgb(), 0x0A0B0D);
    }

    /// 图形色必须在自己底色上看得清——判据是 `icon_rgb` 按底色亮度自动反相。
    ///
    /// 这是 `icon_rgb` 不能用「直接返回 accent」的理由，也是最容易在改色时被顺手
    /// 删掉的一处，所以单独钉住：图形默认白，底色偏亮时翻黑。方向错了则是功能故障
    /// （黑压黑 / 白压白，图标整个消失）。
    #[test]
    fn black_and_white_brands_invert_their_glyph_colour() {
        // 深色底：图形要够亮。
        for agent in [
            CLIAgent::Codex,
            CLIAgent::Grok,
            CLIAgent::QoderCLI,
            CLIAgent::OpenCode,
            CLIAgent::Kimi,
            CLIAgent::Auggie,
        ] {
            let glyph = agent.icon_rgb();
            assert_ne!(glyph, agent.accent_rgb(), "{agent:?} 的图形色没反相");
            assert!(
                luminance(glyph) > 0x80,
                "{agent:?} 的图形色 {glyph:#08X} 太暗，深色底上会隐形"
            );
        }

        // 亮底：图形要够暗（Goose 的官方绿 #3ECC5F 白字仅 2.1:1）。
        for agent in [CLIAgent::Goose, CLIAgent::Pi, CLIAgent::Aider, CLIAgent::Cursor] {
            let glyph = agent.icon_rgb();
            assert_ne!(glyph, agent.accent_rgb(), "{agent:?} 的图形色没反相");
            assert!(
                luminance(glyph) < 0x80,
                "{agent:?} 的图形色 {glyph:#08X} 太亮，亮底上会隐形"
            );
        }
    }

    /// 粗糙的亮度近似（0..=255），只用来判断「亮底还是暗底」，不做色彩管理。
    fn luminance(rgb: u32) -> u32 {
        let r = (rgb >> 16) & 0xFF;
        let g = (rgb >> 8) & 0xFF;
        let b = rgb & 0xFF;
        (r * 299 + g * 587 + b * 114) / 1000
    }

    /// 白底或近白底的 agent 与浅色主题背景糊在一起，描边判定同样依赖这个事实。
    ///
    /// 官方色核验后**没有**任何 agent 的品牌色是纯白：Qoder 的官方 logo 是
    /// 近黑 `#0F0D0C`（此前误用了纯白），其余白色系品牌（Cursor 的 `#EDECEC` 等）
    /// 也都改成了各自官方 logo 的实际颜色。这条钉住这个事实，避免以后又引入
    /// 一个和浅色背景同色的品牌色。
    #[test]
    fn white_branded_avatars_are_known() {
        let white: Vec<CLIAgent> = CLIAgent::ALL
            .into_iter()
            .filter(|a| luminance(a.accent_rgb()) > 0xF0)
            .collect();
        assert!(
            white.is_empty(),
            "品牌色不应是纯白（会与浅色主题背景同色）：{white:?}"
        );
    }

    /// 只有没有自有图形的 agent 才回退到通用机器人图标——
    /// 否则新增 agent 时很容易忘了加图标，然后所有人长得一样。
    #[test]
    fn only_the_unbranded_agents_use_the_fallback_glyph() {
        let expected = [
            CLIAgent::Aider,
            CLIAgent::Auggie,
            CLIAgent::Hermes,
            CLIAgent::Vibe,
            CLIAgent::Antigravity,
        ];
        for agent in CLIAgent::ALL {
            let fallback = agent.icon_path() == "icons/bot.svg";
            assert_eq!(
                fallback,
                expected.contains(&agent),
                "{agent:?} 的图标回退判定与预期不符"
            );
        }
    }

    /// 每个 `icon_path()` 都必须真的能取到资源。
    ///
    /// 路径是字符串拼出来的，编译器不看；漏拷一个 SVG 只会在运行时表现成
    /// 一个空白方块。tty7 的 `icons/bot.svg` 就一直只被引用、从未随包发布。
    #[test]
    fn every_icon_path_resolves_to_an_embedded_asset() {
        use gpui::AssetSource as _;

        for agent in CLIAgent::ALL {
            let path = agent.icon_path();
            let loaded = crate::assets::Assets.load(path);
            assert!(
                matches!(&loaded, Ok(Some(bytes)) if !bytes.is_empty()),
                "{agent:?} 的图标 {path} 取不到：{loaded:?}"
            );
        }
    }

    #[test]
    fn oh_my_pi_and_pi_stay_distinct() {
        assert_eq!(detect(&["pi"]), Some(CLIAgent::Pi));
        assert_eq!(detect(&["omp"]), Some(CLIAgent::OhMyPi));
    }

    #[test]
    fn custom_rules_map_wrappers_to_agents() {
        let mut custom = HashMap::new();
        custom.insert("my-claude".to_string(), "claude".to_string());
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["my-claude", "--foo"]), &custom),
            Some(CLIAgent::Claude)
        );
        // 自定义规则不该劫持已知的 agent 名。
        custom.insert("claude".to_string(), "codex".to_string());
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["claude"]), &custom),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn detects_from_typed_command_lines() {
        let none = HashMap::new();
        assert_eq!(
            CLIAgent::detect_from_command_with("claude --resume abc", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("claude.exe", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(
                r"C:\Users\x\AppData\Roaming\npm\claude.cmd --model opus",
                &none
            ),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("CLAUDE", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(r#"& "C:\tools\codex.exe""#, &none),
            Some(CLIAgent::Codex)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(
                r"node C:\x\node_modules\@anthropic-ai\claude-code\cli.js",
                &none
            ),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("npx.cmd @google/gemini-cli", &none),
            Some(CLIAgent::Gemini)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("notepad claude.txt", &none),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("cat codex.md", &none),
            None
        );
        assert_eq!(CLIAgent::detect_from_command_with("", &none), None);
        let custom: HashMap<String, String> = [("cc".to_string(), "claude".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_command_with("cc -c", &custom),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn command_argv_preserves_case_for_flag_replay() {
        assert_eq!(
            command_argv("claude --model Opus"),
            vec!["claude".to_string(), "--model".to_string(), "Opus".to_string()]
        );
        // shell 集成会把后台命令记成 `& cmd`，前导的 & 不是命令的一部分。
        assert_eq!(command_argv("& claude"), vec!["claude".to_string()]);
    }

    /// Python 自带的彩蛋：`python3 -m antigravity`。
    /// 它不是 agent，绝不能因为名字撞车被认出来。
    #[test]
    fn the_python_easter_egg_is_not_a_coding_agent() {
        assert_eq!(detect(&["python3", "-m", "antigravity"]), None);
        assert_eq!(detect(&["/usr/bin/python3", "-m", "antigravity"]), None);
        // 真正的 Antigravity CLI 是 `agy`。
        assert_eq!(detect(&["agy"]), Some(CLIAgent::Antigravity));
    }

    #[test]
    fn parses_sentinel_events() {
        let payload = br#"777;notify;deepterm://cli-agent;{"v":1,"agent":"claude","event":"prompt-submit","session_id":"abc-123","cwd":"/repo","prompt":"fix the bug"}"#;
        let ev = parse_agent_event(payload).expect("应解析出事件");
        assert_eq!(ev.agent, Some(CLIAgent::Claude));
        assert_eq!(ev.kind, AgentEventKind::PromptSubmit);
        assert_eq!(ev.session_id.as_deref(), Some("abc-123"));
        assert_eq!(ev.cwd.as_deref(), Some(std::path::Path::new("/repo")));
        assert_eq!(ev.prompt.as_deref(), Some("fix the bug"));

        // 未知 agent slug 不致命：事件本身仍然有效，只是没有 agent 归属。
        let ev = parse_agent_event(
            br#"777;notify;deepterm://cli-agent;{"event":"stop","agent":"nope"}"#,
        )
        .expect("未知 agent 不该让整条事件作废");
        assert_eq!(ev.agent, None);
        assert_eq!(ev.kind, AgentEventKind::Stop);
    }

    /// 这条信道与用户的桌面通知共用，认不出来的一律要交还给通知处理。
    #[test]
    fn non_agent_payloads_are_rejected() {
        assert_eq!(parse_agent_event(b"777;notify;Build;done"), None);
        assert_eq!(parse_agent_event(b"9;hello"), None);
        assert_eq!(
            parse_agent_event(br#"777;notify;deepterm://cli-agent;{"event":"bogus"}"#),
            None,
            "未知事件名要整条拒绝，不能猜"
        );
        assert_eq!(parse_agent_event(b"777;notify;deepterm://cli-agent;not json"), None);
    }

    #[test]
    fn empty_optional_fields_read_as_absent() {
        let ev = parse_agent_event(
            br#"777;notify;deepterm://cli-agent;{"event":"stop","session_id":"","message":"  "}"#,
        )
        .expect("应解析出事件");
        assert_eq!(ev.session_id, None);
        assert_eq!(ev.message, None);
    }

    #[test]
    fn session_state_machine_follows_the_turn() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        assert_eq!(s.status, AgentStatus::Idle);

        s.apply_event(&ev(AgentEventKind::SessionStart));
        assert_eq!(s.status, AgentStatus::Idle);

        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        assert_eq!(s.status, AgentStatus::Working);

        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(s.status, AgentStatus::Working, "工具跑完还在干活");

        s.apply_event(&ev(AgentEventKind::Stop));
        assert_eq!(s.status, AgentStatus::Done);

        s.apply_event(&ev(AgentEventKind::SessionEnd));
        assert_eq!(s.status, AgentStatus::Idle);
    }

    #[test]
    fn permission_request_parks_the_turn_in_waiting() {
        let ev = |kind, msg: Option<&str>| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: msg.map(String::from),
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit, None));
        s.apply_event(&ev(
            AgentEventKind::PermissionRequest,
            Some("Bash(rm -rf /tmp/x)"),
        ));
        assert_eq!(s.status, AgentStatus::Waiting);
        assert_eq!(s.message.as_deref(), Some("Bash(rm -rf /tmp/x)"));

        // 授权通过、工具继续跑 → 回到 Working，等待文案清掉。
        s.apply_event(&ev(AgentEventKind::ToolComplete, None));
        assert_eq!(s.status, AgentStatus::Working);
        assert_eq!(s.message, None);
    }

    /// `Stop` 之后 agent 补发的通知不该把已完成的回合翻回 Waiting——
    /// 那会让 Tab 一直停在「需要你」的状态上。
    #[test]
    fn a_notification_after_stop_does_not_reopen_the_turn() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: Some("noise".into()),
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        s.apply_event(&ev(AgentEventKind::Stop));
        assert_eq!(s.status, AgentStatus::Done);

        s.apply_event(&ev(AgentEventKind::Notification));
        assert_eq!(s.status, AgentStatus::Done, "完成态不该被通知改写");
    }

    #[test]
    fn a_notification_while_working_asks_for_attention() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Codex),
            kind,
            session_id: None,
            message: Some("需要确认".into()),
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        s.apply_event(&ev(AgentEventKind::Notification));
        assert_eq!(s.status, AgentStatus::Waiting);
        assert_eq!(s.message.as_deref(), Some("需要确认"));
    }

    /// `ToolComplete` 在状态不变时**不得**改变 session。
    ///
    /// 这条是 `AgentChanged` 风暴的护栏。`apply_agent_events` 用整份 session
    /// 判断"变没变"并据此发事件；若 `ToolComplete` 里塞了任何自增字段
    /// （曾经有个 `activity` 计数），一个回合里几十次工具调用就会每次都发
    /// `AgentChanged`，订阅方跟着白重绘、白转发。
    #[test]
    fn tool_completions_do_not_change_session_when_status_holds_still() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        let after_prompt = s.clone();

        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(
            s, after_prompt,
            "Working 状态下 ToolComplete 不该让 session 有任何变化"
        );

        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(s, after_prompt, "第二次 ToolComplete 同理");

        // 但 Waiting → ToolComplete 是**真的**状态变化，必须能看出来。
        s.apply_event(&ev(AgentEventKind::PermissionRequest));
        let waiting = s.clone();
        assert_ne!(waiting, after_prompt);
        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(s.status, AgentStatus::Working, "授权通过后回到 Working");
        assert_ne!(s, waiting, "Waiting → Working 是真实变化");
    }

    #[test]
    fn session_state_tracks_and_releases_the_agent_cwd() {
        use std::path::PathBuf;

        let ev = |kind, cwd: Option<&str>| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: cwd.map(PathBuf::from),
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::SessionStart, Some("/repo")));
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/repo")));

        // Claude 起 worktree 后，agent 的 cwd 与 shell 的 cwd 分叉。
        s.apply_event(&ev(
            AgentEventKind::ToolComplete,
            Some("/repo/.claude/worktrees/fix-x"),
        ));
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/fix-x"))
        );

        // 回合结束不释放——用户可能还要回去看那个 worktree。
        s.apply_event(&ev(AgentEventKind::Stop, None));
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/fix-x"))
        );

        s.apply_event(&ev(AgentEventKind::SessionEnd, None));
        assert_eq!(s.cwd, None, "会话结束才释放 cwd");
    }

    #[test]
    fn session_id_is_sticky_and_tracks_the_latest() {
        let ev = |id: Option<&str>| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind: AgentEventKind::ToolComplete,
            session_id: id.map(String::from),
            message: None,
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(Some("first")));
        assert_eq!(s.session_id.as_deref(), Some("first"));

        // 没有 id 的事件不该把已有的清掉。
        s.apply_event(&ev(None));
        assert_eq!(s.session_id.as_deref(), Some("first"));

        s.apply_event(&ev(Some("second")));
        assert_eq!(s.session_id.as_deref(), Some("second"));
    }

    #[test]
    fn status_metadata_is_consistent() {
        assert_eq!(AgentStatus::Idle.dot_rgb(), None, "Idle 不画点");
        for status in [
            AgentStatus::Working,
            AgentStatus::Waiting,
            AgentStatus::Done,
        ] {
            assert!(status.dot_rgb().is_some(), "{status:?} 应有状态点颜色");
        }
        // Waiting 与 Done 是「要不要过去看一眼」的那一对，必须颜色可分辨。
        assert_ne!(
            AgentStatus::Waiting.dot_rgb(),
            AgentStatus::Done.dot_rgb()
        );
        assert_ne!(
            AgentStatus::Working.dot_rgb(),
            AgentStatus::Done.dot_rgb()
        );
    }

    #[test]
    fn status_and_event_kind_round_trip_through_serde() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Waiting,
            AgentStatus::Done,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
        for kind in [
            AgentEventKind::SessionStart,
            AgentEventKind::PromptSubmit,
            AgentEventKind::PermissionRequest,
            AgentEventKind::QuestionAsked,
            AgentEventKind::ToolComplete,
            AgentEventKind::Notification,
            AgentEventKind::Stop,
            AgentEventKind::SessionEnd,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: AgentEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    /// `wire_name` 必须与 serde 的 kebab-case 序列化逐字一致。
    ///
    /// 这条守的是一类**静默**失败：hook 载荷里写错事件名时
    /// `parse_agent_event` 会整个返回 `None`（`serde_json::from_value` 对未知
    /// 字符串失败），界面上只是「状态点永远不亮」，没有任何报错。手写一份
    /// `wire_name` 是为了让 `agent::hooks` 不依赖序列化细节，代价就是必须
    /// 用测试把两份绑定住。
    #[test]
    fn wire_name_matches_serde() {
        for kind in [
            AgentEventKind::SessionStart,
            AgentEventKind::PromptSubmit,
            AgentEventKind::PermissionRequest,
            AgentEventKind::QuestionAsked,
            AgentEventKind::ToolComplete,
            AgentEventKind::Notification,
            AgentEventKind::Stop,
            AgentEventKind::SessionEnd,
        ] {
            let serde_name = serde_json::to_string(&kind).unwrap();
            // serde 给的是带引号的 JSON 字符串，剥掉引号再比。
            let serde_name = serde_name.trim_matches('"');
            assert_eq!(
                kind.wire_name(),
                serde_name,
                "{kind:?} 的 wire_name 与 serde 序列化不一致——hook 载荷会被解析器拒绝"
            );
        }
    }
}
