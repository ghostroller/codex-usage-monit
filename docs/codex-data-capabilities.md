# Codex 数据能力与边界

更新日期：2026-07-13

验证版本：`codex-cli 0.144.1`

## 结论

需求 1、2、4、5 可以实现。需求 3 可以实现“5 小时/周 reset cycle 内的精确本地 token 占比”和“额度变化估算”，但不能实现服务端意义上精确到任务或 turn 的配额贡献。

运行状态也有一个边界：连接到同一个 App Server 的线程可以获得精确状态；普通方式独立启动的 Codex CLI 属于不同运行时，只能从进程和持久化事件推断状态。

## v0.1 实现边界

当前工具启动独立 App Server，只调用账户额度与账户用量读取方法。这个新 runtime 无法提供其他已经运行的 CLI/IDE/Desktop task 的精确 active flags，所以 v0.1 的 task 状态来自 rollout turn 边界与文件新鲜度，并明确标为 `inferred` 或 `stale`。官方 thread status API 是未来统一 runtime 模式的数据能力，不是当前实现已经声称拥有的能力。

在线额度以 App Server 为准；rollout 中的额度快照用于离线 fallback 和来源一致时的补充。TUI 进程会积累连续服务端快照，一次性命令不会跨进程保存快照历史。

TUI 默认使用 dark 主题，可通过 `--theme light`（`bright` 为别名）启动浅色主题，并在运行中按 `t` 切换。主题属于渲染层，不影响数据采集、精度标记或一次性 text/JSON。

## 术语

- **任务 / Thread**：一条 Codex 会话，包含多个 turns。
- **对话 / Turn**：一次用户请求及随后发生的 agent 工作。
- **模型调用**：turn 内产生一次 token usage 更新的模型响应。
- **额度窗口**：由服务端返回的 `windowDurationMins` 和 `resetsAt` 标识，周期为 `resetsAt - windowDurationMins` 到 `resetsAt`，不能用 `now - duration` 推导。
- **周 reset cycle**：`windowDurationMins == 10080` 的当前服务端周期，不是滚动过去 7 天，也不保证与日历自然周重合。
- **本地 token**：当前 `CODEX_HOME` 下可以从 rollout JSONL 观察到的 token。
- **配额占比**：OpenAI 服务端 `usedPercent`，不是 token 的同义词。

## 数据源矩阵

| 数据源 | 可提供数据 | 精度与时效 | 边界 |
| --- | --- | --- | --- |
| App Server `account/rateLimits/read` | 各额度桶的使用百分比、窗口时长、重置时间、计划、credits | 服务端当前快照，精确到返回的整数百分比 | 仅账户级，不含 thread/turn 明细 |
| App Server `account/rateLimits/updated` | 额度变化通知 | 实时、稀疏更新 | 需要与最近完整快照合并 |
| App Server `account/usage/read` | lifetime、daily token 活动、峰值、streak | 服务端账户级 | 日粒度，不能用于 5 小时任务归因 |
| App Server `thread/list` / `thread/read` | 线程元数据、标题、cwd、来源、runtime status | 元数据可靠 | 非本 App Server 加载的线程通常为 `notLoaded` |
| App Server `thread/status/changed` | `idle`、`active`、`systemError`、等待审批/输入 | 同一 App Server 内精确、实时 | 无法观察其他独立 CLI 运行时 |
| App Server `thread/tokenUsage/updated` | thread、turn、last/total token、上下文窗口 | 同一 App Server 内精确、实时 | 历史 turn 的 token 没有直接包含在 `Turn` 中 |
| Rollout JSONL | thread/turn 边界、用户消息摘要、模型、token、当时的额度快照、完成/中断 | 本地历史最细来源，可增量 tail | 内部格式；用户消息不一定带 `turn_id`，ephemeral、缺失日志或格式升级会造成不完整 |
| `session_index.jsonl` | 用户可见的 thread 标题与重命名时间 | 小型本地追加日志；可按 thread 取最新标题 | 可能缺失旧 thread；损坏或并发写入中的行需忽略并回退 rollout 摘要 |
| `state_5.sqlite` | thread 元数据和聚合 `tokens_used` | 快速、本地 | 内部 schema；没有 turn token 和跨进程 runtime status |
| 进程与文件新鲜度 | Codex 进程存活、最近写入 | 推断 | 无法可靠区分思考、等待审批和等待输入 |

## 已验证字段

### 额度

`account/rateLimits/read` 返回：

- `rateLimits`：兼容视图；
- `rateLimitsByLimitId`：多额度桶；
- `primary` / `secondary`：`usedPercent`、`windowDurationMins`、`resetsAt`；
- `planType`、`credits`、`individualLimit`、`rateLimitReachedType`。

