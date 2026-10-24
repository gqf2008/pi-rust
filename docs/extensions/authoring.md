# ki Rust 扩展开发最佳实践

本文是 ki 创建 Rust `cdylib` 扩展的模型可读规范。扩展是 Rust 原生动态库，
通过稳定 C ABI（`kiqi-plugin-sdk`）注册工具、事件处理器、资源发现、Provider
与渲染器，宿主（`ki` CLI）负责加载与生命周期。创建完整 Agent 项目的默认结构见
`docs` 的 `agent` 主题；本指南聚焦扩展本身的 ABI 与生命周期。

## 1. 扩展模型总览

| 项目 | Rust 原生扩展 |
| --- | --- |
| 安装 | `ki install crate-name`（crates.io 或 `--path`） |
| 入口 | `kiqi_plugin_register` C ABI 符号 |
| 开发反馈 | `ki dev` watch + 热重载；`ki dev-local` 隔离调试 |
| 静态资源 | `resources_discover` 或随项目放入 `.ki`（wasm 插件改用 `resources.json` 包约定，见 §12.4） |
| 运行环境 | 本机动态库，当前用户权限 |
| 适用场景 | 本地工具、系统集成、性能敏感或纯 Rust 项目 |

扩展应遵循能力检测：只调用宿主明确提供的 capability，检查 `PluginApi`
对应注册槽位是否为 `Some`。缺少能力时返回清晰错误并优雅降级，不要 panic、
空指针或依赖 `undefined` 语义。

## 2. 推荐结构

```text
my-kiqi-extension/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs
│   ├── tool.rs
│   └── error.rs
└── tests/
    └── behavior.rs
```

### Cargo.toml

```toml
[package]
name = "my-kiqi-extension"
version = "0.1.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/example/my-kiqi-extension"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
kiqi-plugin-sdk = "0.1"
serde_json = "1"
```

`cdylib` 是宿主加载的产物；同时保留 `rlib` 便于 Rust 集成测试。扩展边界只依赖
`kiqi-plugin-sdk`，不要依赖 `kiqi-cli`、`kiqi-extensions` 或 harness 内部模块，
否则插件会和宿主实现耦合。

## 3. ABI 入口

宿主只加载统一 C ABI 符号 `kiqi_plugin_register`。扩展使用 `export_plugin!`，
通过同一个 `PluginApi` 注册能力并调用 `api.declare` 声明优先级/平台。
SDK 在注册前检查 `abi_version` 和 `struct_size`；不兼容时返回非零状态，宿主报告加载失败。

```rust
kiqi_plugin_sdk::export_plugin!(|api| {
    if let Some(declare) = api.declare {
        let status = declare(kiqi_plugin_sdk::StbStringRef::from_str(
            r#"{"priority":60,"platforms":["linux"]}"#,
        ));
        if status != 0 {
            return status;
        }
    }
    let Some(register_tool) = api.register_tool else {
        return -1;
    };
    // 构建 StableToolSchema 和 execute/poll/cancel/destroy 函数表，
    // 然后调用 register_tool。完整实现参考 examples/plugin-stub。
    let _ = register_tool;
    0
});
```

`declare` 的 JSON 支持 `priority`（数字，越大越先）和 `platforms`（字符串数组，
如 `["linux","windows","macos"]`）；宿主在分发事件处理器时跳过不匹配的平台。

### 能力检测

每个注册槽位都是 `Option`。注册前检查 vtable slot 是否为 `Some`，缺失时返回
清晰错误；不要依赖空指针或 panic 表达不支持。宿主对未知 runtime action ID
返回结构化错误，不会进入分发。runtime action 使用 SDK 的 `RuntimeActionId`
定义（含 `GetCliFlag`、`UiDialog`），不要自行假定数字范围。

## 4. 工具生命周期：execute → poll → cancel → destroy

工具导出四个 `extern "C"` 函数，宿主按固定协议驱动：

- `execute` 解析宿主传入的 JSON（owned `StbString`，用 `free_with` 释放），
  创建每次调用独立的 handle（`Box::into_raw`）。
- `poll` 必须非阻塞；未完成返回 `Pending`，完成返回 `Done` 或 `Err`。
  `partial_cb` 在 poll 内同步调用。
- `cancel` 只设置线程安全取消标记，必须幂等，不能释放 handle。
- `destroy` 由宿主最终调用一次并释放 handle（空指针 no-op）。

所有权协议：

