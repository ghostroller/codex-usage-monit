# 产品需求文档

更新日期：2026-08-25

## 1. 产品目标

用一个本地终端工具回答五个问题：

1. 最近有哪些 Codex tasks，它们是运行、完成、中断还是状态不确定？
2. 当前 5 小时和周额度用了多少、何时重置？
3. 当前 5 小时或周重置周期内，哪些 tasks、turns 和模型产生了 token，并占本地活动与估算额度的多少？
4. 每个 task 和 turn 使用了多少 token？
5. 如何一次性输出全部或指定部分信息？

## 2. 已确认口径

- task 与 Codex thread 同义；对话指一个 turn；
- token 使用 Codex 本地累计计数的增量，`totalTokens` 不与 breakdown 重复相加；
- 300/10080 分钟窗口显示为 5 小时/周，其他时长使用通用标签；
- 周窗口严格表示服务端 `resetsAt - 10080 分钟` 到 `resetsAt` 的当前 reset cycle，不是滚动过去 7 天，也不是按星期一等日历边界定义的自然周；
- task/turn 额度贡献是 `estimated`，不是服务端账单；
- 同时显示 exact/partial 的本地可观察 token 占比（`TOKEN%`）与基于当前 `codex` gauge 的 estimated quota；实体行以 `~` / `-` 表达估算可用性，估算方法、external activity risk 与 partial 状态在 scope 摘要中统一展示；
- 所有任务结束后可以把 `settled` 标为 true，但 estimated quota 仍保持 Low，任何结果都不能标为 exact quota；
- 5 小时和周窗口都按 `resetsAt - windowDurationMins` 计算，不使用简单的 `now - duration`。

## 3. v0.1 功能需求

### FR-1 近期任务状态

TUI Overview 显示近期 tasks：

- 状态使用轻量行背景色及单字符标记表达，Recent tasks 和 Turns 不单独占用状态列；
- Tasks 面板底部始终显示统一状态图例，Turns 收起时也必须可见；
- `live`、`exact`、`inferred`、`stale` 等证据和置信度；
- task token 总量；
- 当前选中 5h/Week reset cycle 的 `TOKEN5H%` / `TOKENWK%` 与 estimated quota；
- project、来源、turn 数和会话标题；会话标题优先取 `$CODEX_HOME/session_index.jsonl` 中 thread 最新的非空 `thread_name`，缺失或不可读时回退 rollout 首条消息摘要，`--redact-content` 始终显示 `[redacted]`；
- 默认使用 Flat 列表；`R` 与可点击 `[R]Tree` 在 Flat/Tree 间切换。Tree 仅把拥有可靠直接父 thread id 的 subagent 挂到当前过滤结果中可见的父节点下，支持多层关系；父节点缺失或被过滤时 child 作为根节点，损坏的自引用/循环关系不得卡死渲染。拥有子节点的行显示稳定宽度的 `[-]` / `[+]` 按钮；Tasks 聚焦时 `-` 收起、`+` 展开所选父节点，整块按钮可点击且不得误选相邻行。Tree 模式提供大写 `E` 与可点击 `[E]Collapse`，一次收起当前文本和来源条件形成的完整过滤树中所有父节点，包含已隐藏在折叠祖先下的嵌套父节点；当这些父节点已全部收起时，固定宽度按钮改为 `[E]Expand`，相同操作展开当前过滤树的全部父节点。筛选范围外的折叠状态不得被批量切换。折叠状态按 thread id 跨刷新保留，选择不得停留在被折叠隐藏的后代。节点折叠时，父行的 token breakdown、所选 reset cycle `TOKEN%`、`EST.Q` 与 `API EQ.` 必须汇总当前过滤树内所有隐藏后代；`EST.Q` 使用同一 Codex credit-rate 加权分母，`API EQ.` 使用同一 API 价格目录，Spark 后代不得进入 `TOKEN%` 或 `EST.Q`。展开后恢复逐会话值，不能改写 `Snapshot` 或在父子同时可见时重复计数。挂在可见父节点下的 child 省略与父节点重复的项目名，orphan root 仍显示项目名。树根与同层分支按包含隐藏后代在内的子树最新活动项排序，让新活动 child 带动整组到顶部；
- Recent tasks 顶栏提供 task 标题/项目名搜索和 All、Desktop、Subagent、CLI 互斥来源筛选；当前会话标题或 `cwd` basename 使用大小写不敏感的子串匹配，两类条件按 AND 组合，历史 `vscode` 标签归入 Desktop，其他未知来源只属于 All。`F` 与来源按钮的 `A` / `D` / `S` / `C` 必须对应真实快捷键，并按 btop 风格只强调快捷键字符；筛选只属于 TUI 状态，不得修改 `Snapshot` 或一次性输出；
- 默认焦点在 Tasks；`Enter` 将焦点移入所选 task 的 Turns，`Backspace` 返回 Tasks，上下方向键与 `j` / `k` 只移动当前焦点面板的选择。当前焦点使用醒目标记，非焦点面板保留弱上下文标记；Tasks/Turns 标题在对应跳转动作可用时分别以轻量 `↵` / `←` 提示，两个提示整体都是鼠标按钮，且出现或消失不得移动相邻控件；无匹配 task 或无 turn 时不得进入 Turns 或显示旧详情；
- Turns 维护独立于 Tasks 的大小写不敏感 Filter，可匹配 turn ID、model、reasoning effort、消息摘要、状态与 `fast`；筛选后的键盘选择、鼠标选择、详情、滚动条和跨刷新 ID 恢复必须使用同一投影。筛选编辑确认后，非输入状态下的 `Delete` 与可点击 `[Del]` 清空当前焦点面板的查询；从 Turns 切换回 Tasks 时自动清空 Turns 查询及编辑恢复状态、重置 Turns 筛选投影，同时保留 Tasks 查询；
- `V` 切换 Turns 默认显隐。默认显示时 Turns 常显；默认隐藏时 Tasks 使用完整内容区域，`Enter` / `↵` 临时展开并聚焦 Turns，`Backspace` / `←` 返回 Tasks 后关闭临时面板；
- rollout 中 `thread_settings_applied.thread_settings.service_tier` 为 `fast` 或兼容的 `priority` 时，在下一次 turn 激活时快照为 Fast；TUI 只为 Fast turn 在模型名称后增加醒目标识，普通 turn 保持原显示；
- 选中 task 的近期 turns、模型、推理强度、最多 72 字符的用户消息摘要、状态和 token；旧日志缺失强度时显示 unknown，不在 task 层臆造单值；选中 turn 后以响应式详情显示时长、总量与所选 5h/Week reset cycle 的 token breakdown，并在空间允许时补充起止时间、占比、带 `~`/`-` 的 EST 和 turn ID，不回读或展示完整消息正文。

