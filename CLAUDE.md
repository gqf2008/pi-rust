# CLAUDE.md

## 仓库

`ki`（crate 前缀 `kiqi-`）：Rust 编码代理。远端 `walgit` 主推，`origin` GitHub 镜像 / 发布。

## 门禁

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo check --workspace --all-targets --all-features --locked
cargo check -p kiqi-cli --no-default-features --locked  # lean build
cargo test  -p kiqi-ai -p kiqi-tools --lib --locked --all-features
cargo test  --workspace --locked
sh scripts/check-offline-lock.sh
sh scripts/check-ci-locked.sh
sh scripts/check-doc-paths.sh
sh scripts/sync-embedded-docs.sh   # 改了 CHANGELOG.md / README.md 后必须跑
sh scripts/check-abi-sections.sh   # 改了 docs/design/extension-abi.md 后必须跑
sh scripts/tests/docs-install-path.sh
```

worktree 里用 `CARGO_TARGET_DIR=$PWD/target`，主仓基线用独立目录。
wasm guest 纯逻辑单测：`cargo test --manifest-path extensions/wasm/Cargo.toml -p kiqi-<x> --locked`。

MSRV 1.96（wasmtime 49 拉高）。全部 crate `publish = false`。

## 架构

```text
kiqi-telemetry            # 遥测接口与 span/event 合约
└─ kiqi-ai                # 消息类型、Provider、流式 SSE、faux provider
   └─ kiqi-agent          # Agent Loop、工具调用、事件、队列；`env` = 宿主身份/环境变量名
      └─ kiqi-tools       # 内置工具 + ExecutionEnv 抽象
         └─ kiqi-harness  # 会话树、JSONL 持久化、压缩、崩溃恢复、skills
kiqi-tui                  # 终端组件、编辑器、渲染
kiqi-host                 # Extension 契约 + 事件派发脊柱（恒编译）+ wasm 后端（`wasm` feature，wasmtime）
kiqi-mcp                  # MCP 客户端集成（boundary.rs 是与 harness 的接缝）
kiqi-acp                  # `ki acp serve` 协议服务器
kiqi-cli                  # 组合以上
```

## 插件

wasm 是唯一插件通道（`extensions/wasm/kiqi-*`，`extensions/kiqi-extension.wit`，
`kiqi:extension@0.9.0`）。cdylib / 编译期通道均已退役。安装：`ki insmod --build`。
事件总线是纯 serde。会话级 sink 与 caps。
guest 纯逻辑单测在 `extensions/wasm` 工作区跑宿主目标。

## 设计文档

- `docs/design/extension-abi.md` — 契约唯一真源
- `docs/design/extension-abi-handover.md` — 接手先读
- `docs/design/kiqi-host-merge.md` — kiiqi-host 合并记录
