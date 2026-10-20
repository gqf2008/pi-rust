# AGENTS.md — 本仓库给 AI 代理与协作者的约定

## 这个仓库是什么

`ki`（crate 前缀 `kiqi-`）：一个 Rust 编码代理。仓库由 **`gqf2008/pi-rust`** 维护，
开发在 `walgit` 远端的 `main` 分支上推进。

| 远端 | 角色 |
|---|---|
| `walgit` | **主开发远端**（`gqf2008/pi-rust` 的本地镜像，日常推送） |
| `fork` | GitHub 镜像 / 发布目标 |
| `origin` | `bigfish1913/pi-rust`，无关分叉，禁用 |

## 命名（改名已完成，勿回退）

命令行是 **`ki`**，项目名是 **`kiqi`**。2026-10 的改名把 crate / 插件 / wasm guest
目录、package 名、feature（`plugin-kiqi-*`）、Rust 路径（`kiqi_*`）、插件 id、
文件名、运行时目录（`.ki/`）与资源（`.ki/skills/`）统一到了 `kiqi-`。

`git grep -i rpi` 只应命中这三类，**不要**「顺手清干净」：

1. **外部项目名**：`pi-rust/rpi-package`（被收编插件的来源 registry，~30 个文件里
   用作溯源路径）、`@juicesharp/rpiv-todo`（第三方 npm 包）。改名会伪造出处、打断链接。
2. **有意保留的旧数据/旧配置读取**，每处都在原位标注了 `legacy trace` 与失败模式：
   `rpi.value`（`crates/kiqi-harness/src/session/values.rs`）、更新缓存的
   `#[serde(alias = "rpi")]`（`crates/kiqi-cli/src/updates.rs`）、
   `KI_*` 对 `RPI_*` 的回退（`plugins/kiqi-langfuse`、`plugins/kiqi-permissions`、`crates/kiqi-cli/src/permissions.rs`）。
   删掉它们会静默丢数据或静默忽略用户已配置的路径。
3. **历史记录**：CHANGELOG 旧条目、设计文档里的旧 worktree 名等。

`website/data/packages.json` 里的 `"author"` 标签、包名、`install` 命令与
`repository` 路径已统一为 `kiqi-`（main 里这一组本来就是全统一的 `rpi-`），
只有仓库名 `pi-rust/rpi-package` 作为外部标识保留。

## 上游已分叉，与本项目无关（**重要，勿误判**）

- 远端 `origin` = `bigfish1913/pi-rust`：**这是一个已分叉、与本项目没有任何关系的仓库**。
  它既不是本项目的上游，也不是下游。
- 因此：**不要**向它提 issue/PR、不要向它推送、不要从它合并/同步、不要把它当作变更来源或
  对齐目标；汇报时也不要说「上游如何」。日常推送主远端 `walgit`
  （`gqf2008/pi-rust` 的本地镜像）；`fork` 只作 GitHub 镜像与发布目标。
- 本项目**没有上游**。引用历史出处时用 commit SHA，不要用 `origin/main` 之类做基线。

## 跟踪与流程

- **`fork` 关掉了 issue**（`gh issue list -R gqf2008/pi-rust` 会报 disabled）。
  批次跟踪落在仓库内的设计文档（`docs/design/*.md` 的分轮记录）与提交信息上，
  不要因为「没有 issue」而跳过立项/验收记录。
- 一批一个 worktree（`.claude/worktrees/wt-<批名>`），批次完成、门禁全绿后 ff-合入 `main`，
  并清理 worktree 与分支。**合并与清理是自动收尾，不逐次询问用户**，判据见「硬规矩」。
- 提交信息用 Conventional Commits（中英混排在本仓库历史里都有，前缀必须是规范形式）。

## 本地门禁（= CI 跑的东西）

