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

`cargo install` 安装的是可执行文件，与扩展安装无关。扩展一律是 wasm 组件，用 `ki insmod` 安装（2026-10-26 起 cdylib 扩展通道已退役，`ki install`/`ki dev`/`ki package` 已删除）。

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
ki update                 # 更新 ki CLI 自身
ki insmod <path.wasm>     # 安装 wasm 扩展（授权走 grants；--build <guest-crate-dir>
                          # 先编译；见 `ki extension --help`）
ki rmmod <id>             # 卸载 wasm 扩展
ki extension list|show|grant|revoke|rename
                          # 管理扩展：查看/授权/改名（grants 与存储凭据随目录走）
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
松开后停在放下的位置；普通拖动是文本选择、松开自动复制，覆盖**整个屏幕**——回显区、
状态行、输入框都能选，复制的是屏幕上实际显示的字符（`regular` 模式用终端自身的原生选择）。
盒子之间允许重叠，拖到汇总盒子上面会盖住它。Esc 中断后盒子随 run 结束消失。

transcript 里的工具块只保留工具自己的文本：子代理进度（`{"ki":"node",…}` 行）只以
上面的盒子呈现，不再重复一遍原始 JSON。

`subagent` 是**后台**的：调用立刻返回（结果里只有 `Started background subagent#N`），
子代理在自己的进程里跑，多个调用真正并行，每个都有自己的盒子。它结束时结果会作为一条
follow-up 用户消息回到会话里——空闲就起一轮，正在跑就排队（显示为 `Steering: …`）。
调用 `subagent` 之后不要轮询，继续干别的或结束本轮即可。`/agents` 列出正在跑的后台
子代理，`/agents kill <id|all>` 停止它们。

`plan` + `dispatch` 仍然是阻塞的 wave：一个调用内部并行跑子进程，调用返回时整波结束；
并行批里的多个 `dispatch` 会因为同一个扩展一次只执行一个调用而排队（汇总盒子里以
`queued · waiting for a free extension call` 列出）。

盒子只在 `--tui-mode fullscreen`（默认）下存在：`regular` 模式没有合成层，改为在回显
区底部的工作块里给每个子代理列一行（`id · 状态 用时 · 任务名`）。

### MCP 服务器（`/mcp-reload` / `/mcp-doctor`）

MCP 服务器从 `settings.json` 的 `mcpServers` 读取（全局一份，受信任项目可以再覆盖一层；
**项目没被信任时它的 `mcpServers` 会被忽略**并给出提示）。启动时连接一次，工具以
`{server}__{tool}` 的形式出现，资源更新通过通知 mailbox 送达。

一个 stdio 条目的完整形状：

```json
{
  "mcpServers": {
    "walgit": {
      "command": "/path/to/walgit",
      "args": ["mcp", "--repo", "/path/to/repo"],
      "cwd": "/path/to/repo",
      "env": { "WALGIT_LOG": "info" },
      "enabled": true,
      "timeoutSeconds": 60,
      "subscribe": ["walgit://collab/board/gqf2008/abb"],
      "autoRead": ["walgit://collab/board/gqf2008/abb"],
      "signals": { "wake": "auto", "minIntervalSeconds": 60 },
      "modelWatch": true
    }
  }
}
```

HTTP 服务器把 `command`/`args`/`env`/`cwd` 换成 `url` + 可选 `headers`。键名是 camelCase
（`timeoutSeconds`、`autoRead`、`minIntervalSeconds`…）；**拼错的键不会静默生效**：解析时
忽略它（不连累整个条目），但会在启动告警里打一条、并写进 `/mcp-doctor` 的 `config:` 行，
附最近似的正确拼写（`auto_read` → `autoRead`）。以前这里是静默忽略，看起来和写对了一模一样。

stdout/stderr 里出现的转义序列不会进终端：`/mcp-doctor` 会剥掉 CSI/OSC 等序列和控制字符、
把 Tab 折成空格、按字符截断，所以一个乱打印控制字节的服务器最多让报告难看，不会清屏、切走
备用屏幕或改窗口标题。这条保证不止于 doctor：**渲染器在把每一行写进终端之前都会过滤**，
只放过 SGR 样式（数字参数）、OSC-8 链接、OSC 133 提示区、内联图片序列和 ki 自己的 APC 标记，
而且序列必须正常收尾（无 BEL/ST 的 OSC、夹带 `ESC` 的 APC 一律丢弃，否则终端会在裸 `ESC`
处中断它、转头执行后面紧跟的 `\x1b[2J`）；工具输出、模型回复、扩展消息、MCP 载荷、粘贴内容
里的 `\x1b[2J`、光标移动、模式切换、OSC 52 写剪贴板都在这里被丢掉。`ki -p` 的输出和
`ki extension` 打印的扩展声明 / 授权说明同样按纯文本写出。

**`subscribe`**：握手后为每条 uri 调一次 `resources/subscribe`。写不进去的（服务器拒绝、
或它根本没声明 `resources.subscribe` 能力）只报一条 warning，不会拖垮整个服务器。订阅
来自配置的**在 `/mcp-reload` 后仍然有效**；模型自己用 `{server}__watch_resource` 建的
订阅属于本次会话，reload 后消失，且最多同时 32 条（`MAX_MODEL_SUBSCRIPTIONS`）。