工具必须按 `windowDurationMins == 300` 和 `10080` 识别 5 小时/周窗口。若服务端返回其他时长，应显示通用窗口名称，而不是把 primary/secondary 硬编码为 5 小时/周。

若同一 duration 返回多个当前桶，不能把它们的账户百分比混合。周期边界候选应优先 Codex 产品桶和服务端 provenance；仍有歧义时使用稳定排序选一个并输出 warning，其他候选只保留在额度 gauge 中。由于本地 `UsageCall` 没有 limit id，duration 级 token 构成仍可展示，但无法证明属于所选桶；因此该窗口必须标为 partial，并禁用所有实体的 estimated quota。

### Thread 与 Turn 状态

Thread 状态：

- `notLoaded`
- `idle`
- `systemError`
- `active`

`activeFlags` 当前包括：

- `waitingOnApproval`
- `waitingOnUserInput`

Turn 状态：

- `inProgress`
- `completed`
- `interrupted`
- `failed`

### Token

Token breakdown 包括：

- `inputTokens`
- `cachedInputTokens`
- `outputTokens`
- `reasoningOutputTokens`
- `totalTokens`

`cachedInputTokens` 是 input 的子集，`reasoningOutputTokens` 是 output 的相关细分；展示时不能把所有字段再次相加。

Rollout JSONL 中可利用以下事件重建历史：

- `task_started`：turn id 和开始时间；
- `user_message`：用户消息正文，以及部分版本可选的 turn id；
- `turn_context`：turn id、模型、reasoning effort；
- `token_count`：last/total token 和额度快照；
- `task_complete`：完成时间和耗时；
- `turn_aborted`：中断原因和时间。

近期 rollout 的 `turn_context.effort` 通常可提供 `low/medium/high/xhigh/ultra`；部分旧版本没有该字段，只能显示 unknown。`thread_settings_applied.thread_settings.service_tier=priority` 可以为下一次激活的 turn 提供 Fast 标识，TUI 把 `FAST` 放在模型名称后，普通 turn 不显示。Desktop 的 `session_meta.source` 当前仍可能是底层兼容值 `vscode`，具体客户端应优先读取 `originator=Codex Desktop`；子代理角色则优先由结构化 source/thread source 判断。

subagent 的 owning `session_meta` 通常提供直接父 thread：新版位于 `source.subagent.thread_spawn.parent_thread_id`，旧版可回退到顶层 `parent_thread_id` / `forked_from_id`。`forked_from_id` 也可能出现在普通 resume/fork，因此只有 metadata 已确认 subagent 身份时才能建立父子关系；`session_id` 表示根会话，不可代替直接父节点。旧日志缺少父标识、父 rollout 不在扫描范围或父任务被 TUI 过滤时，只能把该 subagent 作为当前视图的根节点。Tree 模式收起父节点时，父行会在渲染层汇总当前过滤树内隐藏后代的 token、所选 reset cycle local share 与 estimated quota；它不改变底层 task/turn 归属，也不会影响一次性输出。

工具最多保留每个 turn 首条用户消息的 72 字符摘要。有显式 `turn_id` 时直接归属；没有时只归入当前 active turn，若当前没有 active turn，则不猜测归属。部分 subagent turn 没有明文 `user_message`，摘要显示 `-`，不会从注入上下文反推。

对每个 turn 的 token 统计应使用累计计数的单调增量，避免重复的 `token_count` 通知被重复相加。

## 当前窗口归因的硬边界

OpenAI 当前没有暴露类似以下字段：

```text
thread_quota_units
turn_quota_units
request_rate_limit_cost
```

服务端只提供账户级 `usedPercent`，而本地提供 token。二者不能直接等同，原因包括：

- 百分比是整数，存在取整；
- 不同模型和服务层可能有未公开权重；
- cached input、output、reasoning 或特殊工具的额度影响不一定线性；
- 多个本地线程可能并发；
- 其他设备、IDE、桌面端或云任务可能共享额度；
- 快照通知可能延迟或缺失；
- reset credit 会改变窗口边界。

### 可以精确提供

当前额度窗口内，本地可观察 token 的占比：

```text
turn_token_share = turn_total_tokens / observed_local_window_total_tokens
```

该值必须标记为 `local token share`，不能称为官方额度贡献。

同一公式同时适用于当前 5 小时和周 reset cycle。周周期只有在本地扫描覆盖服务端周期起点，且没有 `max-files` 截断、坏行、不可读文件或歧义 counter reset 时才能标为本地精确。`--days 7` 表示本地扫描 lookback，不定义周窗口；若配置的 `--days` 不足以覆盖 `resetsAt - 10080m`，只有周分析标为 partial，完整的 5h 分析不受污染。每项 `windowAnalyses` 通过自己的 `partial` / `partialReasons` 表达这一点；额度 gauge 仍可保持服务端完整，因为它不依赖 rollout lookback。

