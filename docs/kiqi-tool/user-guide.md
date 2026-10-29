# ki 使用手册

这是一份面向使用者和扩展作者的 ki 手册。ki 是基于 Pi agent 设计的 Rust 原生 coding-agent：核心能力以 crate 形式提供，`ki` 是开箱即用的 CLI。本文记录当前版本已经实现的命令和行为，随 `kiqi-cli` 一起发布。

## 1. 安装

### 安装 CLI

安装 CLI 需要 Rust 1.96+。这些 crate 没有发布到 crates.io（每个 manifest 都是
`publish = false`），所以 `cargo install kiqi-cli` 装不到任何东西。从本仓库源码安装：

```bash
git clone https://github.com/gqf2008/pi-rust.git
cd pi-rust
cargo install --path crates/kiqi-cli --force
ki --version
ki --help
```

`cargo install` 安装的是可执行文件。曾经的 `ki install`（安装 `kiqi-extension-sdk` 构建的 Rust 动态库扩展）已随 cdylib 通道于 2026-10-26 退役；现在插件只有 wasm 一条通道，用 `ki insmod` 安装。

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

不要把密钥写入 Git 仓库、项目文档或 package manifest。

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

不传 `--model` 时，ki 优先使用已保存的模型选择；没有保存选择时，按 `models.json` 的声明顺序选择第一个已认证的自定义模型，再回退到已认证的内置默认模型。只配置一个自定义 provider、模型和凭据即可直接使用，不需要再设置环境变量或 `defaultProvider/defaultModel`。`contextWindow` 和 `maxTokens` 保持可选，未配置即不设置：OpenAI 系协议不发送对应参数；Anthropic 协议的 `max_tokens` 为必填字段，未配置时由 ki 代填默认值。

