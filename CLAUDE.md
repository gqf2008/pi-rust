# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 先读 AGENTS.md

仓库协作约定（远端角色、`ki`/`kiqi` 命名与遗留 `rpi` 的处置、worktree 批次流程、
硬规矩）以根目录 **AGENTS.md** 为准，本文不重复其细节。最容易踩的三条：

- 远端 `origin`（`bigfish1913/pi-rust`）是**无关分叉**（unrelated fork）：不推、不拉、不合并、不当变更来源。
  日常推送主远端 `walgit`（`gqf2008/pi-rust` 的本地镜像），`fork` 作 GitHub 镜像与
  发布目标。本项目没有上游，引用历史出处用 commit SHA。
- 命名是 `ki`（命令）/ `kiqi`（crate 前缀）。`git grep -i rpi` 的命中大多是**有意保留**
  （外部项目名 `pi-rust/rpi-package`、legacy 数据/配置读取回退、历史记录），
  三类清单见 AGENTS.md，不要「顺手清干净」。
- 删除文件/目录/分支前先列清单取得明确授权。唯一例外：批次完成、门禁全绿、
  `git rev-list --count main..<branch>` 为 0 且 worktree 干净时，ff 合入 `main`、
  推送 `walgit`/`fork` 并清理 worktree 与分支是**自动收尾**，不再逐次询问。
  提交信息用 Conventional Commits。

## 常用命令

本地门禁 = CI 跑的东西（个别本地-only 已注明）；cargo build/check/test 一律带 `--locked`（可加 `--offline`，
Cargo.lock 覆盖全部 feature；`cargo fmt` 不接受 `--locked`）：

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo check --workspace --all-targets --all-features --locked
# 被 feature 挡住的测试必须单跑 --all-features 才可见：
cargo test  -p kiqi-ai -p kiqi-tools --lib --locked --all-features
cargo test  --workspace --locked

# wasm guest 的纯逻辑单测（宿主目标）：
cargo test --manifest-path extensions/wasm/Cargo.toml -p kiqi-plan-mode --locked

# 脚本门禁（改到对应文件后必须重跑）：
sh scripts/check-offline-lock.sh          # Cargo.lock 覆盖全部 feature
sh scripts/check-ci-locked.sh             # CI 每条 cargo build/check/test 都带 --locked
sh scripts/tests/install-sh.sh            # install.sh（离线可测）
sh scripts/tests/release-sh.sh            # release.sh 与工作流一致
sh scripts/check-release-targets.sh       # 发布矩阵三处消费清单一致
sh scripts/sync-embedded-docs.sh          # 改了 CHANGELOG.md / README.md 后必须跑
sh scripts/check-abi-sections.sh          # 改了 docs/design/extension-abi.md 后必须跑
sh scripts/tests/docs-install-path.sh     # 文档安装指令与清单一致（禁 crates.io 指令）
sh scripts/tests/collab-entry-retry.sh    # walgit collab entry 只重试瞬态（本地-only，CI 不跑）
sh scripts/tests/setup-collab-agents.sh   # 协作身份：一个 agent 一个 worktree + principal
sh scripts/check-install-version-hints.sh # 示例版本号必须真的发布过（需联网）
```

跑单个测试：`cargo test -p <crate> --locked <test_name>`（加 `-- --nocapture` 看输出）。

两个结构性陷阱：

- **default-members**：根目录裸 `cargo build` / `cargo test` 只覆盖
  `kiqi-agent`/`kiqi-tools`/`kiqi-harness`/`kiqi-cli` 四个 crate（见根 `Cargo.toml`），
  其余 crate 要 `--workspace` 才进来。
- **worktree 里必须显式 target**：本机 `.cargo/config.toml` 设了相对
  `build.target-dir`，嵌套 worktree 的构建会写进主仓 `target/`——worktree 里复用陈旧
  rlib 报不存在的错误，主仓可能跑到分支树的测试二进制。worktree 一律
  `CARGO_TARGET_DIR=$PWD/target cargo test ...`，主仓基线用独立目录复跑
  （`CARGO_TARGET_DIR=/tmp/ki-base-target ...`）。判据见 AGENTS.md。

MSRV 1.96（由 wasmtime 49 拉高，见根 `Cargo.toml` 的注释）。全部 crate
`publish = false`，安装走本仓库 GitHub Releases + `scripts/install.sh`；
发布编排见 `scripts/release.sh` 与 `Taskfile.yml`（`task publish`）。

## 架构

`ki` 是终端编码代理，`kiqi-*` 是拆开的 SDK；同一套实现既能嵌入自己的进程
（`examples/minimal`、`examples/tools`），也能作为 wasm 插件扩展每个会话
（`extensions/wasm/kiqi-*`）。架构总览：`docs/architecture/overview.md`。

### crate 分层（依赖边自各 Cargo.toml 实核）

```text
kiqi-telemetry            # 遥测接口与 span/event 合约（零内部依赖）
└─ kiqi-ai                # 消息类型、Provider、流式 SSE、faux provider
   └─ kiqi-agent          # Agent Loop、工具调用、事件、队列、取消；`env` = 宿主身份/环境变量名
      └─ kiqi-tools       # 内置工具 + ExecutionEnv 抽象（真实/离线环境）
         └─ kiqi-harness  # 会话树、JSONL 持久化、压缩、崩溃恢复、skills
