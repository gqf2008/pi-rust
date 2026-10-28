# AGENTS.md

`ki`（crate 前缀 `kiqi-`）：Rust 编码代理。

## 远端

| 远端 | 用途 |
|---|---|
| `walgit` | 主开发远端，日常推送 |
| `origin` | `gqf2008/pi-rust` GitHub 镜像 / 发布目标 |

本项目已与旧上游脱离关系：不配置、不拉取、不合并旧上游，历史出处只用 commit SHA。

## 命名

CLI = `ki`，crate 前缀 = `kiqi-`。`git grep -i rpi` 命中的是外部项目名、legacy
数据回退或历史记录，不要清。

代码/文档/CLI/WIT 统一用 **extension**，不用旧词。wasm 是唯一插件通道。

## 门禁

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo check --workspace --all-targets --all-features --locked
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

## 插件

wasm 是唯一插件通道（`extensions/wasm/kiqi-*`，`extensions/kiqi-extension.wit`，
`kiqi:extension@0.9.0`）。cdylib / 编译期通道均已退役。安装：`ki insmod --build`。

## 设计文档

- `docs/design/extension-abi.md` — 契约唯一真源
- `docs/design/extension-abi-handover.md` — 接手先读
- `docs/design/kiqi-host-merge.md` — kiiqi-host 合并记录
