# ki 使用手册

这是一份面向使用者和扩展作者的 ki 手册。ki 是基于 Pi agent 设计的 Rust 原生 coding-agent：核心能力以 crate 形式提供，`ki` 是开箱即用的 CLI。本文记录当前版本已经实现的命令和行为，随 `kiqi-cli` 一起发布。

## 1. 安装

### 直接安装 CLI

需要 Rust/Cargo：

```bash
cargo install kiqi-cli
ki --version
ki --help
```

从本仓库源码安装开发版本：

```bash
git clone https://github.com/gqf2008/pi-rust.git
cd pi-rust
cargo install --path crates/kiqi-cli --force
```

`cargo install` 安装的是可执行文件；它不等同于 `ki install`。后者用于安装由 `kiqi-plugin-sdk` 构建的 Rust 动态库扩展。

### 从源码启动与构建

本项目是 Rust Cargo workspace，不是 Node/npm 项目，根目录没有 `package.json`。在仓库根目录执行：

```bash
# 直接启动 CLI
cargo run -p kiqi-cli

# 向 CLI 传递参数（`--` 后的内容属于 ki）
cargo run -p kiqi-cli -- -p "总结 README.md"
cargo run -p kiqi-cli -- --mode json -p "列出项目中的 crates"

# 构建 release 版本
cargo build -p kiqi-cli --release
```

构建完成后，Windows 可执行文件位于 `target/release/ki.exe`，Linux/macOS 下通常为 `target/release/ki`。也可以把本地 CLI 安装到 Cargo 的 bin 目录后直接使用：

```bash
cargo install --path crates/kiqi-cli --force
ki
```

离线示例（不需要 API Key 或网络）可以使用：

```bash
cargo run -p minimal
```

### 第一次运行

交互模式：

```bash
ki
ki "解释当前项目的目录结构"
```

单次输出模式：

```bash
ki -p "总结 README.md"
ki --mode json -p "列出项目的风险"
```

通过 `@` 把文件放进第一条消息：

```bash
ki @README.md "给出这个项目的改进建议"
```

## 2. 模型和凭据

### 凭据优先级

当多个来源同时存在时，优先级为：

1. `--api-key`
2. `~/.ki/auth.json`（由 `ki auth login` 写入）
3. `~/.ki/agent/models.json` 中对应 provider 的 `apiKey`
4. 环境变量

Anthropic 使用 `ANTHROPIC_API_KEY` 或 `ANTHROPIC_AUTH_TOKEN`；OpenAI-compatible provider 使用 `OPENAI_API_KEY`。检查凭据时不会发起网络请求：

```bash
ki auth check
ki auth check --provider gateway --json
ki auth login
ki auth logout
```

不要把密钥写入 Git 仓库或项目文档。

### 自定义 Provider

