> 历史存档：本文记录已完成的 crate 合并方案；当前宿主实现位于 crates/kiqi-host。

# 方案：`kiqi-host` 合并与 `Extension → Extension` 改名

状态：块 1、块 3 已完成（2026-10-26，`main @ 291ca0e`）；块 2 待执行。
本文补记 2026-10-24/25 会话中定稿、但当时只存在于会话记录里的设计。

## 1. 目标

把插件面收敛成**一个**宿主 crate：

- `kiqi-extension`（契约：trait + `EventTag` + grants + dialogs + 共享常量）
- `kiqi-extension-host`（wasmtime 后端，跑 `extensions/kiqi-extension.wit` 的 guest）
- `kiqi-extensions` 的幸存部分（registry、`translate` 漏斗、compiled sink、
  UI 信箱、`ActionBridge`、stdio 生命周期、`ToolCallContext`、provider hooks）

→ 合并为 **`kiqi-host`**，wasmtime 放在 `wasm` feature 后面（契约与派发面不拉
wasmtime），trait 由 `Extension` 改名为 **`Extension`**。

收益：`kiqi-cli` 只依赖一个 crate（3 条依赖边变 1 条），"宿主 + 插件契约"成为
一个可读的整体。

## 2. 已定决策

### 2.1 trait 命名：`Extension`

两轮 Jev 评分（约束逐轮收紧）：

| 候选 | 第一轮（无约束） | 第二轮（加 Rust `*Ext` 惯例约束） |
|---|---|---|
| `Extension` | 0.46 | **0.57** |
| `AgentExt` | 0.37 | 0.11 |
| `HostExtension` | 0.54 | **0.05** |

约束原话：*trait 由插件实现、不是给 host 类型加方法的扩展 trait；`XxxExt` 在
Rust 里约定俗成是「给已有类型加方法的扩展 trait」，语义错了要扣分*。
于是定案 `Extension`；`impl Extension for WasmExtension` / `dyn Extension`。

### 2.2 合并的粒度

- 契约面（原 `kiqi-extension`）**不挂 gate**：`kiqi-tools`/`kiqi-mcp`/`kiqi-cli`
  无条件依赖它的常量与 `EventTag`。
- wasmtime 只在 `wasm` feature 后面；`kiqi-cli` 的 `extension-wasm` feature 映射到
  `kiqi-host/wasm`。
- 原 `kiqi-extensions` 的模块整体并入（`registry`/`translates`/`compiled`/
  `actions`/`status`/`editor_text`/`tool`/`provider*`/`resources`/`stdio`/`abi`），
  它的 `crate::` 引用改指 `crate::ext::*`（或直接平铺到根）。

### 2.3 块 1 的落地偏差（记录）

最初的块 1 计划是「删 cdylib，并把 `kiqi-extensions` 一并删掉」。执行前用户改选了
**A 方案**：保留 `kiqi-extensions` 作为派发脊柱（wasm + TUI + acp 都骑着它），
只删装载层。因此块 2 的「吸收幸存部分」指的是**整个** `kiqi-extensions`。
共享常量最终落在 `kiqi-extension`（当时 `kiqi-host` 尚不存在）。

## 3. 目标 crate 布局

```text
crates/kiqi-host/
  src/lib.rs            # 契约 + 根 re-export（EventTag / Extension / grants / consts）
  src/grants.rs         # 原 kiqi-extension
  src/dialogs.rs
  src/abi.rs            # 原 kiqi-extensions（内部事件总线线格式）
  src/registry.rs       #   ...
  src/translate.rs
  src/compiled.rs
  src/actions.rs
  src/tool.rs
  src/stdout... (stdio.rs)
  src/wasm/             # 原 kiqi-extension-host（engine/store/gates/oauth/provider_*/...）
```

Cargo feature：

```toml
[features]
default = []
wasm = ["dep:wasmtime", "dep:tokio", ...]
```

## 4. 迁移步骤（块 2）

1. 开分支 `refactor/kiqi-host`，基线门禁全绿后再动。
2. `git mv crates/kiqi-extension crates/kiqi-host`，package 改名（版本沿用 0.4.1）。
3. 把 `crates/kiqi-extension-host/src/*` 移入 `crates/kiqi-host/src/wasm/`，内部
   `crate::` → `crate::wasm::`；Cargo 依赖 wasmtime 系列全部挂到 `wasm` feature。
4. 把 `crates/kiqi-extensions/src/*` 平铺进 `crates/kiqi-host/src/`（与其根同名
   冲突的 `lib.rs` 合并），内部 `crate::` 不变（同一 crate 根）。
5. 全仓替换 crate 名：`kiqi_extension` / `kiqi_extension_host` / `kiqi_extensions`
   → `kiqi_host`（消费者：cli / acp / mcp / tools）。
6. `Extension` → `Extension` 全仓改名（trait 定义、`impl`、`dyn`、文档、测试替身）。
7. workspace members / workspace.dependencies / `kiqi-cli` 的 `extension-wasm`
   feature 映射 / Cargo.lock。
8. 文档：CLAUDE.md 分层图、AGENTS.md、`extension-abi.md` §6.x、handover、CHANGELOG。
9. 门禁：fmt / check ±all-features / workspace 测试 / 脚本门禁；合并、推送。

## 5. 遗留待定

- ~~契约 crate 合并后 `kiqi-tools`/`kiqi-mcp` 传递依赖 `kiqi-host → ai/agent`~~
  ✅ 2026-10-26 收口（方案 A）：环境/身份常量下沉到 **`kiqi-agent::env`**
  （三个消费方本就依赖 agent，零新边），`kiqi-tools` 与 `kiqi-mcp` 直接删掉
  `kiqi-host` 依赖；事件类型（`EventTag`/`Event`/`EventAction`）只被 `kiqi-cli`
  与 `kiqi-host` 使用，留在 `kiqi-host`。于是**没有**新增 `kiqi-contract` crate。
- `kiqi-extensions` 里的 C-ABI 注册机（`HostApi`/vtable）仍是不可达代码，
  折叠成 serde 总线是独立批次（`extension-abi.md` §6.45）。