`input` 仅作为 models.json 的兼容元数据保留，ki 不根据它过滤文本、图片或工具图片；多模态请求是否兼容由 provider 决定并返回错误。配置向导不再询问图片支持，模型列表也不显示 images 能力列。

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
--session-dir <dir>     指定会话目录（相对路径按进程 cwd 解析，不像 KI_CODING_AGENT_DIR 那样要求绝对路径）
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
--tui-mode <mode>       TUI buffer：fullscreen（默认）或 regular（原生滚动回看）
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
ki update                 # 更新 ki CLI 自身
ki insmod <path.wasm>     # 安装 wasm 插件（--build 先编译 guest）
ki rmmod <id>             # 卸载 wasm 插件
```

### TUI 历史消息滚动

交互式 TUI 默认使用 alternate screen，并由应用自己管理 transcript 滚动。可以使用 `PageUp` / `PageDown`、`Home` / `End`，或鼠标和触控板滚轮浏览历史消息。

macOS 的行为与 main-screen 的终端原生 scrollback 不同：alternate-screen TUI 必须接收鼠标追踪事件，滚轮和触控板才能滚动历史消息。进入 raw mode 后，TUI 会在 alternate-screen 模式显式重新启用鼠标追踪；main-screen 模式则继续关闭鼠标追踪，以保留终端原生滚动和文本选择。

如果 macOS 上滚轮无法滚动历史消息，应检查 `TuiAltScreen::start_readerless` 是否包含 alternate-screen 的鼠标追踪初始化，不要直接修改 macOS 的全局默认值。

### 输入历史（↑ / ↓）

输入框为空（或已经在历史里翻）时，`↑` 依次取出之前提交过的消息，`↓` 往回走；翻回最新一条
后恢复你当时正在写的草稿。输入框里有内容时 `↑`/`↓` 仍然是光标移动，不会拿历史覆盖草稿。

历史跨会话保留：提交的每条消息（含多行输入）追加到 `<agent_dir>/input-history.jsonl`，
下次启动 `ki` 直接可用，保留最近 100 条，连续重复的只留一条。删除这个文件即清空记忆。

### 工作视图（`/work`）

全屏 TUI 里 `/work` 打开或关闭工作视图，默认打开。它不替换 transcript，而是在回显区
右上角叠加两类浮层：

- 一个汇总盒子：`todo` 列表、正在运行的工具、LLM 回合与 token 计数（流式时显示
  `llm streaming <秒>s · <字符数> chars`，并带上上一轮的耗时与输出 token）。
- 每个子代理一个独立盒子：任务名（`task`）、状态与已用时间/预算进度条、当前工具、
  最近事件（`▶ bash` / `✔ bash` / `✖ bash`）。没有子代理时只有汇总盒子；两者都没有
  可显示的内容时整个视图不占屏。

盒子默认靠右对齐、从上到下依次排列（汇总盒子在最上面，子代理盒子按出现顺序排在它
下面），整块铺主题的 `surface` 底色——终端没有 alpha 通道，这层底色就是「毛玻璃」：
内容被盖住，但盒子明确浮在回显区之上。**`Alt`（或 `Ctrl`）按住盒子正文拖动**即可移动，
松开后停在放下的位置；普通拖动始终是文本选择，松开自动复制，鼠标捕获不会把选择吃掉。
盒子之间允许重叠，拖到汇总盒子上面会盖住它。Esc 中断后盒子随 run 结束消失。

transcript 里的工具块只保留工具自己的文本：子代理进度（`{"ki":"node",…}` 行）只以
上面的盒子呈现，不再重复一遍原始 JSON。

一个 wasm 扩展同一时刻只执行一个调用，所以并行批里的多个 `subagent` 会依次进入
guest：正在跑的有自己的盒子，还在排队的在汇总盒子里以
`queued · waiting for a free extension call` 列出。需要真正并行时用 `plan` + `dispatch`。

盒子只在 `--tui-mode fullscreen`（默认）下存在：`regular` 模式没有合成层，改为在回显
区底部的工作块里给每个子代理列一行（`id · 状态 用时 · 任务名`）。

### macOS 键盘与插件面板

支持 Kitty 键盘协议的终端使用与 Pi 一致的 flags 7，启用修饰键识别及功能键重复、松键事件。普通文本保留 UTF-8 输入，保证 iTerm2 等终端的中文输入法可以正常提交文字；不启用把所有文本转成按键事件的 flag 8。普通字符键（例如空格）的松键事件不作保证，依赖按住操作的插件应使用终端支持松键事件的功能键。协议在切换到当前屏幕后开启，并在退出或挂起前恢复；恢复运行时也会重新启用全屏鼠标滚动。

本地 Terminal.app 的 `Shift+Enter` 使用 macOS 原生 Shift 状态补足换行识别，普通 `Enter` 继续发送。SSH 会话不会读取本机 Shift 状态。未支持增强键盘协议的终端仍使用传统输入，按住快捷键等依赖松键事件的功能需要终端支持。

插件输入框、编辑器、选择器和历史搜索打开时优先接收输入，插件全局快捷键不会抢走这些窗口的按键。已被插件接管的按键仍会收到对应的松键事件，避免按住状态卡住。浮层按实际宽度排版，并避开输入框和页脚；多个被动面板在空间足够时自动纵向避让，窗口过小时暂时隐藏放不下的面板。需要避免浮层遮住聊天正文时可关闭面板，或使用插件的 sidebar 布局。

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
限制见 [`remote-mode.md`](remote-mode.md)。

### 3.1 退出码

| 码 | 含义 |
|---|---|
| `0` | 正常结束 |
| `1` | 运行时失败（模型/会话构建失败、`Error`/`Aborted`），**或 run 撞到单轮上限**（`KI_MAX_TURNS_PER_RUN`，`ki -p` 下即 `maxTurns`） |
| `2` | 用法错误 |
| `3` | 启动被扩展否决（`BeforeTuiStart` 返回 `EVENT_HANDLER_ABORT`） |

被单轮上限截断的 run 走的是正常出口，只读 stdout 会以为任务完成（输出可能是半截的）；
把上限记为 `1` 才能让脚本/编排分辨“做完”与“被截断”。交互模式同时追加 `runBudget` 消息说明原因。

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

ki 会优先使用 ki 自己的目录，同时兼容原 Pi 的 `.pi` 布局：

```text
项目/
├── .ki/
│   ├── settings.json
│   ├── SYSTEM.md
│   ├── APPEND_SYSTEM.md
│   ├── skills/
│   ├── prompts/
│   ├── themes/
│   ├── extensions/
│   └── packages/
└── themes/
```

项目 `.ki` 优先于项目 `.pi`。全局资源默认位于 `~/.ki/agent/`，包括 `settings.json`、`models.json`、`skills/`、`prompts/`、`themes/`、`extensions/` 和 `packages/`。同名资源发生冲突时，项目资源优先于全局资源，`.ki` 优先于 `.pi`。

`.agents` 是一个跨工具共享约定（Claude Code、skills 生态都用它），ki 也会读取它：

| 技能目录 | 说明 |
|---|---|
| `<项目>/.ki/skills` | 项目技能（最高优先级） |
| `<项目>/.agents/skills` | 项目技能（共享约定） |
| 配置的全局 `skillDirs` | 自定义全局技能 |
| `~/.ki/agent/skills` | ki 全局技能 |
| `~/.agents/skills` | 用户级共享技能（最低优先级） |

上下文文件按「越通用越靠前」排列：`~/.agents/AGENTS.md` → `~/.ki/agent/AGENTS.md` → 祖先目录 → `<项目>/AGENTS.md`。全局、通用的常驻指令放 `~/.agents/AGENTS.md`，只针对 ki 的放 `~/.ki/agent/AGENTS.md`；文件不存在时静默跳过。

技能是**渐进式加载**的：系统提示只放名称、描述和位置，完整 `SKILL.md` 在任务匹配时才读取。但列表本身有成本——描述无法省（模型靠它路由），实测 64 个技能的 `<available_skills>` 段约 39 KB / 7–9k tokens，**每次请求都要付一次**。想保留技能但不让某条进提示，给它写 `disable-model-invocation: true`。

`~/.agents` 只提供 `AGENTS.md` 与 `skills/`，ki 不从那里找 prompts、themes 或 extensions。项目 `.agents/skills` 与 `.ki/skills` 同规则：默认加载；项目被标记不可信或传 `--no-approve` 时项目级目录整体不读。

项目级 `.ki/settings.json` 可以追加资源目录：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

路径相对于项目根目录；`skills`、`prompts`、`extensions` 是对应 `*Dirs` 字段的简写。自定义目录会与 `.ki`、`.pi` 和全局约定目录一起加载，`.ki` 优先。全局 `~/.ki/agent/settings.json` 也支持这些字段，相对路径相对于 agent 目录。

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

共享目录 `~/.agents` 用 `KI_AGENTS_DIR` 指定（须绝对路径，相对路径被忽略）：

```bash
KI_AGENTS_DIR=/work/shared-agents ki
```

注意：设了 `KI_CODING_AGENT_DIR` 后就不再回退读真实的 `~/.agents`——重定位配置目录意味着「用这套布局、不用用户那套」。想让重定位的运行也用共享目录，显式设 `KI_AGENTS_DIR`。

## 6. 插件（wasm）

**cdylib 通道已于 2026-10-26 退役**：`ki install` / `ki uninstall` / `ki dev` / `ki dev-local` 与 `kiqi-extension-sdk` 均已删除，`--extension` / `--extensions-dir` 仅为兼容而接受、不再加载任何东西。现在唯一的插件通道是 wasm 组件（契约 `extensions/kiqi-extension.wit`），跑在 wasmtime 沙箱里，按授权文件 `$KI_AGENT_DIR/extensions/<id>/grants.json` 拿能力：

```bash
# 从仓库内 guest 源码构建并安装
ki insmod --build extensions/wasm/kiqi-todo
# 安装已构建的组件
ki insmod path/to/component.wasm --yes
# 查看/卸载
ki extension list
ki rmmod <id>
```

能力契约与能力模型见 `docs/design/extension-abi.md`。改写旧 cdylib 扩展请以 `extensions/wasm/kiqi-todo` 为模板。

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

确认插件目录下有 `extension.wasm` 与 `grants.json`（`ki insmod` 会写好两者），并用 `ki extension list` 查看宿主报错。加载失败时保留宿主输出的完整错误信息，检查授权是否覆盖插件声明的能力（缺失 = 什么都不授予）。

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
kiqi-telemetry → kiqi-ai → kiqi-agent → kiqi-extension → kiqi-extensions →
kiqi-tools → kiqi-harness → kiqi-tui → kiqi-cli
```

发布前更新版本号和 `docs/release-vX.Y.Z.md`，确认 README 与本手册中的命令一致。文档随仓库和对应 Git tag 一起发布。

## 10. 文档查询入口

- 用户指南：[`docs/guides/user-guide.md`](user-guide.md)
- 源码仓库：<https://github.com/gqf2008/pi-rust>
- Rust API：<https://docs.rs/kiqi-agent>

- Package 与扩展作者指南：[`docs/extensions/authoring.md`](docs/extensions/authoring.md)
- 远程模式：<docs/remote/user-guide.md>
- Rust 扩展与 agent 调试：`ki` 内 `docs` 工具的 `debugging` 主题

当文档和已安装版本不一致时，以对应版本的 Git tag 和仓库内文档为准。
