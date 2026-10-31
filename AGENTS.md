# AGENTS.md

`ki`（crate 前缀 `kiqi-`）：Rust 编码代理。

## 远端

| 远端 | 用途 |
|---|---|
| `walgit` | 主开发远端，日常推送 |
| `origin` | `gqf2008/pi-rust` GitHub 镜像 / 发布目标 |

本项目已与旧上游脱离关系：不配置、不拉取、不合并旧上游，历史出处只用 commit SHA。

**已彻底分叉（2026-10-31）**：旧上游 remote（`bigfish1913/pi-rust`）及其 112 个
remote-tracking ref 已从本仓删除。不要再 `git remote add upstream`、也不要
`git fetch --all`（会连带拉回上游）。自查：

```bash
git remote -v                      # 只应有 walgit / origin
git for-each-ref refs/remotes      # 只应有 origin/* 与 walgit/*
```

旧世代（0.9.x–0.15.x）遗留 tag 已于 2026-10-31 一并删除（44 个，含 preview）；现存的
tag 全部在 `main` 历史上。`walgit` / `origin` 上本来就没有这些 tag，因此 fetch 不会把它
们拉回来。

**`origin` 已不是 fork（2026-10-31）**：GitHub 不会自动解除 fork 关系，所以旧仓被改名为
`gqf2008/pi-rust-fork-archive`（仍是 fork，留着 v0.4.0–v0.4.6 的 release 与资产），
再在同名位置新建了一个**非 fork** 仓并推入全部 refs。判据：

```bash
gh api repos/gqf2008/pi-rust --jq .fork    # 必须是 false
```

新仓建仓时没有搬 release（旧资产在归档仓）。发布前需重建 release，或从归档仓转存。

## macOS 发行版必须带 JIT entitlements

`--options runtime`（公证要求）签出的二进制，如果没带 entitlements，一进入
wasmtime 的 JIT 页就被内核杀掉：`zsh: killed ki`、
`termination: {"namespace":"CODESIGNING","indicator":"Invalid Page"}`、
`signal: "SIGKILL (Code Signature Invalid)"`。0.4.6/0.4.7 的 macOS 资产都中过这个。
签名一律带 `packaging/macos/ki.entitlements`（见 `.github/workflows/release-binaries.yml`）；
该文件里的 XML 注释**不能含双破折号**，AMFI 解析器会报
`AMFIUnserializeXML: syntax error`，而 `plutil -lint` 照样放行。

同一版本重新上传资产后 digest 会变，频道刷新要显式加 `FORCE=1`
（`refresh-channels.sh` 默认按版本号短路）：
`FORCE=1 bash scripts/refresh-channels.sh 0.4.7`，再跑 `scripts/push-taps.sh 0.4.7`，
winget 的 `InstallerSha256` 也要同步改。

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