配置文件默认是 `~/.ki/agent/models.json`，也可以通过 `KI_CODING_AGENT_DIR` 指定 agent 配置目录：

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "models": [
        { "id": "custom-claude", "name": "Custom Claude" }
      ]
    },
    "openai-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://api.example.com/v1",
      "apiKey": "sk-openai-secret",
      "models": [
        { "id": "custom-model", "name": "Custom Model" }
      ]
    }
  }
}
```

选择模型：

```bash
ki --model gateway/custom-claude -p "hello"
ki --model openai-gateway/custom-model -p "hello"
ki --provider gateway --model custom-claude -p "hello"
```

不传 `--model` 时，ki 会优先选择已经认证的模型。标准 Anthropic 凭据存在时使用内置默认模型；只有自定义 gateway 已认证时，会选择该 gateway 的第一个可用模型。

Anthropic 的兼容网关还可以使用：

```text
ANTHROPIC_BASE_URL=https://gateway.example.com
ANTHROPIC_AUTH_TOKEN=token
```

## 3. CLI 命令

常用全局选项：

```text
--provider <name>       Provider 名称
--model <pattern>       模型 ID，支持 provider/model 和 thinking 后缀
--api-key <key>         本次运行覆盖凭据
--base-url <url>        覆盖模型 endpoint
--timeout <seconds>     LLM API 请求超时（默认 600）
--thinking <level>      off/minimal/low/medium/high/xhigh/max
--print, -p             单次运行并退出
--mode text|json|rpc    输出模式
--continue, -c          继续最近会话
--resume, -r            选择历史会话
--session <id|path>     指定会话
--session-dir <dir>     指定会话目录（相对路径按进程 cwd 解析；这与 KI_CODING_AGENT_DIR 要求绝对路径不同）
--name, -n <name>       设置会话显示名称
--tools <list>          只允许指定工具
--exclude-tools <list>  禁用指定工具
--no-tools              禁用所有工具
--no-builtin-tools      禁用内置工具（read/bash/edit/write/docs）
--no-skills             跳过技能发现
--no-prompt-templates   跳过 prompt-template 发现
--no-context-files      跳过 AGENTS.md/CLAUDE.md 发现
--no-extensions         禁用扩展加载
--extensions-dir <dir>  额外扫描 Rust 扩展目录
--package-only         只加载当前项目 package/resource，排除全局资源
--project-only         `--package-only` 的等价别名
--list-models [search]  列出可用模型（可带模糊搜索）
--offline               禁用启动时的网络检查
--export <session.jsonl>  把会话导出为 HTML（参数是**输入**文件；输出路径取第一个位置参数，默认写到当前目录）
--tui-mode <mode>       TUI buffer：regular 或 fullscreen
--system-prompt <text>  替换默认系统提示词
--append-system-prompt <text>  追加系统提示词（可重复）
--debug-system-prompt   把解析后的系统提示词各段打印到 stderr
--verbose               显示启动警告（如被忽略的 flag）
--server [--port <n>] [--bind <ip>]
                        无头服务端（启动时打印 token）
--connect <host:port>   远程客户端 TUI（零本地资源）
--token <token>         远程认证 token（也支持环境变量 KI_SERVER_TOKEN）
```

子命令：

```text
ki auth login|check|logout
ki events path|tail    检查扩展事件日志（需 KI_EVENT_LOG=1）
ki install <crate>
ki uninstall <crate>
ki update                 # 更新 ki CLI 自身
ki dev [options]          # 开发 Rust 扩展：编译、watch、热重载
ki dev-local [options]    # 只调试当前 Rust 扩展（隔离模式）
ki package link|list|unlink # 管理项目本地 Rust 扩展包
```

### 启动资源清单

启动 TUI 时，`Context` 会列出实际加载成功的系统提示词和项目指令文件路径，
包括 `SYSTEM.md`、`APPEND_SYSTEM.md` 以及 `AGENTS.md` / `CLAUDE.md`。
项目 `SYSTEM.md` 和 `APPEND_SYSTEM.md` 优先于全局同名文件；清单只显示生效的来源。
没有加载文件时，启动页会省略 `Context`。`/context` 可查看当前完整资源清单，
成功执行 `/reload` 后，启动页的 `Context` 和 `/context` 都会更新。
设置 `quietStartup: true` 可隐藏启动资源清单。

### 更新日志

`/changelog` 显示当前二进制对应的最新发布说明；`/changelog all` 显示完整历史。
更新后第一次启动新交互会话，会展示自上次阅读以来的新版本说明，后续启动不再重复。
恢复已有会话时，提示会延后到下一次新会话。

ki 使用全局 `settings.json` 中的 `lastKiqiChangelogVersion` 记录已读版本，避免导入的
`lastChangelogVersion` 干扰。已有配置缺少 ki 标记时，只补充展示当前版本一次；首次安装
直接建立基线。设置 `collapseChangelog: true` 可显示简短更新提示。

### 跳到最新消息

在 `--tui-mode fullscreen` 和 `--connect` 中，向上滚动回看历史时，消息区底部会显示
`↓ Jump to latest message · End`。点击提示或按 `End` 即可回到最新消息，并恢复跟随输出；
输入框中的草稿和焦点会保留。

可在 `settings.json` 的 `keybindings` 中设置 `"tui.altScreen.bottom": "ctrl+end"`。
提示会显示实际快捷键；设置为 `null` 可禁用按键，点击入口仍可使用。

### 远程模式（`--server` / `--connect`）

把 agent 跑在无头服务端，另一个进程用 TUI 远程驱动。客户端**无任何本地 agent 资源**。

```bash
ki --server --port 9899          # 服务端：启动时打印 token
ki --connect 127.0.0.1:9899 --token <token>   # 客户端