- 宿主产生的 `StbString`（execute 的 params、事件 payload）由插件通过宿主的
  `free_string` 释放。
- 插件产生的 `StbString`（done 结果、partial 进度、resources_discover 的 out）
  由宿主通过插件注册时提供的 `plugin_free_string` 释放。
- 每个 `extern "C"` 边界都不能 unwind；插件内部捕获 panic 并转换为错误结果。

完整、可运行的 ABI 模板位于 `examples/plugin-stub/src/lib.rs`。创建新工具时先
复制生命周期骨架，再替换参数 schema 和领域逻辑，不要重新设计所有权协议。

### 注入到工具参数里的宿主上下文

宿主在把参数交给插件**之前**，会往这次调用的 JSON 参数里加一个保留键
`__ki`（`kiqi_plugin_sdk::HOST_CONTEXT_KEY`）：

```json
{ "action": "add", "text": "…",
  "__ki": { "cwd": "D:\\Projects\\pi-rust",
             "sessionId": "01adb026-7230-708b-855b-91eab6b056b8" } }
```

- **模型看不到、也写不了。** 注入发生在模型参数解析之后、`execute` 之前，不
  会进 `ToolExecutionStart`/`Update` 事件，也不会进会话日志；模型若碰巧传了同
  名键，宿主的值覆盖它。
- **两个字段都可能是缺失的。** 拿不到工作目录时没有 `cwd`；ephemeral 会话以及
  早于该注入的旧宿主没有 `sessionId`；整个键也可能不存在。
- **所以 tool 的参数类型必须容忍未知的顶层字段。** 用 `serde_json::Value`
  取值天然满足；给参数定义 `#[serde(deny_unknown_fields)]` 的结构体会直接失败。
- 内置工具不走这条路——它们从 harness 直接拿到 `ExecutionToolContext`。

需要"每个会话一份状态"的插件就用 `sessionId` 做键（`kiqi-todo` 存
`.ki/todos/<sessionId>.json`，一条会话一个文件），取不到时退回项目级并**在
回复里说明**，不要编一个共享的伪会话——那正是它要解决的问题。

## 5. 事件处理器、资源发现与 Provider

### 事件处理器

通过 `api.register_event_handler` 按 `EventTag`（36 类）订阅：

- **会话生命周期**：`SessionStart`、`SessionBeforeSwitch`、`SessionBeforeFork`、
  `SessionBeforeCompact`、`SessionCompact`、`SessionShutdown` 等。
- **Agent 循环**：`BeforeAgentStart`、`AgentStart`、`AgentEnd`、`AgentSettled`、
  `TurnStart`、`TurnEnd`、`MessageStart`/`MessageUpdate`/`MessageEnd`、
  `ToolExecutionStart`/`Update`/`End`、`ToolCall`、`ToolResult`、`UserBash`、`Input`。
- **ki 特有 / UI**：`BeforeTuiStart`（TUI 初始化前）、`UiPromptStart`/`UiPromptEnd`。

处理器收到 `StablePluginEvent`，不得释放事件里的字符串（宿主 fan-out 后统一释放）。
返回 `0` 成功；返回 `EVENT_HANDLER_ABORT` 表示 veto。

### 生命周期 veto

`BeforeTuiStart` 是带 veto 语义的 ki 专有钩子：第一个返回 `EVENT_HANDLER_ABORT`
的处理器中止启动，CLI 以退出码 `3` 退出（用于"进入 TUI 前必须先建立外部连接"）。
`SessionStart`/`SessionShutdown` 的 veto 只是 advisory。分发带超时，慢处理器不会
无限阻塞主流程。

### 资源发现

`api.register_resources_discover` 注册 handler，宿主在 `startup`/`reload` 时
fan-out 到所有已注册 handler，合并返回的 `{skillPaths, promptPaths, themePaths}`。
单个 handler 报错不中止 fan-out。`out` 是插件拥有的 `StbString`，宿主通过插件
注册时给的 `plugin_free_string` 回收。

### Provider 与渲染器（cdylib 槽位，已被 wasm 取代）

`api.register_provider` 注入自定义 LLM Provider（`provider_id`/`base_url`/
`api_style` + `request_fn`）。`register_message_renderer`/`register_markdown_transformer`/
`register_entry_renderer` 注册消息渲染能力。这些槽位都可能为 `None`，使用前必须检测。