kiqi-tui                  # 终端组件、编辑器、渲染（零内部依赖）
kiqi-host                 # Extension 契约 + 事件派发脊柱（恒编译）+ wasm 后端（`wasm` feature，wasmtime）
kiqi-mcp                  # MCP 客户端集成（依赖 ai+agent+harness；boundary.rs 是与 harness 的接缝）
kiqi-acp                  # `ki acp serve` 协议服务器（依赖 acp 协议 crate + agent/harness/mcp/host；会话组装根留在 kiqi-cli 注入）
kiqi-cli                  # 组合以上（kiqi-acp 除外）：配置、认证、CLI/TUI；acp 的 factory/precheck 宿主侧在 src/acp.rs
```

feature 门：`kiqi-ai` 的真实 HTTP provider 在 `cfg(feature = "providers")` 后，默认关
（faux provider 让全部测试离线可跑）；`kiqi-cli` 默认开 `extension-wasm` + `clipboard`。

### 一次请求的路径

TUI Enter → `interactive_tui/run.rs::run_prompt_streaming` →
`AgentHarness::prompt_text`（`Arc<dyn AgentLane>` 分发；lane 由
`harness.lane("main")` 取得）→ `run_agent_loop`
（Provider 流式 → 工具执行 → 下一轮）→ `AgentEvent` 广播 → TUI 事件循环渲染。
运行中提交经 `lane.steer` 入队，由 loop 在检查点消费。完整分支见
`docs/agent/loop-walkthrough.md`。

### 会话模型（kiqi-harness）

会话是带 branch 和 lane 的 entry 树，JSONL 后端持久化，崩溃后可恢复。
压缩追加摘要 entry、保留原始历史（run 前与工具结果后两处检查点）。
恢复/重放受工具 replay 策略约束——不是所有工具都能安全重复执行。

### 插件通道（改动前先分清）

- **一方插件只走 wasm**：`extensions/wasm/kiqi-*`（组件模型，跑 wasmtime 沙箱），
  契约 `extensions/kiqi-extension.wit`（当前 `kiqi:extension@0.7.1`）。授权文件
  `$KI_AGENT_DIR/extensions/<id>/grants.json`，缺失 = 什么都不授予。
  安装：`ki insmod --build extensions/wasm/kiqi-<name>`。
  **改能力时 trait 面（`kiqi_extension`）与 WIT 面必须一起改**，
  差异写进 guest README。
- 编译期一方插件通道已于 2026-10-21 退役；**cdylib 通道（`kiqi-extension-sdk` /
  `ki dev` / `ki install` / `ki package` / `libloading` 装载）已于 2026-10-26 整体退役**——
  wasm 是唯一插件通道。三个宿主侧 crate 已合并为 **`kiqi-host`**（契约 + 派发脊柱
  + `wasm` 子模块；trait 由 `Extension` 改名为 `Extension`）：registry/translate/
  compiled sink/mailboxes 恒编译，wasmtime 只在 `wasm` feature 后；
  `src/abi.rs` 是内部事件总线的线格式。
- **wasm guest 的单测**：guest 的**纯逻辑**可以带原生 `#[cfg(test)]` 单测，在
  `extensions/wasm` 工作区里 `cargo test -p kiqi-<x>` 就能在宿主目标上跑
  （实测可行：wit-bindgen 会为宿主目标生成桩）。**跨 WIT 边界的行为**（import
  调用、宿主侧效果）一律由 `crates/kiqi-host/tests/e2e.rs` 从宿主侧验证
  （`--features wasm`）。

### 设计文档

- `docs/design/extension-abi-handover.md` — **接手 wasm 插件层先读这个**
- `docs/design/extension-abi.md` — wasm 契约唯一真源，每轮改动加一节 `### 6.x`
- `docs/design/extension-trait.md` — 编译期 trait 契约（历史存档）
- 其余方案文件在 `docs/design/`；已发布内容看 `CHANGELOG.md`，方向看 `ROADMAP.md`