独立启动的监控进程不能读取其他 Codex runtime 的精确等待状态。未闭合 turn 只根据事件与文件新鲜度标为 `inferred running`，超过宽限期标为 `stale`；不得把 `notLoaded` 当成 completed。

### FR-2 额度窗口

TUI 和 CLI 显示所有 App Server 返回的额度桶：

- used/remaining percent；
- reset time；
- window duration 与 label；
- limit id；
- server/stale provenance；
- 可用时包含 plan、credits 和 reached 状态的 JSON 字段。
- 可选的 `rateLimitResetCredits.availableCount` 重置机会数；缺失或 `null` 表示 unavailable，明确的 `0` 必须保留，不得用可能被截断的 credit 详情数量代替；
- 可选的 `rateLimitResetCredits.credits` 明细：字段缺失或 `null` 表示只知道数量，`[]` 表示明细已获取且为空；旧 summary 缺少该字段时必须兼容为 unknown details；
- 每条明细保留原始字符串 `status` / `resetType`、可选 `title` / `description`、`grantedAt` 和可空 `expiresAt`。JSON 时间使用 RFC 3339，`expiresAt: null` 保持 null，TUI/text 显示 `never`；服务端不透明 id 不进入 Snapshot 或输出；
- 服务端允许截断详情数组。`credits.len() < availableCount` 时显示中性的 `SHOWING n/N`，不得据此把权威数量改成 n，也不得仅因正常截断把数据称为错误。

单条 reset-credit 明细非法时只丢弃该行，保留 count 和其他有效行，并将 Limits section 标为 partial、保留 warning；汇总容器或 count 非法时可丢弃 reset-credit 汇总，但不得丢弃同一响应里已经解析成功的额度窗口，也不得污染 Tasks/Turns/Models 的归因完整性。整个账户刷新失败时缓存的 count 和明细一起保留并标为 stale；成功响应明确返回 summary、`credits: null` 或 `credits: []` 时使用 fresh 值，不回填旧 summary 或旧明细。