> **0.8.0 起这两类能力的权威路径是 wasm**：provider 走 `provider-api` 导出 +
> `provider-http`/`provider-stream` 导入，渲染走 `render-api`（只支持 markdown 定稿转换）。
> 上面的 cdylib 槽位已被取代（P6 删除代码），新插件不要再依赖它们。细节见
> [plugin-abi.md §6.41/§6.42](../design/plugin-abi.md) 与本文 §12。

## 6. Rust 工具设计最佳实践

1. 把领域逻辑写成普通 Rust 函数，FFI 函数只负责转换、生命周期和错误映射。
2. 对普通逻辑写 `rlib` 单元测试；对 ABI 注册、加载和卸载写至少一个宿主 smoke test。
3. Schema 和实现使用同一字段命名，拒绝未知字段；对路径、URL、命令、数量和输出大小设置上限。
4. `poll` 不执行长时间阻塞任务。耗时工作放入线程/任务，handle 保存状态；取消标记必须能让任务在有限时间内结束。
5. 全局可变状态使用明确同步，避免把 session 或 tool call 状态存进无保护的 `static mut`。
6. runtime action、provider、renderer、event handler 和 resources handler 都是可选能力；注册前检查 vtable slot。
7. 插件注册失败返回非零；单个工具执行失败返回结构化错误，不要让整个宿主退出。
8. README 记录工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式。

## 7. 使用 ki dev 开发

在扩展 crate 目录运行：

```bash
ki dev
```

ki 会通过 Cargo metadata 识别当前 `cdylib`，首次编译后把版本化副本放在工作区
`.ki/extensions/.dev/`，启动正常 TUI，并监控 `src/`、`build.rs`、package/workspace
`Cargo.toml` 和 `Cargo.lock`。源文件变化后会自动编译并走现有 reload 流程；编译失败
会保留当前已加载版本。

多扩展 workspace 必须明确选择 package：

```bash
ki dev --package my-ext-a     # 占位名：换成你的 workspace 成员名
ki dev -P my-ext-b            # `-P` 是 `--package` 的缩写
```

其他模式：

```bash
ki dev --release
ki dev --no-watch
ki dev-local
ki dev --package my-ext-a -- --model gateway/model
```

调试单个扩展时使用 `ki dev-local`（等价于 `ki dev --local-only`）。它只加载当前
编译的扩展、当前项目目录的 skill/prompt，以及该扩展发现的 skill/prompt；不会扫描
全局扩展、已安装 Pi 包或其他项目资源，避免同名命令和无关命令干扰调试。内置命令
（包括 `/reload`）仍然可用。没有 Cargo cdylib 时优雅降级为 skills-only 模式。

TUI 中执行 `/reload` 会强制重新运行 Cargo build，再加载新阶段产物。Windows 上
已加载 DLL 无法原地覆盖，因此不要手工把 Cargo target DLL 复制到固定文件名；
`ki dev` 的版本化 staging 会安全地完成切换，并在会话退出、loader 释放后清理。

## 8. 项目本地扩展包

开发完成后，可以把当前扩展构建并登记到目标项目：

```bash
# 在目标项目目录执行；--path 可指向另一个扩展源码目录
ki package link --path ../my-kiqi-extension --package my-kiqi-extension
ki package list

# 之后在目标项目目录直接启动 ki 即可加载
ki

# 删除项目包
ki package unlink my-kiqi-extension
```

项目包保存在 `.ki/packages/<name>/`，动态库位于其 `extensions/` 子目录；同一目录
还可以提供 `skills/`、`prompts/` 和 `themes/`。多个包可以重复执行 `link` 登记。

`ki package` 是 ki 的项目级 Rust 扩展包机制，不执行 npm/Node package。全局扩展
仍使用 `ki install`，开发热重载仍使用 `ki dev` 或 `ki dev-local`。项目包会随普通
`ki` 启动自动发现；包内动态库属于本机可信代码，请只登记可信源码。

## 9. 资源与配置约定

项目级资源放在 `.ki/`。同名资源优先级是项目内定义优先
高于 `.pi`，项目高于全局，package 最后：

```text
.ki/
├── SYSTEM.md
├── APPEND_SYSTEM.md
├── settings.json
├── skills/
├── prompts/
├── themes/
└── extensions/
```

动态 `resources_discover` 适合根据运行环境决定路径；固定资源优先使用 manifest 或
约定目录，便于安装器、欢迎页和 `/context` 在扩展代码运行前发现它们。

