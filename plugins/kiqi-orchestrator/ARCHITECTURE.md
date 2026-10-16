# Architecture — how the pieces collaborate

`kiqi-orchestrator` is one of several extensions loaded into a single `ki`
process. This file shows how it works with the host, with `kiqi-goal` (budgets)
and with `kiqi-mcp-adapter` (external tools).

## Static layout

```
┌────────────────────────────────────────────────────────────────────────────────┐
│ ki main process  =  the orchestrator / "foreman"  (one LLM loop)              │
│                                                                                │
│   main agent ──► tool calls                                                    │
│                                                                                │
│   ┌─ loaded extensions (~/.ki/agent/extensions/, manifest-managed) ────────┐  │
│   │                                                                        │  │
│   │  kiqi-orchestrator            kiqi-goal                 kiqi-mcp-adapter   │  │
│   │  ├ tool subagent             ├ tool goal              ├ mcp_list       │  │
│   │  ├ tool plan                 ├ /goal command          ├ mcp_call       │  │
│   │  ├ tool dispatch             └ TurnEnd hook:          └ mcp_request    │  │
│   │  └ before_agent_start guide*   counts turns /                         │  │
│   │     (only KI_ORCHESTRATOR_GUIDE=1)  expiry → pause                   │  │
│   └────────────────────────────────────────────────────────────────────────┘  │
│                                                                               │
│   built-in tools: read / write / edit / bash / grep / find / ls (parent+child) │
└───────────────────────────────────┬───────────────────────────────────────────┘
                                    │ dispatch: one child process per ready node
                                    ▼
   ┌────────────────────────────────────────────────────────────────────────┐
   │ child ki #1   child ki #2   child ki #3   child ki #4   (parallel)  │
   │                                                                        │
   │  · fresh context: cannot see the parent's conversation, only its task  │
   │  · env: KI_MAX_TURNS_PER_RUN=<node maxTurns>      (hard turn ceiling) │
   │         KI_ORCHESTRATOR_SUBAGENT=1                (no orchestration   │
   │                                                     guidance injected) │
   │  · args: --exclude-tools subagent,plan,dispatch     (recursion guard)   │
   │  · can use: built-in tools + MCP tools (if configured)                 │
   │  · stdout (final answer) ──► back to dispatch ──► parent context       │
   └────────────────────────────────────────────────────────────────────────┘
```

\* The guide is appended to the system prompt only when
`KI_ORCHESTRATOR_GUIDE=1`, and never in a child.

## Example — goal: "summarise 4 crates, then build an index"

```
 human         kiqi-goal           main agent(LLM)        orchestrator tools      child ki procs
 │                │                    │                      │                       │
 │ /goal start …  │                    │                      │                       │
 │ /goal turns 20 ├─► active goal      │                      │                       │
 │ /goal time 30m │   (TurnEnd counts) │                      │                       │
 │                │                    │                      │                       │
 │  KI_ORCHESTRATOR_GUIDE=1 ─► adds "use plan/dispatch for multi-step work"
 │                │                    │                      │                       │
 │ "finish this"  │                    │                      │                       │
 ├────────────────────────────────────► plan                 │                       │
 │                │                    ├ plan(set 5 nodes) ──►│ build graph + acyclic │
 │                │                    │   a,b,c,d free;  e deps=[a,b,c,d]           │
 │                │                    │                      │                       │
 │                │                    ├ dispatch(maxPar=4) ─► ready = {a,b,c,d}      │
 │                │                    │                      ├── spawn 4 children ──►│ ×4
 │                │                    │                      │     each read+write   │
 │                │                    │                      │◄── result / exit ─────┤
 │                │  TurnEnd #1 ◄──────┤                      │ verify expectFile     │
 │                │  turns 1/20        │◄── results fed back ─┤ mark done / failed    │
 │                │                    │                      │                       │
 │                │                    ├ dispatch(maxPar=1) ─► ready = {e}             │
 │                │                    │                      ├── spawn 1 child ─────►│
 │                │                    │                      │◄── index.md written ──┤
 │                │                    │◄── 5/5 done ─────────┤                       │
 │                │                    │ "goal met" / /goal stop                      │
 │◄───────────────┴────────────────────┘                      │                       │

 budget exhausted (20 turns / 30 min) ─► kiqi-goal: paused + notice ─► agent stops
 hard turn ceiling                    ─► KI_MAX_TURNS_PER_RUN=N (host stops the run)
 cancel (TUI Ctrl+C / RPC abort)      ─► orchestrator cancel ─► kill whole child tree
```

## Who does what

| Component | Role | Key point |
| --- | --- | --- |
| **ki host** | Runs the agent loop, loads extensions, ships built-in tools | Also offers the hard turn ceiling `KI_MAX_TURNS_PER_RUN` |
| **kiqi-orchestrator** (this crate) | The foreman's scheduler: `plan` authors the DAG, `dispatch` runs a wave, `subagent` runs one task | Order, parallelism, acceptance and cancellation live in code; `done` is written only by `dispatch` |
| **kiqi-goal** | The foreman's watch: one objective with turn/time budgets; auto-pauses + injects a notice | Bounds *how long*, not *how the work is split* |
| **kiqi-mcp-adapter** | External toolbox: exposes MCP server tools | Available to parent and children |
| **child ki process** | The worker: one node per process, fresh context | Has no orchestrator tools (recursion guard); its output returns to the foreman |

In one line: **`kiqi-goal` bounds *how long*, `kiqi-orchestrator` decides *how the
work is split and run*, `kiqi-mcp-adapter` decides *which external tools are
available*.**