同一 duration 的账户百分比不得跨桶混合。TUI/CLI 继续显示 App Server 返回的全部额度 gauge，TUI Other 还必须逐项显示这些窗口的 reset 数据，但 task/turn/model 归因只使用规范化 `limit_id == codex` 的当前窗口。模型名去除首尾空白后与 `gpt-5.3-codex-spark` 大小写不敏感精确相等的调用必须从归因分母和实体结果中排除；其他调用（包括缺失/空模型名）进入普通 `codex` 分母。`codex_bengalfox` 和其他桶只显示 gauge/Other 信息，不生成 `windowAnalyses` 或 estimated quota。

App Server 失败时仍显示本地 tasks/tokens，并按 `limit_id` 分别使用 rollout 最近快照提供 stale quota，不能因为另一桶快照时间更新就丢弃 `codex` 或 `codex_bengalfox`。过期窗口不能进入当前窗口归因。

### FR-3 当前窗口归因

按以下层级聚合：

```text
task/thread -> turn -> model token events
```

每级至少提供：

- `tokenUsage` 与 `windowTokenUsage`；
- 本地可观察 token 占比（`TOKEN%`）；
- estimated quota percent；
- quota confidence。

工具同时构建普通 `codex` 的当前 5 小时和周 reset cycle 分析；同一 duration 最多一个归因分析。每个 `windowAnalyses` 项必须独立携带 `partial` 和 `partialReasons`，不能用 Week 的不完整状态污染完整的 5h。周周期的 `TOKENWK%` 在扫描完整覆盖周期起点且没有截断、坏行、不可读文件或歧义 counter reset 时是本地可观察范围内的精确 token 占比；额度百分点归因仍然只能标为 estimated。`--days` 小于覆盖所选 reset cycle 所需的范围时，只将对应窗口分析标为 partial，即使已扫描到的文件本身都可读。

估算规则：