> **wasm 插件不用 `resources_discover`**：0.8.0 的权威路径是包约定
> `resources.json` + `resources/<kind>/`，`ki insmod` 复制它们并写 `resources.sha256`，
> 由 `resources` grant 决定加载哪些 kind（§12.4）。三方 cdylib 的
> `register_resources_discover` 槽位已被取代，P6 删除。

### 自定义加载路径

项目级 `.ki/settings.json` 可以声明额外资源路径；无需其他兼容
回退。路径相对于项目根目录，配置路径会排在约定目录前面：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

`skills`、`prompts`、`extensions` 也可分别作为三个路径数组的简写。全局
`~/.ki/agent/settings.json` 支持相同的 `skillDirs`、`promptDirs`、`extensionDirs`，
其中相对路径相对于全局 agent 目录。项目配置优先于全局配置；`.ki` 资源和设置排在
`.pi` 之前。

单次运行可以显式追加路径：

```bash
ki --skill ./extra-skills
ki --prompt-template ./extra-prompts
ki --extension ./target/debug/my_extension.dll
ki --extensions-dir ./target/debug
```

`--skill`、`--prompt-template`、`--extension` 和 `--extensions-dir` 都可以重复使用。
额外的 Rust 扩展目录也可通过 `KI_EXTENSIONS_DIR` 配置；Windows 使用分号分隔，
Linux/macOS 使用冒号分隔。命令行和环境变量适合本机开发、CI 与排错；团队共享配置
使用项目 `.ki/settings.json`。

## 9. 发布检查清单

- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 通过。
- `crate-type` 包含 `cdylib`，使用 `export_plugin!` 导出
  `kiqi_plugin_register`。
- 依赖已发布的 `kiqi-plugin-sdk` 兼容版本，不链接宿主私有 crate。
- `ki dev --no-watch` 能编译、加载并注册预期工具。
- `ki install <crate> --force` 的干净安装路径通过。
- Linux/macOS/Windows 构建由 CI 验证，README 标明支持矩阵。

## 10. 模型执行规则

模型收到"创建 ki Rust 插件、扩展工具"任务时：

1. 先调用 `docs` 查询 `authoring`（扩展开发）或 `debugging`（调试）。
2. 阅读当前仓库现有 manifest、SDK 版本和相邻扩展约定，不猜 API。
3. 优先复制 `examples/plugin-stub` 的最小骨架。
4. 实现领域逻辑与边界测试，再连接 loader/ABI。
5. 使用 `ki dev --no-watch` 与 `ki install <crate> --force` 做真实 smoke test。
6. 不声称未验证的 Pi capability 已完全兼容；明确记录降级行为。

## 11. OAuth 扩展接口（cdylib，已被 wasm 取代）

> **已被取代（0.8.0）。** 权威实现见 [plugin-abi.md §6.40](../design/plugin-abi.md)：
> `oauth-api` 导出 + `oauth-http` 导入，**token 由宿主持有，永不进入 guest 内存**。
> 本节保留 cdylib 的旧契约，直到 P6 删除代码；新插件请直接看 §12。
>
> 两条契约的关键差异：旧接口把 token 本身（`{"access":"…","refresh":"…",…}`）交给宿主，
> wasm 契约里**没有任何能跨边界的 token 类型**，只有 `oauth-credential-status`。

ki 的 OAuth 方向是“轻量核心 + 平台插件”：平台差异由扩展实现，宿主负责统一
发现、生命周期和后续的凭证存储。cdylib 通道当前仍提供可选的
`register_oauth_provider` 槽位（`crates/kiqi-plugin-sdk/src/lib.rs:1265`）；旧插件不需要
重新编译即可继续加载，但该槽位已无宿主侧消费者。

插件注册一个 JSON manifest 和一个 action callback：

```json
{
  "id": "example-platform",
  "displayName": "Example Platform",
  "scopes": ["models:read", "models:run"],
  "supportsPkce": true,
  "capabilities": ["authorization_code", "refresh", "revoke"]
}
```

OAuth callback 使用版本化 JSON action envelope。支持的动作是 `begin`、`exchange`、
`refresh` 和 `revoke`，但 action input 和平台流程完全由扩展定义。OAuth token 属于
敏感信息：不得写入普通事件、模型消息、工具结果、调试日志或错误文本。插件拥有
自己产生的输出字符串，宿主通过注册时提供的 `plugin_free_string` 回收。插件应使用
KI 的 provider-scoped credential bridge，而不是自行把 token 写入项目目录。