**`autoRead`**：只对**已订阅**的 uri 有意义。它是在通知被并入会话的那一刻（会话边界，
一轮结束之后），把该 uri 的**当前全文**读出来一并写进通知里，这样模型醒来时不必先花一次
工具往返去读。每次边界、每个 uri 至多读一次，共享一个超时；单条内容截断到 20000 字符
（`MAX_RESOURCE_TEXT`）。所以它读的**不是「变更的增量」，也不便宜**——默认什么都不配
（只送指针，内容由模型自己决定要不要读）。

**`signals`**：资源通知允许对会话做什么。`wake` 三档——`off`（只记进 transcript，不唤醒）、
`notify`（记 + 呈现给客户端）、`auto`（记 + 在预算内用一条合成触发消息起一轮，醒来后做
什么由模型决定）。优先级：服务器条目自己的 `signals` > 项目顶层 `signals` > 全局顶层
`signals` > 由 `subscribe` 推导的默认值（**配了 `subscribe` 就默认 `auto`，没配就 `off`**）。
预算字段 `minIntervalSeconds`（1–3600，默认 30）、`maxRunsPerHour`（1–60，默认 6）、
`maxConsecutiveRuns`（1–20，默认 2）自动夹取。

改了配置不用重启 `ki`：`/mcp-reload`（别名 `/mcp`）重新读取配置、**原地重连**所有服务器
（manager 与 mailbox 不变），再用新连接重建工具表（走的是和 `/reload` 同一条重建流程，
所以扩展资源也会被重新读取；wasm 扩展实例本身不重建，它们的运行状态保留）。它会逐个
列出连不上的服务器及原因，并显示重连后当前可用的 MCP 工具数量。

出问题时跑 `/mcp-doctor`：**只读**（不重连、不发 RPC），报告上一次启动/重连留下的状态，
所以症状出现之后再跑也能看到现场。每个服务器一块：`status`（`connected` / `disabled
(enabled=false)` / `failed` + 原因）、工具清单、`resources` 能力、`subscribe` 配置了几条 /
实际活了几条（没配置过就不出现这一项）、`autoRead`、`signals` 预算、被忽略的未知键、stdio
服务器最后的 stderr，以及该怎么做（例如配置了但没订阅上、进程根本没起来）。配置改了但还没
reload 时，它会按**名字**指出哪些服务器没在这个会话里启动、哪些已不在配置里。

它只有 TUI 入口：没有 `ki mcp` 子命令，无头模式（`ki -p`）只打启动告警。服务器声明了
`resources` 但没实现 `resources/templates/list`（`-32601`）时，那份模板清单按空处理，
资源清单照常列出，只报一条“未实现”的 warning。

Windows 上两个已知坑（`/mcp-doctor` 会提示）：stdio 服务器**不经 shell 启动**，所以
`npx`/`uvx`/`pnpm` 这类 `.cmd` shim 要写成 `command: "cmd"`、`args: ["/d", "/s", "/c", …]`；
子进程只继承 `PATH`/`HOME`/`TMPDIR`/`TERM`/`USER`/`SHELL`/`LANG`/`LC_*`（外加 ki 自己注入的
`AGENT_ID_ENV`）——服务器需要的其它变量（`APPDATA`、`LOCALAPPDATA`、`PATHEXT`…）必须写进它
的 `env`。

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
    ├── cron.json
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

### 5.3 定时任务（`.ki/cron.json`）

会话工作目录下的 `.ki/cron.json` 可以让 ki 在**会话打开期间**按时间表把任务交给 agent。
它是项目资源，**只有当前项目受信任时才会读取和调度**；`--no-approve` 会同时禁用 cron 与其他
项目资源。`prompt` 和 `command` 都是项目内容，使用同一个信任判定，并非只限制会执行命令的一支。
会话中执行 `/trust no` 撤销信任后，调度器会停止后续触发；`/reload` 也会先重新判定信任，再决定
是否重建调度器。

一个任务要么是 `prompt`（原文注入为一条用户消息），要么是 `command`（在会话工作目录执行，
把合并后的 stdout+stderr 交给 agent），二者恰有其一：

```json
{
  "jobs": [
    {
      "name": "daily-digest",
      "schedule": "@daily",
      "prompt": "总结今天的 git 变更，并按优先级列出待办"
    },
    {
      "name": "git-status",
      "schedule": "*/30 * * * *",
      "command": "git status --short"
    }
  ]
}
```

顶层也可以直接是任务数组（裸数组）。字段：

| 字段 | 必填 | 含义 |
|---|---|---|
| `name` | 是 | 任务名，非空且唯一；注入文本和日志用它标识任务 |
| `schedule` | 是 | 时间表，见下 |
| `prompt` | 二选一 | 原文作为用户消息注入 |
| `command` | 二选一 | 在会话工作目录执行，输出交给 agent |
| `enabled` | 否 | 默认 `true`；`false` 只加载不触发 |
| `timeoutSecs` | 否 | 命令超时秒数，默认 60，上限 600 |