```bash
cargo fmt --all -- --check
# 本地可选加速：下面每条都可以加 `--offline`（`Cargo.lock` 覆盖全部 feature，
# 离线也能全绿）。CI 在线跑，所以若某个 feature 引入了未锁定的依赖，
# 症状只会是「CI 卡在下载」而你本地无感——偶尔用 --offline 跑一次能提前发现。
cargo check --workspace --all-targets --locked
# 全部 feature 也要能编：否则一个坏掉的公开 feature 会一直无人发现
# （实例：`kiqi-tools` 的 `webp` 调用了 image 0.25 没有的 API，`--features webp` 直接编译失败）
cargo check --workspace --all-targets --all-features --locked
# 测试被非默认 feature 挡住的 crate：`cargo test -p kiqi-ai` 只编译 49 条而不是 208 条
# （整个 anthropic provider 在 `cfg(feature = "providers")` 下），单跑该包会静默跳过
cargo test  -p kiqi-ai -p kiqi-tools --lib --locked --all-features
cargo test  --workspace --locked
# 可选 feature（有 wasm 版的插件都不在默认构建里）必须单独跑，否则它们会腐烂：
# 这份清单必须与 .github/workflows/ci.yml 的 "opt-in plugin features" 步骤逐项一致——
# 少列一个，本地门禁就比 CI 松，而 CI 那边仍是红的（2026-10-20 就漏了
# plugin-kiqi-codegraph 与 plugin-kiqi-orchestrator 两项，已补齐）。
cargo test -p kiqi-cli --lib --locked --features plugin-kiqi-todo,plugin-kiqi-lens,plugin-kiqi-webfetch,plugin-kiqi-websearch,plugin-kiqi-subagents,plugin-kiqi-goal,plugin-kiqi-plan-mode,plugin-kiqi-ask-user,plugin-kiqi-langfuse,plugin-kiqi-permissions,plugin-kiqi-codegraph,plugin-kiqi-orchestrator
# Cargo.lock 必须覆盖全部 feature 的依赖（离线可解析）；CI 在线跑，
# 缺项的后果只会是「CI 卡下载 / 本地离线报错」，两边都不像根因
sh scripts/check-offline-lock.sh
# `install.sh` 是 `curl | sh` 的入口，离线可测（`--dry-run --version` 不联网）
sh scripts/tests/install-sh.sh
# `release.sh` 必须与它编排的工作流一致（asset 名、push 的远端）
sh scripts/tests/release-sh.sh
# README 的安装指令必须与清单一致（全仓 crate 都是 publish = false）
sh scripts/tests/docs-install-path.sh
# 文档快照（改了 CHANGELOG.md / README.md 等必须重跑，CI 会比对）
sh scripts/sync-embedded-docs.sh
# CI 还会把每个插件单独编成 cdylib（compile-time feature 关闭时的入口），
# 因为 workspace 整体构建会把它们都开成 compile-time、从而丢掉 cdylib 导出。
# 本地不跑这一步，`ki dev` 的热载路径坏了也看不见：
cargo build --locked -p kiqi-todo -p kiqi-lens -p kiqi-webfetch -p kiqi-websearch -p kiqi-subagents -p kiqi-permissions -p kiqi-goal -p kiqi-plan-mode -p kiqi-codegraph -p kiqi-ask-user -p kiqi-langfuse -p kiqi-orchestrator
# 上面两份清单与 CI 的一致性（CI 也会跑；改任何一侧都要过这条）
sh scripts/check-feature-parity.sh
# `plugin-abi.md` 的 §6.x 编号连续性，以及全仓 `§6.N` 引用是否都能落到真实小节
# （曾经的缺陷：四处引用 §6.34，而它从未被写过）
sh scripts/check-abi-sections.sh
# CI 里每一条 cargo build/check/test 都必须带 --locked（曾经的缺陷：
# `windows-check` 独独没带，于是红叉不再指向某个提交，可能只是上游发了新版本）
sh scripts/check-ci-locked.sh
# 发布矩阵与三处消费清单（updates.rs 的 release_target_triple、install.sh、
# install.ps1）必须给出同一组 target；只改一侧会得到「没人能下载」或「下载 404」
sh scripts/check-release-targets.sh
# 所有「给用户看的示例版本号」（install.sh --help、install.ps1 的 .EXAMPLE 与
# -Version 提示、release-binaries.yml 的 dispatch 输入说明）都必须真的发布过。
# 查 release 而不是查 tag——本地有 v0.3.0/v0.3.19 的 tag 但从未发布，正是漏掉的那类。
# 需要联网；完全无网时设 KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK=1 才退回查 tag（较弱，会告警）
sh scripts/check-install-version-hints.sh
```

注意：goal 测试已用子进程 + tempdir 隔离，跑 `cargo test -p kiqi-cli` 不再改写源码树
（`crates/kiqi-cli/.ki/` 已移出索引并被 `.gitignore` 的 `**/.ki/*` 忽略）。
wasm guest 只对 `wasm32-wasip2` 链接，**guest crate 里不写单测**，
行为一律由 `crates/kiqi-plugin-host/tests/e2e.rs` 从宿主侧验证。

## 两条插件通道（改动前先分清）

| 通道 | 形态 | 适用 |
|---|---|---|
| **编译期** `plugins/kiqi-*` | `kiqi_plugin::Plugin` trait（进程内、无沙箱），另带 cdylib 面供 `ki dev` 热载 | 一方插件 |
| **wasm** `plugins/wasm/kiqi-*-guest` | 组件模型 + `plugins/kiqi-plugin.wit` 契约，跑在 wasmtime 沙箱里，按授权拿能力 | 第三方；契约版本见 WIT 的 `package` 行 |

**默认构建不再编入任何一方插件**：权限策略已宿主化（`crates/kiqi-cli/src/permissions.rs`），`kiqi-permissions` 与其余插件一样改为 `--features plugin-kiqi-<name>` 或装 wasm guest（保留一个版本；`kiqi-orchestrator` 已随 §6.34 迁 opt-in、`kiqi-codegraph` 随 `proc` 迁 wasm）。
有 guest 的那些都是 `--features plugin-kiqi-<name>`，或用 `ki insmod --build plugins/wasm/kiqi-<name>-guest`
装上 wasm 版。**新增/改插件能力时，两侧（trait 面与 WIT 面）必须一起改**，
差异要写进 guest README，不许藏。

## 设计文档在哪

- `docs/design/plugin-abi.md`：wasm 契约与能力模型的**唯一真源**；每轮改动加一节 `### 6.x`，
  §7 是待定项。
- `docs/design/plugin-trait.md`：编译期 trait 契约。
- `docs/design/plugin-abi-handover.md`：**接手先读这个**（现状/能力表/上手命令/已知限制/提交轨迹）。
- 未落地的大改走单独方案文件（如 `docs/design/plugin-process-capability.md`）。

## 硬规矩

- 删除文件/目录/远端分支前，先列清单并取得明确授权（用户全局规则 `RULE_未经授权禁止删除文件.md`）。
- **例外（2026-10-20 用户授权，本仓库批次流程）**：批次已完成、门禁全绿，且同时满足
  `git rev-list --count main..<branch>` 为 0、worktree 无未提交改动时，
  **ff 合入 `main` 并删除该批次的本地 worktree 与本地分支是自动收尾，不再逐次询问用户**。
  未合入分支、有未提交改动的 worktree、远端分支，以及其它文件/目录，仍按上一条取得授权。
- 上限/超时/额度这类常量上调，单独 commit 并写明新旧值与理由（`RULE_阈值变更.md`）。
- 报告「结论」必须带可复现判据（文件:行号或命令），不得凭命名/同类外推（`RULE_能力与依赖结论必须实核不得外推.md`）。