# 或通过环境变量传递 token
export KI_SERVER_TOKEN=<token>
ki --connect 127.0.0.1:9899
```

服务端默认开启 token 认证（`--no-token` 可关闭）。客户端支持 `/state`、`/model`、
`/thinking`、`/tools`、`/abort` 等命令；`/tree`、`/fork`、`/switch`、`/export`、
`/name`、`/reload` 依赖本地 harness，在远程模式下不可用。完整协议、可用命令与
限制见 [`remote-mode.md`](../remote/user-guide.md)。

### 3.1 退出码（脚本依赖这个）

| 码 | 含义 |
|---|---|
| `0` | 运行正常结束 |
| `1` | 运行时失败：模型解析/会话构建失败、run 报告 `Error` 或 `Aborted`、**或 run 撞到单轮上限**（`KI_MAX_TURNS_PER_RUN`，`ki -p` 场景下即节点的 `maxTurns`） |
| `2` | 用法错误（参数无法解析） |
| `3` | 启动被扩展否决（某个 `BeforeTuiStart` 处理器返回 `EVENT_HANDLER_ABORT`） |

`1` 里“**撞到单轮上限**”这一条容易被忽略但很重要：被截断的 run 仍然走正常出口，若只看
stdout 会以为任务完成（输出可能是半截的）。把上限当成失败，是为了让调用方（脚本、
`subagent`/`dispatch` 编排）能分辨“做完了”和“被截断了”。交互模式下同样会追加一条
`runBudget` 消息说明原因，所以人也能看到。

## 4. 内置工具

默认工具包含 Pi 的 `read`、`bash`、`edit`、`write`，以及 ki 自带的只读 `docs` 文档查询工具。Windows 上还会默认注册 `powershell`。`docs` 可以查询使用手册、扩展开发、Agent 项目结构、Rust 调试和架构说明；`grep`、`find`、`ls` 仍保留为库实现，但不由 CLI 默认注册。

可以通过 `.ki/settings.json` 的 `defaultTools` 字段（或简写 `default_tools`）在未指定
`--tools` 时限定启动工具集：

```json
{
  "defaultTools": ["read", "bash", "edit", "write", "docs"]
}
```

需要限制工具范围时，显式列出 Pi 的四个工具：

```bash
ki --tools read,bash,edit,write,docs -p "检查并修改项目文件"
```

## 5. 项目目录和资源优先级

ki 使用自己的 `.ki` 目录：

```text
项目/
└── .ki/
    ├── settings.json
    ├── SYSTEM.md
    ├── APPEND_SYSTEM.md
    ├── skills/
    ├── prompts/
    ├── themes/
    └── extensions/
```

全局资源默认位于 `~/.ki/agent/`，包括 `settings.json`、`models.json`、`skills/`、`prompts/`、`themes/` 和 `extensions/`。同名资源发生冲突时，项目资源优先于全局资源。

### 5.1 跨工具共享目录 `.agents`

`.agents` 是一个跨工具约定（Claude Code、skills 生态都用它），**不是** ki 自己的配置目录。ki 会把它一起读进来，因为你可能已经在用它了：

| 位置 | 内容 | 优先级 |
|---|---|---|
| `<项目>/.ki/skills` | 项目技能 | 最高（同名胜出） |
| `<项目>/.agents/skills` | 项目技能（共享约定） | 次之 |
| 配置的全局 `skillDirs` | 自定义全局技能 | 再次 |
| `~/.ki/agent/skills` | ki 全局技能 | 再次 |
| `~/.agents/skills` | 用户级共享技能 | 最低 |

上下文文件的加载顺序同理（越通用越靠前、越具体越靠后）：

```text
~/.agents/AGENTS.md  →  ~/.ki/agent/AGENTS.md  →  <祖先目录>/AGENTS.md  →  <项目>/AGENTS.md
```

把常驻指令放在 `~/.agents/AGENTS.md`，它对所有遵循该约定的工具都生效；只针对 ki 的规则放 `~/.ki/agent/AGENTS.md`。两个文件都不存在时静默跳过，不报错。

几点需要注意：

- 技能采用**渐进式加载**：系统提示里只放名称、描述和文件位置，完整 `SKILL.md` 在模型判断任务匹配时才读取。
  但**列表本身不是免费的**：它包含每条技能的完整描述，而描述正是模型路由的依据，所以不能省。
  实测 64 个技能（多数描述较长、含中文）的 `<available_skills>` 段约 **39 KB / 约 7–9k tokens**，
  且这笔开销**每次请求都会付一次**。技能少的项目可以忽略，攒到几十个就值得留意。
- 想在不删除的前提下把某个技能从列表里拿掉：在它的 frontmatter 写 `disable-model-invocation: true`。
  技能仍会被加载、仍可显式调用，只是不进系统提示（实测：`2 loaded (1 model-visible, 1 hidden)`）。
- `~/.agents` 只用来放 `AGENTS.md` 与 `skills/`；ki 不会从那里找 prompts、themes 或 extensions。
- 项目 `.agents/skills` 与项目 `.ki/skills` 走**同一套**项目级规则：默认加载；当项目被标记为不可信（或传 `--no-approve`）时，项目级目录整体不读，只保留全局。v1 没有交互式信任提示，缺省即信任。

### 5.2 项目级资源目录

项目级 `.ki/settings.json` 可以追加资源目录：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"]
}
```