未知字段会报错（该任务被丢弃并指出字段名），其余合法任务照常运行。`.ki/cron.json`
是普通 JSON，不接受注释。

`schedule` 是 5 字段 cron（**本地时间**）：`分 时 日 月 星期`，支持 `*`、数字、
`a-b`、逗号列表、`*/n` 与 `a-b/n`；另有 `@hourly`、`@daily`、`@weekly`、`@monthly`。
星期 `0` 与 `7` 都表示周日；日和星期都受限时按 Vixie 语义取“或”。

运行时语义：

- **只在会话里跳动。** 交互式会话启动时若发现 `.ki/cron.json` 才开始调度；无头模式
  （`-p`、`--mode json`、`--server`）不调度。进程退出或会话结束后任务不再运行。
- **错过不补跑。** 会话关闭期间到点的任务直接跳过，不会在下次启动时补发。
- **每分钟每任务最多一次。** 同一进程内，同一个（任务，分钟）最多触发一次。配置
  文件按 mtime 热重载，删除文件即停止全部任务；`/reload` 会重建调度器。
- **命令不并发。** 同名命令上一轮仍在运行时，本次触发跳过并记日志。
- **交付方式像用户输入。** 任务文本以 `⏰ cron:<name> 到点触发` 或
  `⏰ cron:<name> 命令输出（退出码 N）` 为标记注入；若正有一轮在跑，它进入队列
  （与用户此刻输入相同，不取消、不打断当前回合），空闲时直接开始新一轮。
- **命令限制。** 用平台 shell（Unix `sh -c`、Windows `cmd /C`）执行，stdout+stderr
  合并；超过 `timeoutSecs` 会强杀进程组；输出保留前 64 KiB，超出部分截断并附
  显式标记；空输出显示为 `(无输出)`。

与系统 crontab 的区别：任务只在 ki 会话内运行，没有后台常驻进程；错过的触发不会
补跑；同一命令不会重叠执行。诊断（配置错误、注入失败、超时、并发跳过）写入
`<agent_dir>/logs/ki.log`。

## 6. wasm 扩展

扩展一律是 wasm 组件（`wasm32-wasip2`），实现 `extensions/kiqi-extension.wit` 的
`kiqi:extension@0.9.0` 契约，由宿主用 wasmtime 沙箱加载；能力按
`$KI_AGENT_DIR/extensions/<id>/grants.json` 授予，缺失 = 全部拒绝。cdylib 通道
（`kiqi-extension-sdk` / `ki install` / `ki dev` / `ki package` / `libloading`）
已于 2026-10-26 退役。

安装与卸载：

```bash
ki insmod --build extensions/wasm/kiqi-todo   # 编译并安装（id 取自包名）
ki rmmod kiqi-todo                            # 卸载（目录移入 .removed-*）
```

查看与授权：

```bash
ki extension list                # 已安装扩展与已授权能力
ki extension show kiqi-todo      # 单个扩展的声明与 grants
ki extension grant kiqi-todo fs --cwd project
ki extension revoke kiqi-todo net-http
ki extension rename old-id new-id   # 改名（grants 与存储凭据随迁）
```

模型侧是渐进式加载：系统提示只带 name/description/location，正文由模型按需通过
`skill` 工具或 `read` 加载；`disableModelInvocation: true` 的扩展只允许用户显式
触发。事件是观察性的（`Continue`/`Claimed`/`Patch`），每个会话使用自己的派发
caps。调试与契约细节见 `docs` 的 `debugging` 与 `extension-abi` 主题。

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

### 扩展未加载或能力未生效

扩展一律是 wasm 组件：`ki insmod --build extensions/wasm/<crate> --id <id>` 安装，
能力由 `$KI_AGENT_DIR/extensions/<id>/grants.json` 控制（缺失 = 全部拒绝）。用
`ki extension show <id>` 查看声明与授权；`/reload` 后用 `ki --debug-system-prompt`
确认资源与计数。事件日志通道已于 2026-10-27 随 C-ABI 通道退役删除。

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

全部 crate 均为 `publish = false`，不发布到 crates.io；安装走本仓库的
GitHub Releases + `install.sh`。

发布前更新版本号和 `docs/release-vX.Y.Z.md`，确认 README 与本手册中的命令一致。文档随仓库和对应 Git tag 一起发布。

## 10. 文档查询入口

- 用户指南：[`docs/guides/user-guide.md`](user-guide.md)
- 源码仓库：<https://github.com/gqf2008/pi-rust>
- Rust API：crates 均为 `publish = false`，无 docs.rs 页面；API 文档以仓库内源码与 crate README 为准

- Package 与扩展作者指南：[`docs/extensions/authoring.md`](docs/extensions/authoring.md)
- 远程模式：<docs/remote/user-guide.md>
- Rust 扩展与 agent 调试：`ki` 内 `docs` 工具的 `debugging` 主题

当文档和已安装版本不一致时，以对应版本的 Git tag 和仓库内文档为准。
