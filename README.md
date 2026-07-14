# Codex Usage Monitor

本地优先的 Codex 终端监控工具。默认启动实时 TUI，也能一次性输出 text/JSON 快照。

当前实现已在 `codex-cli 0.144.3` 与真实 `~/.codex` 数据上验证，提供：

- 近期 task/thread 状态，以轻量行背景色和图例表达状态，选中项保留证据来源与置信度；
- 选中 task 下的 turns、模型、推理强度、消息摘要和 token；
- 所有可用 5 小时、周及非标准额度桶；
- 当前 5 小时或周重置周期内 task、turn、model 的本地 token 占比；
- 基于当前 `codex` gauge 与模型/服务层短上下文价格加权用量占比的 Low-confidence 额度百分点估算；
- 完整或按 section 输出的一次性 text/JSON 快照；
- 从 TUI 把选中的已停止 root task 恢复到当前 Zellij session 的新 Codex CLI pane；
- 文件指纹缓存，TUI 刷新只重读变化的 rollout。

## 精度边界

Task、turn 和模型的 token 来自 Codex 累计计数的单调增量；在扫描完整、日志未缺失且累计计数没有回退时，这是本地可观察范围内的精确值。累计计数回退时，工具不会把新的较小基线重复算作消费，而会跳过该歧义样本并将 `ambiguousTokenResets`、数据源和快照标为 partial。