宿主只实现标准 interaction/credential/success/error response；浏览器、device-code、
PKCE、state、callback 解析和平台 token 映射全部属于扩展。API key fallback 继续保留，
但宿主 OAuth 层不再包含任何 Anthropic/OpenAI 等平台特判。

### OAuth action envelope

宿主调用已注册 provider 时，会发送如下请求：

```json
{
  "action": "begin|exchange|refresh|revoke",
  "providerId": "example-platform",
  "input": {}
}
```

插件返回版本化标准 response：

```json
{
  "version": 1,
  "kind": "interaction|credential|success|error",
  "payload": {}
}
```

只有扩展负责解析 provider-specific 字段并转换为标准 credential：

```json
{"access":"...", "refresh":"...", "expires_at":1234567890}
```

宿主不识别 authorization URL、callback、PKCE、state 或 provider token alias；这些
都属于扩展实现。非零返回码会被宿主转换为结构化错误，token 不能进入 agent
transcript、普通插件事件或日志。

`kiqi-extensions` 提供 `list_oauth_providers` 和 `request_oauth` 两个宿主侧入口。
它们只允许调用已经注册且 manifest `id` 匹配的 provider，并在读取插件输出后立即
调用插件提供的 `plugin_free_string`。宿主只做 opaque input 转发和标准 response
校验，不包含任何平台 provider 特判。

已接入通用 CLI 流程：

```text
ki auth login --provider <id>
  begin → interaction（由扩展决定展示内容和输入）
  exchange → credential（标准 access/refresh/expires_at）
```

宿主只展示标准 interaction payload，并将用户输入作为 opaque `input` 原样传回扩展。
浏览器回调、device-code、PKCE/state 等由扩展自行实现。`ki auth refresh
--provider <id>` 会传递标准 credential context 并调用 provider 的 `refresh` action；
`ki auth logout --provider <id>` 会先调用 `revoke`，成功后才删除本地凭证。插件不得
把 token 写入普通事件、模型消息或日志。

## 12. wasm 侧的等价能力（0.8.0，权威路径）

wasm 插件不注册槽位，改为**导出接口 + 授权 key**：能力是否存在只看 `grants.json`
（缺失 = 拒绝），策略随授权一起存。下面五个 key 都**不会被 `--yes` 自动授予**——它们的
策略无法从 import 名推导，必须显式执行 `ki plugin grant`（`AUTO_GRANT_EXEMPT`，
`crates/kiqi-cli/src/plugin_cli.rs:1272-1278`；无 tty 且无 `--yes` 时什么都不授）。
接口 → grant key 的唯一映射是 `grant_key_for_interface`
（`crates/kiqi-plugin/src/grants.rs:1224-1243`）。

| 能力 | 导出 / 导入面 | grant key | 授权命令 |
|---|---|---|---|
| OAuth | `oauth-api`（导出）+ `oauth-http`（导入） | `oauth` | `ki plugin grant <id> oauth --provider <pid> --profile <file.json>`（可重复成对） |
| Provider | `provider-api`（导出）+ `provider-http`/`provider-stream`（导入） | `provider` | `ki plugin grant <id> provider --model-json <file.json> --endpoint <scheme://host/prefix> [--method GET\|POST] --credential-from auth-json:<pid> [--timeout <s>]` |
| Render | `render-api`（导出） | `render` | `ki plugin grant <id> render --kind markdown` |
| Resources | 包约定（无 WIT 面） | `resources` | `ki plugin grant <id> resources --kind skills\|prompts` |
| 请求观察 / 补丁 | `on-event` 的 `before-provider-request` + `event-action.patch` | `provider-request` | `ki plugin grant <id> provider-request --observe [--headers names\|values] [--messages none\|full] [--patch]` |

拼写以 `ki plugin grant` 的用法文本为准（`crates/kiqi-cli/src/plugin_cli.rs:1227-1240`）。

### 12.1 OAuth：guest 是流程控制器，宿主做 HTTP 与存储

- 导出 `list-providers()`（自己声明能驱动哪些 IdP）与 `action(provider-id, action)`；
- 导入 `oauth-http` 只有 `token-request` 与 `revoke`：表单键白名单、`client_secret` 直接拒绝，
  grant_type 对应的 flow 与每个 scope 都在计费/HTTP 前按 grant 校验；返回只有
  `stored(status) | failed(string)`，原始 token 响应体从不回传；