路径相对于项目根目录；`skills`、`prompts`、`extensions` 是对应 `*Dirs` 字段的简写。自定义目录会与 `.ki` 和全局约定目录一起加载。全局 `~/.ki/agent/settings.json` 也支持这些字段，相对路径相对于 agent 目录。

只加载当前项目的 package/resource、排除全局资源时，使用以下任一等价参数：

```bash
ki --package-only
ki --project-only
```

两者都会加载当前项目的 skills、prompts、themes、extensions 和 packages，并读取项目 `.ki/settings.json`；不会自动加载全局资源。`--local-only` 语义不同，只加载通过 `--extensions-dir` 或 `--extension` 显式指定的扩展。

配置目录可以重定位：

```bash
KI_CODING_AGENT_DIR=/work/kiqi-agent ki
```

会话默认保存到 agent 配置目录下的 sessions；`--no-session` 可使用临时会话。

跨工具的共享目录（`~/.agents`）也可以指定，用 `KI_AGENTS_DIR`：

```bash
KI_AGENTS_DIR=/work/shared-agents ki
```

两点行为需要知道：

- **`KI_CODING_AGENT_DIR` 一旦设置，就不再回退读真实的 `~/.agents`。** 重定位 ki 配置目录是「用这套布局、不用用户那套」的声明，若仍去读家目录下的共享指令，两套环境的常驻规则会被混在一起。想让重定位的运行也用共享目录，显式设 `KI_AGENTS_DIR`。
- `KI_AGENTS_DIR` 必须是绝对路径；相对路径会被忽略（相对路径会随 cwd 变化，不能用来确定「读哪个目录当常驻指令」）。

## 6. Rust 插件

Rust 原生插件是 `cdylib`，只依赖 `kiqi-plugin-sdk`，由 `kiqi-extensions` 动态加载：

```bash
cargo build -p plugin-stub
ki --extensions-dir target/debug -p 'echo "hi"'
```

从 crates.io 安装：

```bash
ki install kiqi-extension-example
ki install kiqi-extension-example --version 0.1.0
ki install kiqi-extension-example --force
```

本地开发：

```bash
ki install my-extension --path ../my-kiqi-extension --force
```

开发扩展项目时使用 watch 模式：

```bash
cd ../my-kiqi-extension
ki dev
# 多扩展 workspace：ki dev --package my-extension
# 只调试当前扩展：ki dev-local（隔离模式，不加载全局扩展）
```

`ki dev` 会自动识别 Cargo `cdylib`、首次编译并从 `.ki/extensions/.dev` 加载版本化产物。源码变化会触发重新编译和热重载；手工执行 `/reload` 也会先重新编译。编译失败时继续保留当前已经加载的版本。

如果希望把开发好的扩展固定到某个项目，可使用项目包命令：

```bash
ki package link --path ../my-extension --package my-extension
ki package list
# 之后在该项目目录直接运行 ki 即可加载
ki package unlink my-extension
```

项目包位于 `.ki/packages/<name>/`，是受信任的本机 Rust 动态库；它不是 npm/Node Pi package。全局安装仍使用 `ki install`。完整模板和边界规则见在线扩展作者指南：<https://ki.laofu.online/extension-authoring.md>`，调试技巧见 `docs` 的 `debugging` 主题。