5 小时和周额度的账户总百分比来自 `codex app-server`。周窗口指服务端当前 reset cycle：从 `resetsAt - 10080 分钟` 到 `resetsAt`，不是滚动的过去 7 天，也不一定是自然周。Codex 不提供 task/turn 级官方配额账单，因此额度归因始终是估算。当前实现只分析普通 `codex` 桶；`LOCAL` 继续使用原始 token 占比，而 `EST` 按 [OpenAI 短上下文 Standard/Priority 价格](https://developers.openai.com/api/docs/pricing?latest-pricing=priority)分别计算 uncached input、cached input 与 output 的相对成本：`entity_estimated_quota_percent = codex_used_percent * entity_price_units / all_price_units`。`serviceTier=priority` 对应 Fast 价格，其他服务层按 Standard 价格。

模型名去除首尾空白后与 `gpt-5.3-codex-spark` 大小写不敏感精确相等的调用不进入原始或价格分母，也不生成 task/turn/model EST；`codex_bengalfox` 仍显示账户 gauge 和 Data Health 信息，但不生成 `windowAnalyses` 或实体归因。价目表覆盖 `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`、`gpt-5.5`、`gpt-5.4`、`gpt-5.4-mini`；缺失或未定价模型按 Luna 的对应 Standard/Fast 价格降级，并把窗口标记为 `unpriced_model_rate_fallback` partial。rollout 没有 cache-write token，无法应用 GPT-5.6 的独立 cache-write 价格；reasoning token 已包含在 output 中，不重复计费。完整费率与降级规则见 [数据能力边界](docs/codex-data-capabilities.md)。

TUI 的 `EST.Q5H` / `EST.QWK` 分别表示归因给该 task/turn 的估算 5 小时/周额度百分点，不是 token 占比；`LOCAL5H` / `LOCALWK` 才是本地 token share。所有可用 EST 都保持 Low confidence 并使用 `~` 前缀。扫描不完整、lookback 不足或状态 stale 时仍计算 EST，同时把对应分析标为 partial/stale；只有当前 scope 没有 `codex` 窗口或没有任何可作为分母的本地非 Spark token 时才显示 `-`。

5 小时和周周期的本地 token share 在整个周期日志扫描完整时可以精确结算。`--days` 控制本地 rollout 的扫描范围；若它没有覆盖所选周期起点，或受 `--max-files`、坏行、不可读文件、counter 回退影响，相应窗口分析必须标为 partial，不能把不完整分母称为精确占比。无论扫描是否完整，task/turn 级 quota share 都仍是 estimated。

即使所有任务都已结束，也只能得到精确 token 与更稳定的最终估算，不能变成 OpenAI 服务端意义上的精确任务额度账单。`settled=true` 不会把 EST 提高到 Medium 或 exact，估算仍保持 Low。

## 构建运行

```bash
cargo build --release
./target/release/codex-usage-monit
```

也可以安装到 Cargo bin 目录：

```bash
cargo install --path .
codex-usage-monit
```

默认扫描最近 7 天、最多 500 个 rollout。常用全局选项：

```bash
codex-usage-monit --days 2 --max-files 200
codex-usage-monit --offline
codex-usage-monit --redact-content
codex-usage-monit --no-rollout-cache
codex-usage-monit --codex-home /path/to/.codex
codex-usage-monit --theme light
```

`--offline` 不启动 App Server，额度改用 rollout 中每个 `limit_id` 最近的本地快照并标记为 stale/partial，避免离线时丢失账户 gauge；实体 EST 仍只使用其中的 `codex` 窗口。`--redact-content` 不保留 task 标题预览或 turn 消息摘要。首次启动 TUI 使用 `dark` 主题，之后恢复上次保存的主题；`--theme light` 显式覆盖已保存主题，`bright` 是 `light` 的别名。主题仅影响 TUI 渲染，不改变数据采集或一次性 text/JSON 输出。

## 冷启动诊断

`debug-startup` 执行与 TUI 相同的用户状态读取、冷 rollout 扫描、App Server 查询、Snapshot/App 构造，并用指定尺寸的 headless backend 完成一次真实首帧渲染，随后输出分阶段耗时而不进入 alternate screen：

```bash
codex-usage-monit debug-startup
codex-usage-monit --offline --redact-content debug-startup --format json
codex-usage-monit debug-startup --width 100 --height 30
```

普通 TUI 或一次性命令也可用 `--startup-log <FILE>` 开启同一套追踪。文件采用增量 JSONL；可能阻塞的 span（包括 `app_server.total`）会写 `stage_start` / `stage_finish`，`rollout.total`、`snapshot.total` 等回顾性汇总只写 `stage_finish`。每行都会立即 flush，因此启动卡住时最后一条 `stage_start` 能指出正在等待的部分。指定文件会在每次启动时重建，只保留最近一次运行：

```bash
codex-usage-monit --startup-log /tmp/codex-usage-startup.jsonl
codex-usage-monit --startup-log /tmp/codex-usage-startup.jsonl snapshot --section health
```

默认不开启 profiler，也不创建日志。trace header 另含绝对 `startedAt` 和进程 `pid`，阶段记录则只含阶段名、相对时间、文件/字节/行/task/turn 等聚合计数和状态；两者都不含 Codex home 路径、thread/turn ID、标题或消息内容。`rollout.parse_files` 用于判断 JSONL 解析成本，`rollout.reduce` / `rollout.materialize` 分别显示事件回放与实体构造，`app_server.initialize` / `app_server.account_reads` / `app_server.shutdown` 则拆分进程与 RPC 等待；`startup.ready` 表示首帧已经成功绘制。

CLI 默认使用用户级持久化 rollout 解析缓存，避免每次冷启动重新解析未变化的历史 JSONL。macOS 路径为 `~/Library/Caches/codex-usage-monit`，Linux 为 `$XDG_CACHE_HOME/codex-usage-monit`（未设置时为 `~/.cache/codex-usage-monit`），Windows 为 `%LOCALAPPDATA%\codex-usage-monit\cache`；`CODEX_USAGE_MONIT_CACHE_DIR` 可覆盖目录。`--no-rollout-cache` 禁用磁盘读写并恢复纯进程内缓存。缓存按 Codex home 和 redaction 模式隔离，每个 namespace 会尽力把磁盘分片收敛到 2,000 个和 512 MiB 以内，并清理超过 24 小时的中断写入临时文件；写入批次会在落盘前按当前字节预算腾出空间，而不是等整批结束后再清理。启动追踪中的 `rollout.cache_maintenance` / `rollout.cache_load` / `rollout.cache_save` 会报告维护、命中、缺失、损坏、写入、延后、退避和清理计数。

报告同时保留 `tui.bootstrap`、`snapshot.total`、`rollout.total` 等外层计时与其内部阶段，这些时长会重叠，不能逐行相加；成功的 TUI / `debug-startup` 以 `startup.ready` 为端到端总耗时，成功的一次性命令以 `startup.complete` 为准。缺少终点事件表示进程在完成前返回错误或被中断；一次性输出写入失败会显式记录 `startup.failed`。

完整在线刷新会把 `snapshot.local_scan` 与 `snapshot.account_fetch` 作为并发的 sibling span 调度，两者的起止区间可能重叠，不能相加。`--offline`、`limits` 的无本地扫描路径，以及复用缓存账户数据的路径不会额外创建采集线程。

## 一次性输出

```bash
codex-usage-monit snapshot
codex-usage-monit snapshot --format json --compact
codex-usage-monit snapshot --section limits,windows,tasks,turns,models,attribution,health
codex-usage-monit limits --format json
codex-usage-monit windows --format json
codex-usage-monit tasks --format text
codex-usage-monit turns --thread <thread-id> --format json
codex-usage-monit models --format json
codex-usage-monit attribution --format text
```

`limits` 优先走轻量 App Server 查询，不扫描 rollout；仅在额度读取失败时扫描本地日志降级。`windows` 输出可分析的当前 `codex` reset cycles；`snapshot --section windows` 在 JSON 中使用 `windowAnalyses` 字段。为兼容既有消费者，task/turn 上原有的 5h `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence`，以及顶层 `models` 和 `attribution`，继续表示首选 5h 分析，不改成周数据；旧 attribution 汇总字段也继续保留，但当前实体 EST 统一来自 `codex` gauge 与短上下文价格加权用量占比的乘积。`turns` 的 text 输出包含消息摘要，JSON 使用 `messagePreview` 字段；已知时 `serviceTier` 给出 turn 激活时的服务等级，其中 `priority` 对应 TUI 的 Fast。subagent task 已知直接父会话时，JSON 额外输出可选的 `parentThreadId`；task 的 `archived` 表示该 thread 只在 `archived_sessions` 中出现，同一 thread 仍有 active rollout 副本时为 `false`。JSON schema 当前为 v1，字段统一使用 camelCase。

退出码：

- `0`：所请求 sections 完整；
- `1`：无法生成有效结果；
- `2`：所请求 sections 有可用但不完整的数据；
- `64`：参数错误。

## TUI 操作

- `1` / `2`：Overview、Data Health；
- Overview 顶栏提供 `[V]Turns`、`[M]Models`、`[5h]` 与 `[Week]`；按钮均可用鼠标左键点击，Tasks、Turns、Models 及其中的归因摘要同步使用所选 reset cycle；
- `Tab`、左右方向键：切换视图；
- 默认键盘焦点在 Recent tasks；`j` / `k`、上下方向键选择当前焦点面板的数据行，`Home` / `End` 跳到首尾；
- `Enter`：从 Tasks 进入所选 task 的 Turns；`Backspace`：从 Turns 返回 Tasks；标题中的 `↵` / `←` 也可用鼠标点击；
- `O`：在 Tasks 焦点下打开 `[O]Open` 确认弹窗；确认后在当前 Zellij session 创建新 pane，并以同一 `CODEX_HOME` 执行 `codex resume --cd <cwd> <thread-id>`。弹窗中的 `Enter` / `[↵] Open` 确认，`Esc` / `[Esc] Cancel` 取消；终端空间不足以完整显示 cwd 等关键信息时会保留 Cancel 并禁用确认，避免误启动。Filter 正在输入时 `O` 仍优先写入查询。Open 恢复 thread 的最新状态，不会定位到选中的历史 turn，也不是附着到原 Desktop/CLI 进程。Running/Waiting、subagent、archived、缺少有效 cwd 或非 canonical UUID 的 task 会被阻止；当前进程已经登记的 pane 是例外，此时 `O` 只校验并聚焦该 pane，不创建第二个前端。若 pane 已被删除，映射会被清理，用户再次按 `O` 后才按最新 task 状态重新进入确认流程；
- `V`：切换 Turns 的默认显隐；首次启动默认显示，顶栏中的 `[V]Turns` 也可点击。默认隐藏时，`Enter` / `↵` 会临时展开 Turns，`Backspace` / `←` 返回 Tasks 时自动收起；
- `M`：切换 Models 面板显隐；最上方的 `[M]Models` 也可用鼠标点击；
- `R`：在 Recent tasks 的 Flat 与 Tree 视图间切换；标题中的 `[R]Tree` 也可用鼠标点击。Tree 中大写 `E` 或 `[E]Collapse` 收起当前过滤树的全部父会话；全部收起后同一按钮变为 `[E]Expand`，再次触发展开全部。选中拥有子节点的 task 后，`-` 收起、`+` 展开，行内固定宽度的 `[-]` / `[+]` 也可直接点击；
- `/` / `F`：编辑当前焦点面板自己的 Filter；编辑确认后可用 `Delete` 或标题右侧的 `[Del]` 清空当前面板查询；
- Filter 输入时可用左右方向键、`Home` / `End` 移动光标，`Backspace` / `Delete` 编辑，`Enter` / `Tab` 确认，`Esc` 取消本次编辑；
- `A` / `D` / `S` / `C`：直接切换 All、Desktop、Subagent、CLI 来源，`[` / `]` 循环切换；
- 鼠标左键：点击最上方视图 tab、顶栏筛选控件，或选择 Tasks / Turns 数据行并把键盘焦点切到该面板；右侧滚动条支持点击轨道和按住拖动；
- 鼠标滚轮：只滚动所在的 Tasks 或 Turns viewport，每格 3 行，不改变当前选择或键盘焦点；
- Recent tasks 位于顶部时会随刷新保持顶部，让新建或刚更新的 task/subagent 立即可见；向下滚动后则固定当前阅读位置，直到再次滚回顶部；
- `PageUp` / `PageDown`：滚动当前焦点所在的 Tasks 或 Turns viewport；
- `t`：在 dark 与 light 主题间切换；
- `q`、`Ctrl-C`：直接退出；非搜索状态的 `Esc` 打开退出确认，弹窗内 `Enter` 确认、`Esc` 取消；搜索输入状态中的 `Esc` 仍只取消本次编辑。

本地数据每 2 秒检查一次，账户额度每 45 秒刷新一次。进程内 `RolloutCache` 负责增量刷新，用户级持久化缓存则复用跨进程未变化文件的解析结果；首次运行、缓存缺失/损坏或源文件变化时仍会按需解析。活跃文件的成功缓存写入按路径合并到最多每 30 秒一次，I/O 失败会从 30 秒起指数退避到 15 分钟，均不阻塞当前 snapshot。冷启动成本主要取决于未命中 JSONL 的总字节数而不是文件数，少量包含长历史的 rollout 也可能很大。使用 `debug-startup` 查看当前机器与数据集的真实分解，不应把某个固定文件数的旧基准当作启动时延保证。
TUI 中的绝对时间使用系统本地时区；text 输出中带 `UTC` 后缀的时间保持 UTC。
Recent tasks 和 Turns 使用轻量背景色及单字符标记表示状态，Turns 面板底部显示统一状态图例，以减少状态列占用并兼容无色终端。Recent tasks 默认保持 Flat 排序；Tree 模式把已知直接父会话且父节点也在当前过滤结果中的 subagent 递归缩进到父节点下，缺失或被过滤的父节点不会被强行补回。拥有子节点的行显示可点击的 `[-]` / `[+]`，并可在 Tasks 焦点下用 `-` / `+` 收起或展开；大写 `E` 和可点击的 `[E]Collapse` 会一次收起当前过滤树的所有父节点，包括已藏在折叠祖先下的嵌套父节点；全部收起后按钮变为 `[E]Expand` 并展开同一过滤范围。折叠状态跨数据刷新保留。节点收起时，父行会汇总当前过滤树中所有被隐藏后代的 token，LOCAL 继续按原始 token 分母累加，EST 则累加短上下文价格加权后的实体值；Spark 后代继续从两者中排除。展开后各行恢复独立显示。挂在父节点下的 child 省略重复项目名，作为 orphan root 显示时仍保留项目名。树根和同层分支由最近活动的后代带动排序，因此新活动的 subagent 仍会把整组带到顶部。Recent tasks 最后一列优先显示 Codex 当前会话标题；标题来自 `session_index.jsonl` 中该 thread 最新的重命名记录，索引缺失时才回退到 rollout 首条消息摘要。顶栏按该标题或项目名（`cwd` basename）执行大小写不敏感的子串筛选，并可与 All、Desktop、Subagent、CLI 单选来源筛选组合使用；历史 `vscode` 标签归入 Desktop，其他未知来源只出现在 All。Turns 的独立 Filter 可匹配 turn ID、模型、推理强度、消息、状态以及 `fast`；非空筛选右侧的 `[Del]` 整块可点击，非输入状态下也可按 `Delete` 清空当前焦点面板的查询。焦点从 Turns 返回 Tasks 时会自动清空 Turns 查询并恢复完整 turn 列表，但 Tasks 查询保持不变。Codex 标记为 Fast 的 turn 会在模型名称后额外显示醒目的 `FAST`，普通 turn 的显示保持不变。醒目的 `▌` 表示当前键盘焦点，较弱的 `▏` 保留另一面板的上下文选择；筛选无结果时 Turns 不显示旧 task 数据。选中的 turn 会在表格下方显示详情：紧凑终端优先保留状态、时长、模型、推理强度和 token breakdown，空间允许时再显示起止时间、占比、置信度、turn ID 与本地保存的最多 72 字消息摘要。Overview 的 Models 面板显示所选 5h/Week `codex` gauge、本地非 Spark token、短上下文价格加权公式得到的 Low-confidence EST，以及按 token 降序的模型表；容量不足时标出 `top N/M`。当所选 scope 不可分析时，面板明确显示 unavailable，不会用另一时长的数据冒充；面板隐藏后仍可从最上方的 `[M]Models` 恢复。

TUI 会把稳定菜单偏好保存到用户级状态目录：macOS 为 `~/Library/Application Support/codex-usage-monit/tui-state.json`，Linux 为 `$XDG_STATE_HOME/codex-usage-monit/tui-state.json`（未设置时使用 `~/.local/state/...`）。`CODEX_USAGE_MONIT_STATE_DIR` 可覆盖目录。保存项包括主题、顶层视图、5h/Week、Turns/Models 显隐、Flat/Tree 和 task 来源筛选；搜索、选择、滚动位置及具体 thread 的折叠状态不会跨进程保存。一次性输出不会读取或写入该文件。

额度展示与额度归因是两条边界明确的路径：所有 App Server 桶都可以显示 gauge，但 `windowAnalyses`、Tasks、Turns 和 Models 的 EST 只选择当前普通 `codex` 窗口。`gpt-5.3-codex-spark`（忽略大小写、精确匹配）完全跳过归因，`codex_bengalfox` 只保留 gauge/Data Health。每个 `windowAnalyses` 独立给出 `partial` 和 `partialReasons`，所以 Week 扫描不完整不会污染完整的 5h 标记，也不会取消仍可计算的 Low estimate。

## Open 配置

TUI 启动时读取用户级 `open.json`；文件不存在时立即写入默认内容。`debug-startup` 使用同一初始化路径，一次性 `snapshot`、`limits` 等命令不读取或写入该文件。路径按以下优先级解析：

- 所有平台设置 `CODEX_USAGE_MONIT_CONFIG_DIR` 时使用 `<该目录>/open.json`；
- 否则，设置 `XDG_CONFIG_HOME` 时使用 `$XDG_CONFIG_HOME/codex-usage-monit/open.json`；
- 再否则，macOS 使用 `~/Library/Application Support/codex-usage-monit/open.json`，Windows 使用 `%LOCALAPPDATA%\codex-usage-monit\open.json`，其他 Unix 使用 `~/.config/codex-usage-monit/open.json`。

默认配置为：

```json
{
  "version": 1,
  "enabled": true,
  "backend": "zellij",
  "codexBin": null,
  "zellij": {
    "floating": true,
    "widthPercent": 90,
    "heightPercent": 90,
    "closeOnExit": false
  }
}
```

`codexBin: null` 表示从 monitor 进程的 `PATH` 解析 Codex；需要固定版本时建议配置绝对路径。`floating: false` 改为当前 tab 的 tiled pane；宽高必须在 `1..=100`，仅用于 floating pane。`closeOnExit: false` 会在 Codex 退出后保留 pane，便于查看错误。当前唯一 backend 是 `zellij`，且 monitor 必须已经运行在 Zellij 内。

配置只在 TUI 启动时加载，修改后需要重启 monitor。语法损坏、缺少 `version`、包含未知或拼错的字段、使用未来或其他不支持的版本、非法尺寸或读取失败时，工具不会覆盖原文件，而是禁用 Open，并在触发 `[O]Open` 时显示原因。Unix 上自动创建的目录和文件分别使用 `0700`、`0600` 权限。

## 数据与隐私

- 只读 `CODEX_HOME/sessions`、`CODEX_HOME/archived_sessions`、`CODEX_HOME/session_index.jsonl` 和 Codex App Server；
- 不读取 `auth.json`，认证完全由 App Server 管理；
- 监控采集与一次性输出不修改、恢复或终止 Codex task；只有用户在 TUI 中主动触发并确认 `[O]Open` 时，工具才会启动新的 `codex resume` CLI 前端。Open 不携带 prompt，不自动 unarchive、fork、checkout、stash、恢复 worktree，也不更改 sandbox/approval 配置；
- CLI 默认把解析后的 rollout 元数据、token 事件和有限长度的 task/turn 消息摘要写入上述用户级缓存目录；`--redact-content` 模式使用不含这些摘要的独立缓存且不会读取可见缓存，但不会删除之前生成的可见缓存。`--no-rollout-cache` 只禁用后续磁盘读写；需要擦除时可直接删除缓存目录。Unix 新建目录和文件分别使用 `0700`、`0600` 权限，缓存会自动重建；
- TUI 另行写入上述不含会话内容的菜单偏好文件，以及不含 thread、标题、cwd 或消息内容的 `open.json`；
- 进程内保留 `session_index.jsonl` 的当前会话标题；索引没有对应标题时，才回退到最多 96 字符的首条用户消息。每个 turn 最多保留 72 字符的用户消息摘要，不保存完整消息、reasoning 或工具内容；缺少显式 `turn_id` 时只归入当前 active turn，没有 active turn 则不猜测归属。
- 部分 subagent turn 没有明文 `user_message`，消息摘要会显示 `-`，不会从注入上下文反推。
- TUI 与 text 输出会剥离动态文本中的终端控制字符；JSON 使用标准转义。
- task 来源优先按 `originator` 归一化为 `desktop`/`cli`，子代理保持 `subagent`；缺失时才回退到底层 rollout `source`。

## 文档

- [现有开源工具调研](docs/existing-tools.md)
- [Codex 数据能力与边界](docs/codex-data-capabilities.md)
- [从任务列表恢复 Codex 终端会话](docs/codex-terminal-resume.md)
- [产品需求](docs/requirements.md)
- [实现路径](docs/implementation-plan.md)
