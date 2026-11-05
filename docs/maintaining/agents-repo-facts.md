# 仓库技术事实（协作章程附录）

> 本文件承接协作章程 [`AGENTS.md`](../../AGENTS.md) 中的仓库技术事实：
> 远端拓扑、macOS 发行签名、命名与术语、每条门禁背后的原因。
> 协作流程、派工、验收与合并规则看 `AGENTS.md`；crate 分层与门禁概览看
> [`CLAUDE.md`](../../CLAUDE.md)。本文件与 `CLAUDE.md` 冲突时，以 `CLAUDE.md`
> 中更具体、更新的条目为准，并回头修正本文件。

## 1. 远端

`ki`（crate 前缀 `kiqi-`）：Rust 编码代理。仓库由 `gqf2008/pi-rust` 维护。

| 远端 | 用途 |
|---|---|
| `walgit` | 主开发远端（`http://127.0.0.1:8081/gqf2008/pi-rust.git`），日常推送 |
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

**GitHub Issues 已禁用**（`gh issue list -R gqf2008/pi-rust` 报 disabled），也**不要**
向 `origin` 提 issue/PR。跨团队事项的唯一事实源是本机 walgit collab board，
规则见 `AGENTS.md` §7 与 [`docs/design/walgit-collab-loop.md`](../design/walgit-collab-loop.md)。

## 2. macOS 发行版必须带 JIT entitlements

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

## 3. 命名与术语

CLI = `ki`，crate 前缀 = `kiqi-`。`git grep -i rpi` 命中的是外部项目名、legacy
数据回退或历史记录，不要清：

1. **外部项目名**：`pi-rust/rpi-package`（被收编插件的来源 registry，~30 个文件里
   用作溯源路径）、`@juicesharp/rpiv-todo`（第三方 npm 包）。改名会伪造出处、打断链接。
2. **有意保留的旧数据/旧配置读取**，每处都在原位标注了 `legacy trace` 与失败模式：
   `rpi.value`（`crates/kiqi-harness/src/session/values.rs`）、更新缓存的
   `#[serde(alias = "rpi")]`（`crates/kiqi-cli/src/updates.rs`）、
   `KI_*` 对 `RPI_*` 的回退（`crates/kiqi-cli/src/permissions.rs`）。
   删掉它们会静默丢数据或静默忽略用户已配置的路径。
3. **历史记录**：CHANGELOG 旧条目、设计文档里的旧 worktree 名等。

代码/文档/CLI/WIT 统一用 **extension**，不用旧词；wasm 是唯一插件通道，
cdylib / 编译期通道均已退役。安装：`ki insmod --build`。

| 概念 | 用词 | 位置 |
|---|---|---|
| 可安装单位（用户面） | extension | `ki extension …`、`$KI_AGENT_DIR/extensions/<id>/`、`extension.wasm` |
| 宿主侧实现契约（Rust） | `Extension` trait | `crates/kiqi-host` |
| 组件模型产物 | guest | `extensions/wasm/kiqi-*`（仅 host/guest 语境保留后缀） |
| 契约身份（WIT） | `kiqi:extension@0.9.0` | `extensions/kiqi-extension.wit` |

改名前的名字**不再兼容**；已安装的 guest 需要重新 `ki insmod`。历史提交信息里的旧词
是历史记录，不回改。

## 4. 门禁与每条检查的原因

命令清单以 `AGENTS.md` §3 为准；这里只记录「为什么需要它」。

- `cargo check --workspace --all-targets --all-features --locked`：一个声明了却编译不过的
  公开 feature 会一直无人发现（实例：`kiqi-tools` 的 `webp` 调用了 image 0.25 没有的 API）。
- `cargo test -p kiqi-ai -p kiqi-tools --lib --locked --all-features`：整包测试被非默认
  feature 挡住时会静默少跑（`kiqi-ai` 只编译 49 条而不是 208 条；整个 anthropic provider
  在 `cfg(feature = "providers")` 下）。`--all-features` 的 `check` 只编译、不运行，
  跑挂的 feature 测试在这一步是隐形的。
- `cargo check -p kiqi-cli --no-default-features --locked`：默认 feature 会通过 feature
  unification 掩盖坏掉的 lean build。
- `check-offline-lock.sh`：`Cargo.lock` 必须覆盖全部 feature 的依赖，否则离线构建
  （本机 `.cargo/config.toml` 可能设了 `offline = true`）报的错看起来像本地配置问题，
  而 CI 在线只会静静地下载。
- `check-ci-locked.sh`：CI 里每条 cargo 命令都必须带 `--locked`；漏带时红灯不再指向某个提交。
- `check-doc-paths.sh` / `tests/doc-paths.sh`：根 README/CLAUDE/AGENTS、`docs/**` 与
  embedded docs 引用的仓库路径必须真实存在；历史存档文档按文件头豁免。
- `check-release-targets.sh`：发布矩阵（producer）与 `updates.rs` 的
  `release_target_triple`、`install.sh`、`install.ps1`（consumers）必须给出同一组 target。
- `check-install-version-hints.sh`：安装器 help 里的示例版本必须真的发布过（查 release
  而不是查 tag——本地 tag 里有从未发布的版本）。需要联网与 `GITHUB_TOKEN`；完全无网时
  可设 `KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK=1`，但这是较弱检查且会告警。
- `check-release-version-consistency.sh`：README/ROADMAP 里手写的当前版本必须与 workspace
  版本一致，否则 embedded docs 会把陈旧版本随二进制发出去。
- `tests/install-sh.sh`：`install.sh` 是 `curl | sh` 入口，`--dry-run --version` 离线可测。
- `tests/release-sh.sh`：`release.sh` 必须与它编排的发布工作流一致（asset 名、推送远端）。
- `tests/docs-install-path.sh`：全仓 crate 都是 `publish = false`，README 与网站 Packages
  目录不得出现 crates.io 链接、裸 `cargo install <crate>`／`cargo add`、非零下载量。
- `tests/collab-entry-retry.sh`：`walgit collab entry` 偶发网络/5xx；包装器只重试瞬态，
  不重试 schema/validation/signature。
- `tests/setup-collab-agents.sh`：一个 agent 一个 worktree + 一个 walgit principal。
- `sync-embedded-docs.sh`：`crates/kiqi-cli/embedded-docs` 是 `include_str!` 快照，
  CI 比对；改了 `CHANGELOG.md`、`docs/kiqi-tool/*.md` 或 README 后重跑并提交结果。
- `check-abi-sections.sh`：`docs/design/extension-abi.md` 的 §6.x 编号连续，且全仓
  `§6.N` 引用都能落到真实小节。

worktree 里用 `CARGO_TARGET_DIR=$PWD/target`，主仓基线用独立目录；**不要**让多个
worktree 共用一个 target 目录（同包名同 metadata 的测试二进制「后建者赢」，会跑到别的树的产物）。
`windows-check` 在真实 Windows runner 上跑 `cargo check --workspace --all-targets --locked`，
因为 ring 等原生依赖需要 Windows 工具链。

## 5. 设计文档入口

- `docs/design/extension-abi.md` — 契约唯一真源
- `docs/design/extension-abi-handover.md` — 接手先读
- `docs/design/walgit-collab-loop.md` — walgit collab 协作协议
- `docs/design/kiqi-host-merge.md` — kiqi-host 合并记录
- `docs/maintaining/releasing.md` — 发布流程