安装后的动态库位于 `~/.ki/agent/extensions`（或 `KI_CODING_AGENT_DIR` 指定的目录），下次启动 ki 时加载。插件通过稳定 ABI 注册工具、Provider、事件处理器和资源处理器；不要直接依赖 `kiqi-cli` 的私有模块。

## 7. SDK 集成

只需要嵌入 Agent 时，依赖 `kiqi-agent` 和 `kiqi-ai`；需要内置工具时再加入 `kiqi-tools`；需要会话、压缩、skills 和上下文文件时加入 `kiqi-harness`。

```rust
use kiqi_agent::{Agent, AgentEvent};
use kiqi_ai::providers::faux::faux_provider;

let model = faux_provider().model("echo");
let mut agent = Agent::builder(model)
    .system_prompt("You are a helpful assistant.")
    .build();

let mut events = agent.subscribe();
agent.prompt("Hello from ki").await?;
while let Some(event) = events.recv().await {
    if matches!(event, AgentEvent::AgentEnd { .. }) {
        break;
    }
}
```

自定义工具实现 `kiqi-agent::AgentTool`；Provider 实现 `kiqi-ai::Provider`。使用 `faux provider` 和 `InMemoryExecutionEnv` 可以在没有网络、API key 或真实文件系统的情况下测试 Agent loop。

## 8. 故障排查

### 找不到模型或接口报错

运行 `ki auth check`，确认 provider、`models.json`、环境变量和 `baseUrl`。使用 `--provider` 和完整的 `provider/model` 避免同名模型歧义。接口错误会保留 provider 返回的诊断文本；先检查 endpoint、认证头和模型 ID。

### 扩展加载失败

确认 Rust 插件是 `cdylib` 并与当前平台匹配；临时使用 `--extensions-dir` 指向生成目录。加载失败时保留宿主输出的完整错误信息，检查扩展是否声明了宿主未提供的能力。

### 扩展事件处理器没触发或启动被中止

```bash
export KI_EVENT_LOG=1
ki            # 记录所有扩展事件处理器调用
ki events tail   # 跟踪日志；看 result 是 Continue 还是 Error/Abort/Panic/Timeout
```

日志默认在 `~/.ki/logs/events.jsonl`（可用 `KI_EVENT_LOG_PATH` 覆盖）。如果
`BeforeTuiStart` 阶段有扩展返回 veto（`EVENT_HANDLER_ABORT`），CLI 以退出码 `3`
中止启动并打印原因。

### 资源没有出现在欢迎页

确认文件位于 `.ki`（或兼容的 `.pi`）或全局 `~/.ki/agent`。使用 `ki --debug-system-prompt` 查看最终系统提示词、skills 和资源计数；同名资源优先检查 `.ki` 是否覆盖了 `.pi`。

### 远程模式连不上

确认服务端已用 `ki --server --port <port>` 启动且 token 正确（或通过
`KI_SERVER_TOKEN` 提供）。客户端是纯显示端，不加载任何本地 provider/工具/扩展；
会话文件在服务端。

## 9. 开发、测试和发布

运行格式化、检查和测试：

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
```

发布 crate 时按依赖顺序：

```text
kiqi-telemetry → kiqi-ai → kiqi-agent → kiqi-tools → kiqi-harness →
kiqi-plugin-sdk → kiqi-extensions → kiqi-tui → kiqi-cli
```

发布前更新版本号和 `docs/release-vX.Y.Z.md`，确认 README、官网 `website/data/docs.json` 和本手册中的命令一致。官网是静态站点，文档文件提交到仓库后仍需按项目部署流程重新部署。

## 10. 文档查询入口

- 在线文档：<https://ki.laofu.online/docs.html>
- 源码仓库：<https://github.com/gqf2008/pi-rust>
- Rust API：<https://docs.rs/kiqi-agent>、<https://docs.rs/kiqi-plugin-sdk>

- Package 与扩展作者指南：<https://ki.laofu.online/extension-authoring.md>
- 远程模式：<docs/remote/user-guide.md>
- Rust 扩展与 agent 调试：`ki` 内 `docs` 工具的 `debugging` 主题

当在线文档和已安装版本不一致时，以对应版本的 Git tag 和仓库内文档为准。