### 可以观测但不能精确归因

在相同 `limitId` 和相同窗口内，可以计算相邻快照：

```text
observed_delta = max(0, current_used_percent - previous_used_percent)
```

这只是账户额度变化。若区间内只有一个本地模型调用且没有可见并发，可把它显示为“观测到的百分点变化”，并附置信度。若区间内有多个任务、并发、缺口或外部活动，则必须进入 `unattributed`，或者在用户明确接受后按 token 代理量做 `estimated` 分摊。

v0.1 采用已经确认的 token 比例代理量，但只分摊间隔不超过 5 分钟的快照。扫描截断、坏行、不可读文件、累计 token counter 回退、长快照间隔和没有本地调用的变化不分摊。Counter 回退样本只重建 baseline 并标记 partial，避免把共享基线重复算作消费。服务端与 rollout 明显不一致时只使用服务端历史；百分比回退会开启新的观测 epoch。即使区间成功分摊，其他设备或云 task 仍可能同时贡献该 delta，因此 schema 保留 `externalActivityPossible`，confidence 不会达到 High。

### 不允许的口径

不能用下面的公式冒充官方配额账单：

```text
turn_tokens / window_tokens * current_used_percent
```

它可以作为估算代理，但必须明确标为 `estimated quota share`，并同时展示置信度和未归因部分。

## 多窗口输出与交互

TUI 只保留 Overview 与 Data Health 两个顶层视图。Overview 的 Models 标题提供 `[5h]` / `[Week]` scope，分别由 `5` / `W` 或鼠标左键选择；Tasks、选中 task 的 Turns、Models 及 Models 内的 attribution 摘要必须在一次切换中使用同一个 reset cycle，不可用的 scope 显示 unavailable，不能借用另一时长的数据。Models 可由 `M` 或 Tasks 标题中的 `[M]Models` 隐藏和恢复，归因信息不再占用独立 TUI 面板。

一次性输出通过 `windows` 子命令或 `snapshot --section windows` 暴露全部 `windowAnalyses`。独立 TUI Attribution 面板的删除不影响 CLI/JSON attribution 能力；旧 JSON v1 的 task/turn `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence` 与顶层 `models`、`attribution` 固定保留首选 5h 语义，新增 Week 分析不能静默改变这些字段，避免旧消费者把周数据误认为 5h。

## 状态可信度

所有状态记录都应带 provenance：

| provenance | 含义 |
| --- | --- |
| `live` | 来自拥有该 thread 的 App Server runtime |
| `inferred` | 来自 JSONL turn 边界、进程存活和文件新鲜度 |
| `stale` | 存在未闭合 turn，但进程或日志长期无活动 |
| `unknown` | 数据不足，不能可靠判断 |

独立 CLI 的 `waitingOnApproval` 和 `waitingOnUserInput` 不能从持久化日志百分百恢复。若用户需要所有任务都具备精确 live 状态，需要让这些 Codex TUI 统一通过 `codex --remote ...` 连接同一个 App Server。

## 所有任务结束后的精度

当没有 active 或 stale task 时，最终服务端快照可以把 `settled` 标为 true，并让已经观测到的额度 delta 不再继续变化。此时：

- task/turn/model token 可以精确结算；
- 扫描完整的当前 5 小时或周 reset cycle 本地 token share 可以精确结算；
- estimated quota share 可以提高到 Medium confidence；
- 服务端 task/turn quota 仍然不能精确计算。

取整、模型隐藏权重、快照间隙和外部活动不会因为本地任务停止而消失，所以 idle 不是把 estimated 变成 exact 的条件。

## 稳定性和安全

- 使用默认 stable API surface；MVP 不依赖 `thread/turns/list` 等 experimental 方法。
- 启动时检测 Codex 版本，并为协议解析保留兼容层和未知字段容忍。
- 可用 `codex app-server generate-json-schema` 生成与安装版本严格匹配的 schema。
- 只读 Codex JSONL 与 App Server，不写入或修改其状态；v0.1 不查询 SQLite。
- 不直接读取、复制或缓存 `auth.json`。
- 默认索引聚合字段，不持久化提示词、回复、工具输出或 secrets。
- TUI 的 Recent tasks 与 Turns 使用轻量背景色及单字符标记表达状态，Turns 底部显示统一图例；一次性 text 输出包含消息摘要，JSON 使用 `messagePreview`。
- `--redact-content` 不保留 task 标题或 turn 消息摘要。

## 官方依据

- [Codex App Server](https://developers.openai.com/codex/app-server)
- [OpenAI Codex app-server source](https://github.com/openai/codex/tree/main/codex-rs/app-server)
- [App Server protocol README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