- 端点、`client-id`、scope 全部来自 grant；`action.begin(string)` 只携带 PKCE
  `code_challenge`（设备码流传空串），授权 URL 由宿主构造，guest 返回的 `url` 被忽略；
- token 存到命名空间 key `plugin:<install-id>:<provider-id>`；`ki auth refresh` /
  `ki auth logout` 是纯宿主操作（refresh token 直达 IdP，logout 先 revoke 再删 key）；
- 凭据绑定签发 profile 指纹（provider-id、client-id、各端点 origin）；指纹缺失或不匹配时
  refresh/logout/revoke 拒绝把旧 token 发往新 IdP。logout 只有显式 `--force-local` 才在不发
  远端请求的情况下删除本地 OAuth 项；删除按类型判断，绝不移除 `ApiKey`；
- **边界**：内建 provider 的 `stored_api_key` 仍只匹配 `ApiKey`，不会自动采用 OAuth entry；
  wasm provider 则可在显式 `--credential-from` 后使用 OAuth access token（§6.41）。

### 12.2 Provider：grant 是唯一书目

- 导出 `list-providers()`（安装期发现与交叉核对的建议性元数据）与 `request(request-json)`；
  CLI catalog 直接来自 grant 的 `models`，guest 不能增加未授权模型；请求 model 也必须在 grant 里；
- `WasmProvider` 已接入 resolve 与 `/model`：每次 resolve 有自己的 slot，首次 `stream_simple`
  才编译/实例化专用 `ProviderHost`；每次请求都重读 grant，revoke 后下一次请求直接拒绝，profile
  改变则重建实例；
- 宿主用 grant 覆盖返回消息上的 `provider`/`model`/`api`，guest 不能冒充别的模型；
- 流式用 `provider-stream.emit`/`emit-batch`：delta 带 `kind`（text/thinking/tool-call）、
  `phase`（start/delta/end）与 `content-index`，只在 provider 调用栈上有效；聚合上限为
  16,384 deltas / 1,024 blocks / 16 MiB，CLI 另有容量 256 的实时队列，溢出即取消并失败；
- `provider-http.open/read/close` 是**请求过滤器**：必须同时匹配 grant 的
  scheme/host/path-prefix/methods，否则 `denied` 且不建立连接；原始路径的 `%2F`、编码 dot、
  dot segment 与重复 `/` 都拒绝，重定向拒绝，guest 的认证/框架头被剥离，凭据只由宿主注入；
  句柄按调用回收；
- 凭据在 grant 里只是不透明句柄：`--credential-from auth-json:<pid>` 由宿主解析，
  `{source, provider, display, fingerprint}` 写入 `plugins/<id>/credentials.json`（0600，
  在 fs jail 之外），`ki plugin show` 只展示指纹前缀；每次请求重新读取值并校验指纹，改变即拒绝；
- 宿主墙钟覆盖 WASI 阻塞，CLI 用 `select!` 同时等宿主结果与 delta；超时后不再等遗留 guest
  sender 关闭，立即以错误终止流。

### 12.3 Render：只做 markdown 定稿转换

- `render-api.transform-markdown(markdown) -> result<string, plugin-error>` 是唯一函数，
  纯文本到文本；没有消息 / entry renderer 的 wasm 等价物；
- 跑在 deny-all 实例上（独立 Engine/Store，没有 session/ui/exec），流式期间不调用，
  每条定稿消息恰好调用一次；
- 超时默认 2000 ms（上限 10 s），实例化也有 fuel/epoch/墙钟上限；transform 由墙钟等待独立
  worker，超时 detach 而非 join，epoch 负责纯 CPU 自旋。输出默认 1 MiB（上限 8 MiB）；宿主先剥
  ANSI、控制字符判硬错误、再按 UTF-8 边界截断；任何失败都保留原文并记一条诊断。

### 12.4 Resources：包约定，不是注册

- guest crate 在 `plugin.wasm` 旁放 `resources.json` 与 `resources/<kind>/`；
  `ki insmod` 复制它们并写 `resources.sha256`（manifest 自己也在摘要内，安装后手改会失败）；
- manifest 是包声明，grant 是用户授权，加载取两者交集；插件目录追加在最后并按名字
  first-wins，内建/项目资源永远赢过插件；
- 主题（themes）被丢弃：`ThemeManager` 是 preset 驱动的。
