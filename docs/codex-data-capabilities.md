# Codex 数据能力与边界

更新日期：2026-07-14

验证版本：`codex-cli 0.144.1`

## 结论

需求 1、2、4、5 可以实现。需求 3 可以实现“5 小时/周 reset cycle 内的精确本地 token 占比”和“基于当前账户 gauge 的额度代理估算”，但不能实现服务端意义上精确到任务或 turn 的配额贡献。

运行状态也有一个边界：连接到同一个 App Server 的线程可以获得精确状态；普通方式独立启动的 Codex CLI 属于不同运行时，只能从进程和持久化事件推断状态。

## v0.1 实现边界

当前工具启动独立 App Server，只调用账户额度与账户用量读取方法。这个新 runtime 无法提供其他已经运行的 CLI/IDE/Desktop task 的精确 active flags，所以 v0.1 的 task 状态来自 rollout turn 边界与文件新鲜度，并明确标为 `inferred` 或 `stale`。官方 thread status API 是未来统一 runtime 模式的数据能力，不是当前实现已经声称拥有的能力。

在线额度以 App Server 为准；rollout 中的额度快照用于离线 fallback。实体 EST 不再尝试从相邻整数快照重建严格 delta，而是始终使用当前普通 `codex` gauge 与同一 reset cycle 的短上下文价格加权用量占比做 Low-confidence 投影。主要显示口径 `TOKEN%` 是未加权的本地可观察 token 占比；一次性命令与 TUI 使用相同定义。

扫描不完整、lookback 不足或 task 状态 stale 会保留在 `partial` / `partialReasons` 和 provenance 中，但不会关闭仍可计算的 EST。只有没有当前 `codex` 窗口或窗口内没有本地非 Spark token 分母时，归因才不可用。

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
- `rateLimitResetCredits`：可空汇总；`availableCount` 是可用重置机会的权威数量，不能用可能被服务端截断的 `credits` 详情数组长度代替；`credits: null`（或旧响应缺字段）表示只知道数量，`credits: []` 表示明细已读取且为空；
- `rateLimitResetCredits.credits[]`：每项保留原始 `status`、`resetType`、可选 `title` / `description`，以及 `grantedAt` / 可空 `expiresAt`；输出将时间规范化为 RFC 3339，`expiresAt: null` 表示不会过期。服务端的不透明 `id` 只做输入校验，不进入 Snapshot 或输出；
- `planType`、`credits`、`individualLimit`、`rateLimitReachedType`。

工具必须按 `windowDurationMins == 300` 和 `10080` 识别 5 小时/周窗口。若服务端返回其他时长，应显示通用窗口名称，而不是把 primary/secondary 硬编码为 5 小时/周。

单条 reset-credit 明细非法时，采集器保留 `availableCount` 和其他有效明细，丢弃坏行并把 Limits 标为 partial；汇总容器或 `availableCount` 非法时才丢弃整个 reset-credit 汇总，已解析的额度窗口仍然保留。服务端允许限制返回的明细数，因而数组长度小于 `availableCount` 只表示明细截断，不改变权威数量。整个账户刷新失败时，缓存的 count 和明细一起保留并标为 stale；成功刷新明确返回 `credits: null` 或 `credits: []` 时以新值为准，不回填旧明细。

同一 duration 的多个账户百分比不能混合。额度列表继续保留 App Server 返回的全部桶，并在 TUI Other 的 Resets 分组逐项显示 primary/secondary 的具体 reset time；同一张表还显示已返回的重置机会明细，分组标题独立显示 `availableCount` 及 provenance。归因只选择规范化 `limit_id == codex` 的当前 5 小时或周窗口。模型名去除首尾空白后与 `gpt-5.3-codex-spark` 大小写不敏感精确相等的调用从归因中排除；其他调用（包括缺失或空模型名）都进入普通 `codex` 的本地 token 分母。`codex_bengalfox` 和其他桶只用于 gauge/Other，不生成 task、turn 或 model attribution。监控器只读取 reset credits，不调用 consume 接口。

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

subagent 的 owning `session_meta` 通常提供直接父 thread：新版位于 `source.subagent.thread_spawn.parent_thread_id`，旧版可回退到顶层 `parent_thread_id` / `forked_from_id`。`forked_from_id` 也可能出现在普通 resume/fork，因此只有 metadata 已确认 subagent 身份时才能建立父子关系；`session_id` 表示根会话，不可代替直接父节点。旧日志缺少父标识、父 rollout 不在扫描范围或父任务被 TUI 过滤时，只能把该 subagent 作为当前视图的根节点。Tree 模式收起父节点时，父行会在渲染层汇总当前过滤树内隐藏后代的 token、所选 reset cycle `TOKEN%` 与 estimated quota；它不改变底层 task/turn 归属，也不会影响一次性输出。

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
- Codex 配额的模型/服务层权重不保证等同于公开 API 价格；
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