- 只选择当前普通 `codex` 窗口，并按该窗口的 `resetsAt - windowDurationMins` 边界筛选本地调用；
- `local_share_percent = entity_non_spark_tokens / all_local_non_spark_tokens * 100`；
- EST 使用 OpenAI 当前的 [Codex token-based rate card](https://learn.chatgpt.com/docs/pricing)；Standard `(input, cached input, output)` credits / 1M tokens 分别为：`gpt-5.6`（Sol 别名）、`gpt-5.6-sol` 与 Daybreak Blue 的 `daybreak-blue-latest` `(100,10,500)`，`gpt-5.6-terra` `(50,5,300)`，`gpt-5.6-luna` `(5,.5,30)`，`gpt-5.5` `(125,12.5,750)`，Daybreak Red 的 `daybreak-red-latest`、`gpt-5.6-cyber` 与历史兼容 slug `gpt-5.5-cyber` `(312.5,31.25,1875)`，`gpt-5.4` `(62.5,6.25,375)`，`gpt-5.4-mini` `(18.75,1.875,113)`；该映射包含 2026-08-21 的 Sol 调价，官方说明其促销费率至少持续到 2026-11-21；
- 当前官方费率卡不再列出 GPT-5.3-Codex/GPT-5.2；为读取历史 rollout，`gpt-5.3-codex`、`gpt-5.2` 与历史 `gpt-5.2-codex` slug 仍保留早期 `(43.75,4.375,350)` Standard 兼容权重，不得把它们描述为当前官方费率卡行；
- `serviceTier=fast` 与本地登录态 rollout 的兼容 `serviceTier=priority` 值都按 ChatGPT Fast 识别；根据官方 [Speed](https://learn.chatgpt.com/docs/agent-configuration/speed)，GPT-5.6/GPT-5.5 family 应用 `2.5x` Standard credit 倍率，GPT-5.4 family 应用 `2x`，GPT-5.3-Codex/GPT-5.2 不在支持范围时保留 Standard；其他 service tier 使用 Standard。这里的 `priority` 兼容行为不得解释为 API Priority 计费，后者在官方文档中是独立费率；
- `cached = min(cached_input, input)`，`uncached = input - cached`；基础 EST 始终使用 Codex token-based credit rate，不自动套用 API 长上下文倍率。TUI 提供默认关闭的 `[L]EST Longx` 可选口径：对 GPT-5.6 Sol/Terra/Luna/Cyber（含当前 aliases）、GPT-5.5 和 GPT-5.4，只有与安全累计 delta 完全相等的单次 `last_token_usage.input_tokens > 272000` 时，才把 API 公布的 input/cached input `2x` 与 output `1.5x` 代理倍率应用到整次请求。判断必须逐调用进行，不能使用 turn/thread/bucket 累计值；可选口径开启且单次边界未知、聚合 input 超过阈值时保留基础费率并标 `long_context_usage_unknown`，关闭时不得仅因该假设标 partial；聚合不超过阈值可安全按短上下文处理。GPT-5.4 mini、旧 `gpt-5.5-cyber` 与 GPT-5.3/GPT-5.2 兼容映射，以及未知模型 Luna fallback 不得凭推断套用长上下文倍率；Codex credit 卡未公布相同公式，输出不得称为官方逐请求 credit 账单；
- reasoning 是 output 子集，cache-write 是 input 子集，两者都不得重复相加。rollout 必须保留 snake/camel case 的 cache-write 字段并用于累计 delta 与单次请求一致性校验；当前 Codex credit 卡没有 cache-write 行，因此 credit 代理不得额外增加 API cache-write charge；
- 缺失或未映射的非 Spark 模型按 `gpt-5.6-luna` 对应 Standard/Fast credit 费率降级，并增加兼容的 `unpriced_model_rate_fallback` partial reason，不得从未知模型后缀猜测基础模型或从分母中静默删除；
- `estimated_quota_percent = codex_used_percent * entity_credit_units / all_credit_units`；task、turn、model 的 EST 使用同一 credit-rate 分母，`TOKEN%` 使用同一原始 token 分母；所有可用 EST 在数据模型/JSON 中保持 Low，TUI/text 仅以 `~` 表示近似，不显示独立 quota-confidence 标签或列；
- `gpt-5.3-codex-spark` 的公开费率仍是 research preview，继续按精确模型名排除；不得为 Spark 虚构 credit 值；
- token-based 双口径映射使用 estimator revision 5，历史 metric revision 3；每个新观察必须同时持久化基础 credit units 与可选 API 长上下文 extra，recorder 不得因 TUI 开关改变写入内容。不得假定任意持久化聚合都能直接重新定价，仅从仍处于配置扫描范围内的 rollout 调用重建重叠的本地桶/周数据点，并在 revision 5 新点的未加权 token/call/cache-write 证据不差于旧点时由 revision-aware upsert 替换。已发布的 revision 3 基础历史必须保留，但在重建前开启可选口径时标 `api_long_context_history_unavailable`；无法拆分基础与附加值的开发版 revision 4 历史必须丢弃。其他无法重建的旧 revision 继续隔离，混合 revision 不得合并 EST，必须让 `~EST` unavailable 并标记 `estimator_revision_changed` partial reason；
- Help Center 所述少量仍使用 legacy rate card 的 Enterprise workspace 无法从本地 rollout 自动识别；对这些 workspace，EST 不得声称代表其适用计费卡；
- 每个 scope 的摘要统一说明估算方法、`external activity possible` 与 partial 状态；partial 时列出 `partialReasons`。partial、lookback 不完整或 stale 不得清空仍可由当前 gauge 与本地分母计算的 EST；
- 没有当前 `codex` 窗口或没有本地非 Spark token 分母时显示 unavailable/`-`，不得把未知表达成 `0.0%`；
- 不得把 credit-rate 代理称为服务端逐 task/turn 账单；
- 服务端不提供当前 300 分钟窗口时，不得用周窗口冒充 5 小时归因；Models 面板必须显示明确的 unavailable 原因，不能只留下空表或把不可用表达成零模型使用量。
- 任何 scope 不可用时不得回退到另一 duration 冒充；旧顶层 window 字段固定兼容首选 5h 结果，周结果只进入多窗口分析结构。

### FR-4 Task 与 Turn Token

- task/turn token 从单调累计计数计算；
- 完全重复的 `token_count` 不重复计数；
- counter 回退只重建 baseline、不重复计算歧义样本，并报告 warning/partial；
- 嵌套 turn 完成后恢复仍在执行的父 turn；
- task 完成后迟到的最终 token 仍归入刚完成的 turn；
- 用户消息有显式 `turn_id` 时按其归属；缺失时只归入当前 active turn，没有 active turn 时不猜测归属；
- subagent rollout 内嵌的 parent 历史不得重复计入 parent 或 child；
- 当前结构化的普通 ThreadSpawn subagent 可从经过 provenance gate 的 settings snapshot 还原 service tier，但必须同时满足 metadata 明确为 `agent_role=null` 且 child model 完全匹配；较新 snapshot 覆盖旧状态，并且只有在这条路径中省略或 null tier 才规范化为 API `default`。旧版、自定义 role、model mismatch 及其他缺 tier 记录不得猜价；唯一策略例外是 `codex-auto-review` 的 `API EQ.` 固定使用 `gpt-5.6-luna` 价格 profile，缺失 tier 按 Standard，保留原始 model label，并加入 `api_price_codex_auto_review_luna_proxy`；
- TUI 显示 total，JSON 同时显示 input、cached input、output、reasoning output 和 total。

### FR-5 一次性输出

默认命令启动 TUI：

```bash
codex-usage-monit
codex-usage-monit --theme light
```

TUI 首次启动使用 dark 主题，之后恢复用户级状态文件中的主题；`--theme light` 显式覆盖保存值，`bright` 是 `light` 的别名，运行中按 `t` 可切换。主题只影响 TUI 渲染，不得改变采集结果或一次性 text/JSON 输出。

TUI 必须在用户级状态目录保存稳定菜单偏好，包括主题、顶层视图、5h/Week、默认关闭的 API 长上下文倍率、Turns/Models 显隐、Tasks/Turns/Models 共用的 table columns、Flat/Tree 与 task 来源筛选。搜索、选择、Settings 当前行、滚动位置、临时 Turns 展开和具体 thread 折叠集合不得持久化；字段缺失时使用各自安全默认值（Longx 关闭，Turns/Models 与四个指标列开启），状态损坏时回退完整默认状态。一次性输出不得读写此文件，默认保持基础 EST 口径；显式 `--long-context` 只为本次命令选择可选 Longx 投影，不得改变 TUI 偏好或 recorder 数据。

一次性输出：

```bash
codex-usage-monit snapshot [--format text|json] [--compact] [--long-context] [--section ...]
codex-usage-monit limits [--format text|json] [--compact] [--long-context]
codex-usage-monit tasks [--format text|json] [--compact] [--long-context]
codex-usage-monit turns [--thread <id>] [--format text|json] [--compact] [--long-context]
codex-usage-monit models [--format text|json] [--compact] [--long-context]
codex-usage-monit attribution [--format text|json] [--compact] [--long-context]
codex-usage-monit windows [--format text|json] [--compact] [--long-context]
codex-usage-monit summary [--range cycle|7d|30d] [--grain 1d|12h|6h|3h|1h] [--metric tokens|estimated|api-equivalent] [--long-context] [--format text|json] [--compact] [--history-dir <DIR>]
codex-usage-monit trends [--day-offset 0..7] [--long-context] [--format text|json] [--compact] [--history-dir <DIR>]
codex-usage-monit health [--format text|json] [--compact] [--history-dir <DIR>]
codex-usage-monit service status [--format text|json] [--compact]
```

TUI 与 snapshot 系列 CLI 使用同一 `Snapshot`；`summary` 与 `trends` 必须分别使用与 TUI Summary/Trends 相同的共享报告构建逻辑，不能复制一套独立聚合。`windows` 输出当前可分析的普通 `codex` 5h/Week reset cycles；`snapshot --section windows` 输出 `windowAnalyses`。默认 JSON 顶层包含 schemaVersion、asOf、partial、所请求 sections，以及 partial 时的来源和错误原因。原有 task/turn 顶层 5h `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence` 以及顶层 `models`、`attribution` 和旧 attribution 汇总字段保持 schema 兼容，始终表示首选 5h 分析，不得因 TUI 选择 Week 而改变语义；`--long-context` 只切换这套 5h 估算的投影并写入 `estimateProjection`，不得改变原始 token 或 API 等价费用。schema v2 新增 `apiPricing` 与窗口/实体 `apiEquivalentCost`，并继续序列化现有 `statusConfidence`、各层 `quotaConfidence` 与 attribution `confidence`，字段名和枚举值不变。Turns 的 text 输出包含消息摘要，JSON 使用 `messagePreview` 字段。

`summary` JSON 必须稳定包含查询条件、UTC window、所选指标、可加总 totals、Complete/Partial/Missing coverage、`valueIsLowerBound`、partial reasons、本地墙上时间 chart buckets、逐项目稀疏 buckets，以及 project/session/turn 层级；本地时间转换必须按每个时间戳的真实 offset 处理 DST。`valueIsLowerBound` 只表示所选 totals 确实遗漏了非负贡献（包括缺失覆盖、未归属用量、所选 EST/Longx/API 成分缺失），不得仅因 fallback window、只读状态、其他指标 warning 或 chart 首尾桶被查询范围裁切而置为 true。Summary 的精确 `u128` 指标/值和 Trends 的精确 token readout 必须使用十进制字符串，不能依赖 JSON number 的 JavaScript 安全整数范围。`trends` JSON 必须包含 5h/Week 剩余额度、周 token/EST、15 分钟 token/EST、准确 readout/interval、所选 24 小时 bounds 和 history diagnostics。`health` 必须统一包含 snapshot、history、recorder、service 及其读取错误；`service status --format json` 必须输出结构化服务状态和 `heartbeatRecent`。pretty/compact 只能改变空白，不能改变字段或过滤数据。

首次请求覆盖不完整的 `summary --range 30d` 时，CLI 必须复用 TUI 的 namespace-scoped 回填策略，在输出前执行一次 local-only、31 天、扩大文件上限的扫描；部分尝试不得每次命令重复，覆盖仍不完整时七天后才重新具备自动尝试资格。无法回填的桶和 totals 必须继续明确标记 partial/lower-bound，不能把缺失当作零。

一次性 Summary/Trends 收集必须先将当前 observation 暂存在内存，再尝试落盘，并从“持久化历史 + 未落盘 staged observation”构建报告。历史目录不可写、包含未来格式或单次写入失败时，报告必须保留本次已成功采集的数据和脱敏语义，同时以 warning/read-only/partial 暴露持久化故障；不得因无法落盘而把当前数据丢弃或误报为无可用数据。

退出码：`0` 完整、`1` 所请求报告没有可用数据、`2` 有可用数据但 partial、`64` 参数错误。局部命令只根据与所请求 section 相关的数据源决定 partial；Summary coverage 为 Missing 或 Trends 没有任何观察时返回 `1`，统一 `health` 正常报告只返回 `0` 或 `2`。

## 4. TUI 信息架构

### Overview

- quota gauges；
- Overview 最顶栏中的 `[V]Turns`、`[M]Models`、`[5h]` / `[Week]`、`[L]EST Longx` 按钮；`V`、`M`、`5` / `W`、`L` 是真实快捷键，整块按钮支持鼠标左键点击，并遵循 btop 风格快捷键字符强调规则；紧凑布局可缩为 `[L]`，但不得删除快捷键或 hitbox；
- 所选当前 reset cycle 的 duration、实际起止时间和额度；
- 使用所选 scope 的 task 表；
- 选中 task 的 turns，包含消息摘要、可点击选中态和 turn 详情；
- Models 面板先显示所选 `codex` scope 的当前 gauge、本地非 Spark token、带 `~`/`-` 的 EST，以及方法、external activity 与具体 partial reasons，再显示按 token 降序且不含独立 confidence 列的模型表；空间不足时标记 `top N/M`，并区分“scope 不可用”和“窗口内没有本地非 Spark 模型调用”；
- `M` 与最顶栏中可点击的 `[M]Models` 切换 Models 面板显隐；隐藏后 Tasks/Turns 使用释放的空间，顶栏恢复控件始终可达；
- `V` 与最顶栏中可点击的 `[V]Turns` 切换 Turns 默认显隐；首次启动默认显示，隐藏后仍可临时进入 Turns，顶栏恢复控件始终可达；
- 非搜索状态的 `Esc` 打开退出确认弹窗，弹窗内 `Enter` 确认、`Esc` 取消；`Ctrl-C` 与 `q` 保持直接退出，搜索输入中的 `Esc` 只取消编辑。

Overview 的 scope 切换同步更新 Tasks、Turns、Models 及其中的归因摘要；`[L]EST Longx` 同步切换这些实体 EST 与 Trends 的 Weekly/15m `~EST` 图，但不改变原始 token 图或 API 等价费用。Tasks 与 Turns 的每一行分别显示所选 5h/Week scope 的独立 `API EQ.`，不是 lifetime 金额；窗口缺失时显示 `-`。scope、Longx、面板和列项开关都作为稳定菜单偏好跨 TUI 进程保存；搜索输入焦点必须先消费可打印字符，非 Overview 视图不得误触发 Overview 快捷键。既有 JSON 的顶层 task/turn、`models` 与 `attribution` 字段继续固定表示首选 5h 分析，默认使用基础投影，显式 CLI `--long-context` 可选择 Longx；CLI/JSON attribution 能力不因独立 TUI 面板删除而改变。

### Settings

- `4` 打开 Settings；Display 管理主题、Turns/Models 显隐和 `EST Longx`；Table columns 全局管理 Tasks、Turns、Models 的 Tokens、Token share、Estimated quota 与 API equivalent 列；
- 每行显示并强调真实快捷键，支持上下选择、`Enter` 切换和整行鼠标点击；文本输入焦点仍优先消费可打印键；
- task/model 身份列与 turn 的 model/effort/message 列不可关闭。面板过窄时可以临时省略低优先级的已启用指标列，但不得改写保存的列配置；隐藏 Tokens 后，task/turn 状态标识必须迁移到身份列而不能消失。

### Other

- 数据源状态；
- scanned/discovered/truncated/unreadable 文件数；
- parsed/skipped 行数与 ambiguous token reset 数；
- active/completed/uncertain task 数；
- Resets 分组使用同一张 `ITEM / STATE / GRANTED / RESET TIME (LOCAL)` 表：先列重置机会明细，再按 bucket 展开每个 primary/secondary 窗口；窗口行显示 limit id、slot、duration，宽布局额外显示 used percent，`GRANTED` 为 `-`；
- `RESET TIME` 固定使用本地 `YYYY-MM-DD HH:MM:SS ±HH:MM`，窗口缺少 `resetsAt` 时明确显示 `unavailable`，不得推算；机会 `expiresAt: null` 显示 `never`。宽度小于 80 时机会的 `GRANTED` 可缩短为本地 `MM-DD HH:MM`，其他宽度使用完整本地时间和 offset；
- Resets 标题显示 `availableCount` 对应的权威可用数及 server/stale provenance；缺值显示 `credits unavailable`，`0` 显示 `0 available`，明细未知显示 `DETAILS UNAVAILABLE`，正常截断显示 `SHOWING n/N`，解析异常另标 `PARTIAL`；
- partial 与 diagnostics。

宽度小于 100 列时 task/turn 区域改为上下布局。Recent tasks 的 Tree、名称搜索和来源按钮嵌入面板顶边，不额外占用窄终端数据行。Flat/Tree 的显示、点击、滚动和键盘导航均把过滤后的位置映射回 `snapshot.tasks` 绝对索引；切换模式保留所选 thread/turn 并 reveal 新位置，刷新时按 `thread_id` / `turn_id` 保留仍符合筛选的选择。Recent tasks viewport 位于偏移 `0` 时进入跟随顶部模式，新建或更新的 task/subagent 插到排序顶部后必须立即可见；用户向下滚动后则继续按刷新前的首行 task 保留阅读位置，直到再次滚回顶部。

Turns 可分页滚动；Recent tasks 和 Turns 以轻量背景色及单字符标记区分状态，Tasks 底部始终提供统一图例，空间足够时同一底边保留选中 task 的状态证据。Overview、Trends、Summary、Other、Settings 五个顶层 tab 均可用鼠标左键切换；Overview scope 切换不得清空 task/turn 搜索、来源筛选、焦点和仍存在于新 scope 的 ID 选择。Overview 中可点击 Tasks/Turns 数据行并切换键盘焦点，Settings 每个设置行整体可点击。除显式视图 tab、当前视图的真实控件、数据行和滚动条外，标题、边框、表头和空白区不得触发选择。`Enter` / `Backspace` 在 Tasks 与 Turns 之间移动焦点，上下键只改变当前焦点面板的选择；Settings 中上下键选择行、`Enter` 切换。参考 btop 的面板路由语义，滚轮只滚动鼠标所在的 Tasks 或 Turns viewport，每格 3 行，不改变选择或键盘焦点；内容超出 viewport 时在右边框显示比例 thumb，点击轨道可跳转、按住左键可拖动，释放后停止拖动，且均不得改变当前数据行选择。dark/light 主题均须保持状态、选中项、额度和 diagnostics 可辨识，按 `t` 即时切换。

## 5. 非功能需求

### 性能

- 默认最近 7 天、最多 500 文件；
- TUI 维护进程内文件指纹缓存；
- 无变化刷新不得重新读取 rollout 或 session title JSONL，只允许检查文件元数据指纹；
- 单文件变化只重读该文件，再用缓存事件重建全局累计状态；
- Running/Stale 必须即使在缓存命中时仍按当前时间重算。

### 隐私与安全

- Codex 数据源只读；
- 不调用 `account/rateLimitResetCredit/consume` 或其他会消耗重置机会的写接口；
- 不读取、复制或缓存 `auth.json`，包括 reset-credit 明细采集；认证完全交给 App Server；
- 可在进程内缓存 `session_index.jsonl` 的当前会话标题；索引缺失时只保留最多 96 字符的首条用户消息回退标题；
- 不保存完整 prompt、assistant、reasoning 或工具内容；
- `--redact-content` 禁用 task 标题预览和 turn 消息摘要；
- 未识别字段容忍，坏行、不可读文件与累计 token 回退进入 partial；
- TUI/text 输出清洗终端控制字符，JSON 使用标准转义；
- 不向第三方发送本地数据。

## 6. v0.1 验收

- synthetic parser、app-server、归因、输出和 TUI 测试通过；
- 真实 rollout 中 task token 与 turn token 汇总一致；
- 真实 subagent parent replay 不重复计数；
- 真实 80x24 与 120x40 TUI 中筛选控件、Tasks/Turns 焦点与键鼠交互不重叠、不崩溃，并能恢复终端；
- 多额度桶读取成功；
- 可用重置机会的正数、`0`、unavailable 与 stale 状态显示正确；明细 null/空/截断、`grantedAt`、具体 reset time、never、未知状态和非法单行降级均有覆盖；
- 235 文件真实基准 warm refresh 小于 200ms；
- partial 与退出码按 section 生效；
- 5h/Week 使用服务端 reset cycle 边界；`--days` 覆盖不足时对应 `windowAnalyses` 为 partial；
- Overview 最顶栏中的 `5` / `W` 和鼠标按钮会同步切换 Tasks、Turns、Models 与归因摘要，`V` / `[V]Turns` 和 `M` / `[M]Models` 可稳定隐藏并恢复对应面板；`L` / `[L]EST Longx` 默认关闭，键盘和整块鼠标 hitbox 都能同步切换 Overview 与 Trends 的 EST 口径，搜索焦点和其他视图不会误触发；
- Tasks/Turns 的独立 `API EQ.` 会随 5h/Week scope 切换、不随 Longx 改变，并区分窗口缺失 `-` 与已观察零值；`4 Settings`、全部设置行快捷键、整行鼠标命中、非默认列配置持久化、隐藏 Tokens 后状态标识以及 60x24 紧凑布局均有覆盖；
- Tree 节点收起时父行准确汇总当前过滤树中隐藏后代的 token、`TOKEN%` 与 estimated quota，展开时不重复计数；
- `serviceTier=fast` / `priority` 的 Fast turn，其 `FAST` 只出现在模型名称后；普通 turn 的模型单元格保持不变；
- 相同 duration 出现多个额度桶时仍完整显示 gauge，但只有 `codex` 生成归因；Spark 精确模型名大小写不敏感排除，`codex_bengalfox` 保持 gauge-only，缺失模型名进入普通 `codex` 分母；
- `windows` 与 `snapshot --section windows` 输出多窗口分析，旧 5h 字段保持兼容；
- `summary` / `trends` 与 TUI 的共享报告在固定 snapshot/history fixture 下逐字段一致，覆盖 Longx、Summary 本地时间桶与 30 天回填、Trends day offset/readout，以及完整/partial/missing 退出码；
- `health` text/JSON 同时覆盖 snapshot、history、recorder、service 和读取错误，`service status --format json --compact` 输出稳定单行结构；
- idle 后额度估算仍不声称 exact。

## 7. 后续增强，不属于 v0.1

- 统一通过同一 App Server 启动 tasks，从而获得精确 waiting approval/input 状态；
- 模型、项目、时间等更多筛选与多种排序（task 名称/source 筛选已实现）；
- TUI token breakdown 显示模式切换；
- 跨进程持久化额度快照与索引；
- 非 Codex 产品桶或同 duration 歧义桶的显式高级归因选择；
- 安装包、Homebrew 与跨平台发布流水线。