该值在界面中标记为 `TOKEN%` / `TOKEN SHARE`，含义是本地可观察 token 占比，不能称为官方额度贡献。

同一公式同时适用于当前 5 小时和周 reset cycle。周周期只有在本地扫描覆盖服务端周期起点，且没有 `max-files` 截断、坏行、不可读文件或歧义 counter reset 时才能标为本地精确。`--days 7` 表示本地扫描 lookback，不定义周窗口；若配置的 `--days` 不足以覆盖 `resetsAt - 10080m`，只有周分析标为 partial，完整的 5h 分析不受污染。每项 `windowAnalyses` 通过自己的 `partial` / `partialReasons` 表达这一点；额度 gauge 仍可保持服务端完整，因为它不依赖 rollout lookback。

### 可以估算但不能精确归因

当前实现采用单一、可解释的价格代理公式。它固化 [OpenAI API Pricing](https://developers.openai.com/api/docs/pricing?latest-pricing=priority) 的短上下文 Standard/Priority 价格，单位均为美元/百万 token：

| model | Standard input | Standard cached | Standard cache write | Standard output | Fast input | Fast cached | Fast cache write | Fast output |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `gpt-5.6-sol` | 5.00 | 0.50 | 6.25 | 30.00 | 10.00 | 1.00 | 12.50 | 60.00 |
| `gpt-5.6-terra` | 2.50 | 0.25 | 3.125 | 15.00 | 5.00 | 0.50 | 6.25 | 30.00 |
| `gpt-5.6-luna` | 1.00 | 0.10 | 1.25 | 6.00 | 2.00 | 0.20 | 2.50 | 12.00 |
| `gpt-5.5` | 5.00 | 0.50 | - | 30.00 | 12.50 | 1.25 | - | 75.00 |
| `gpt-5.4` | 2.50 | 0.25 | - | 15.00 | 5.00 | 0.50 | - | 30.00 |
| `gpt-5.4-mini` | 0.75 | 0.075 | - | 4.50 | 1.50 | 0.15 | - | 9.00 |

实现以 `$0.025/百万 token` 为一个整数价格单位，避免浮点累计误差。`priority` 使用 Fast 列，缺失、`default` 或其他 service tier 使用 Standard 列。模型名采用去除首尾空白后的大小写不敏感精确匹配，不从未知后缀猜测基础模型：

```text
local_share_percent = entity_non_spark_tokens / all_local_non_spark_tokens * 100
cached = min(call_cached_input_tokens, call_input_tokens)
uncached = call_input_tokens - cached
call_price_units = uncached * input_rate + cached * cached_rate + call_output_tokens * output_rate
estimated_quota_percent = codex_used_percent * entity_price_units / all_price_units
```

`reasoning_output_tokens` 是 output 的子集，不能再次相加。rollout 当前不暴露 `cache_write_tokens`，因此上表的 GPT-5.6 cache-write 价格无法进入历史 EST；`input - cached` 只能按普通 input 价格处理。只有 `total_tokens` 而缺少 input/output breakdown 的旧记录按 uncached input 降级，并增加 `token_breakdown_missing` partial reason。

缺失或不在价目表中的非 Spark 模型仍保留 `TOKENS` / `TOKEN%`，并按 `gpt-5.6-luna` 的对应 Standard/Fast 价格降级，以免静默丢出分母；该窗口增加 `unpriced_model_rate_fallback` partial reason。原始 token 与 `TOKEN%` 应用同一个未加权分母，EST 则对 task、turn 和 model 使用同一个价格分母；task/model EST 合计等于当前 `codex` 的 `usedPercent`，缺少 turn id 的调用会使 turn 行合计低于该值。所有可计算结果在数据模型/JSON 中仍标记为 Low；TUI/text 的实体行只用 `~` 表示近似、用 `-` 表示不可用，不再重复 confidence 标签。估算方法、`externalActivityPossible` 与具体 partial reasons 在每个 scope 摘要中统一展示。扫描不完整、lookback 不足、价格降级或状态 stale 只会降低可信度并标记 partial/stale，不会清空仍有分母的 EST。

该公式隐含“Codex 配额相对成本近似 API 短上下文价格，且本机看到了账户活动”的强假设。真实 Codex 配额权重、cache write、其他设备或云 task、服务端取整与缺失日志都可能让 EST 偏离真实贡献，所以它只能称为 `estimated quota share`，不能称为官方配额账单。JSON v1 为兼容旧消费者保留既有 attribution 汇总字段，但它们不再驱动当前实体 EST。

## 多窗口输出与交互

TUI 只保留 Overview 与 Other 两个顶层视图。Other 保留数据健康信息，并新增 Resets 分组：同一张 `ITEM / STATE / GRANTED / RESET TIME` 表先显示重置机会，再按 bucket 展开所有 primary/secondary 窗口。`RESET TIME` 使用本地 `YYYY-MM-DD HH:MM:SS ±HH:MM`，宽布局的 `GRANTED` 使用相同格式，窄布局缩短为 `MM-DD HH:MM`。窗口缺少 `resetsAt` 时显示 unavailable，机会的 `expiresAt` 为 `null` 时显示 never；标题显示权威的可用数、provenance，并区分 `DETAILS UNAVAILABLE`、正常截断的 `SHOWING n/N` 与解析异常的 `PARTIAL`。明确的零显示 `0 available`，整个刷新失败保留的旧汇总和明细都标为 stale。Overview 最顶栏提供 `[V]Turns`、`[M]Models`、`[5h]` 与 `[Week]`，分别由 `V`、`M`、`5`、`W` 或鼠标左键操作；Turns 首次启动默认显示。Tasks、选中 task 的 Turns、Models 及 Models 内的 attribution 摘要必须在一次切换中使用同一个 `codex` reset cycle，不可用的 scope 显示 unavailable，不能借用另一时长或 `codex_bengalfox` 的数据。Models 先显示当前 `codex` scope 的 gauge、本地非 Spark token、带 `~`/`-` 的 EST 和 scope 级方法/external/partial-reasons 摘要，再显示不含独立 confidence 列的模型表；Turns 或 Models 隐藏后顶栏恢复入口和 scope 控件仍然可达，归因信息不再占用独立 TUI 面板。

TUI 将主题、顶层视图、window scope、Turns/Models 显隐、Flat/Tree 和来源筛选保存为版本化的用户级 JSON。读取失败或内容损坏时回退到默认值；搜索、选择、滚动位置和具体 thread 折叠集合不持久化。写入采用同目录临时文件替换，未来版本文件不会被旧程序覆盖；`--theme` 显式值优先于保存值。CLI 一次性输出不参与此状态生命周期。

一次性输出通过 `windows` 子命令或 `snapshot --section windows` 暴露全部 `windowAnalyses`。独立 TUI Attribution 面板的删除不影响 CLI/JSON attribution 能力；旧 JSON v1 的 task/turn `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence` 与顶层 `models`、`attribution` 固定保留首选 5h 语义，新增 Week 分析不能静默改变这些字段，避免旧消费者把周数据误认为 5h。此次展示简化不改变 `statusConfidence`、task/turn/model/窗口 usage 的 `quotaConfidence` 或 attribution `confidence` 的 JSON 字段名与枚举值。

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

当没有 active 或 stale task 时，最终服务端快照可以把 `settled` 标为 true。此时：

- task/turn/model token 可以精确结算；
- 扫描完整的当前 5 小时或周 reset cycle `TOKEN%` 可以精确结算；
- 按当前 `codex` gauge 与短上下文价格加权用量占比得到的 estimated quota share 仍保持 Low；
- 服务端 task/turn quota 仍然不能精确计算。

取整、模型隐藏权重、快照间隙和外部活动不会因为本地任务停止而消失，所以 idle 不是把 estimated 变成 exact 的条件。

## 稳定性和安全

- 使用默认 stable API surface；MVP 不依赖 `thread/turns/list` 等 experimental 方法。
- 启动时检测 Codex 版本，并为协议解析保留兼容层和未知字段容忍。
- 可用 `codex app-server generate-json-schema` 生成与安装版本严格匹配的 schema。
- 只读 Codex JSONL 与 App Server，不写入或修改其状态；reset-credit 明细也只通过 `account/rateLimits/read` 获取，不调用 consume；v0.1 不查询 SQLite。
- 不直接读取、复制或缓存 `auth.json`，App Server 自行处理认证。
- 默认索引聚合字段，不持久化提示词、回复、工具输出或 secrets。
- TUI 的 Recent tasks 与 Turns 使用轻量背景色及单字符标记表达状态，Tasks 底部始终显示统一图例，Turns 收起时仍可见；一次性 text 输出包含消息摘要，JSON 使用 `messagePreview`。
- `--redact-content` 不保留 task 标题或 turn 消息摘要。

## 官方依据

- [Codex App Server](https://developers.openai.com/codex/app-server)
- [OpenAI Codex app-server source](https://github.com/openai/codex/tree/main/codex-rs/app-server)
- [App Server protocol README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
