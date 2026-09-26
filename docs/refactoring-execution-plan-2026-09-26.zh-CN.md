# 项目瘦身与重构：修改方案及本地 Agent 执行说明

日期：2026-09-26  
仓库：`ghostroller/codex-usage-monit`  
依据：[`refactoring-review-2026-09-26.zh-CN.md`](refactoring-review-2026-09-26.zh-CN.md)  
审核修订提案：[`refactoring-proposal-2026-09-26.zh-CN.md`](refactoring-proposal-2026-09-26.zh-CN.md)

> **本文是可执行的任务拆解，不是全部路线的实施授权。**
>
> 用户下达“按本文件默认范围实施”时，执行 M0—M6：修订提案、明确的小范围机制替换、保持外部语义的应用层解耦，以及收尾验证。A 方案的产品切换/删除与 SQLite 原型、全面迁移均不在默认范围。不要因遇到这些未批准事项而停止其他独立任务。

## 1. 开工约定与范围

### 1.1 先读实际仓库，不照旧行号机械修改

依次读取当前适用的 `AGENTS.md` 和已配置的本机环境指令、配套审核意见、原提案，再检查实际 HEAD 的相关代码和测试。不要假设必须存在名为 `environment.local` 的文件，也不要为了本任务额外建设环境配置系统。

审查读取提交为 `abef359eac136a2b004c7e4b98890cee27bd37fe`；原提案代码基线为 `1c153229d5c167a0b02eca12fbc6ce4c4770e26f`。这些是比较基线，不是要求 checkout、reset 或还原到该版本。

记录开工 HEAD、工作区状态和实际工具链。保留用户未提交修改；不得使用破坏性清理、覆盖已有工作或重写历史来获得“干净基线”。若相关实现已更新，按实际代码调整任务，并记录已被解决、仍成立和不再适用的部分。

### 1.2 默认范围与决策门

| 工作 | 默认范围 | 限制 |
| --- | --- | --- |
| M0：复核、修订原提案、建立契约/调用清单 | 是 | 不把建议改成“产品已批准” |
| M1：用 `semver` 替换版本 parser | 是 | 升级身份、降级及发布策略保持 |
| M2：标准库锁替换生产 `fs2` 使用 | 是 | 保持原生锁与私有文件安全契约 |
| M3：应用查询与 CLI/TUI 编排解耦 | 是 | 保留现有 A/C 状态、`All`、JSON、退出码及可见行为 |
| M4：小范围 XML 解析替换 | 是 | 不默认修改生成输出与服务指纹 |
| M5：同契约私有临时文件封装 | 是，先分类后实施 | 不改变恢复命名、发布顺序、权限与持久性 |
| M6：回归、复审与净收益记录 | 是 | 不以未执行的检查冒充通过 |
| R2：已经落地的 Windows 三项迁移 | 只保留回归 | 不重复迁移 |
| A0—A5：来源隔离产品路线 | 否 | 必须先明确选择 A，并批准兼容/迁移规则 |
| P1/P2：SQLite 有界原型 | 否 | 被明确委托后，才在隔离数据上实施 |
| SQLite 生产存储切换 | 否 | 原型通过之后仍需独立批准 |
| R7/R8 其余库替换、daemon/IPC 重构 | 否 | 不因“顺手优化”纳入 |

批准 M3 不代表批准新的集合报告 schema；批准 A 不代表批准 SQLite；批准 SQLite 原型不代表批准操作真实历史数据。

### 1.3 本次任务不默认授权的外部操作

不自动 push、发 PR、创建 tag/release、部署、安装/卸载真实服务或触发 GitHub Actions。只有用户明确要求时才执行相应操作；尤其不要通过 push 间接触发未授权 CI。

本机验证遵循本机环境指令，不把某台机器的限制固化到项目级 `AGENTS.md`。不要为补齐不存在的原生平台而自行启动 Docker 测试、远端测试或 GitHub CI。测试优先使用隔离临时目录、合成样本与仓库夹具，不修改真实 Codex 历史、账户配置或现有服务。

## 2. 执行顺序、并行与提交边界

建议依赖顺序：

```text
M0：基线/契约/任务所有权 + 原提案修订
  ├─ M1：semver
  ├─ M2：标准库文件锁
  ├─ M3：报告/应用层解耦
  └─ M4：XML 解析
       （以上可按文件冲突情况并行）

M2 完成相关私有文件改动后 → M5：临时文件同契约归并
各工作包完成 → M6：组合回归、复审、交付记录

A0—A5、P1/P2：仅在各自决策门打开后追加，不阻塞默认范围
```

支持多 agent 的环境中，主 agent 负责集成、共享接口与最终验收；已就绪且文件所有权不冲突的实现任务可以并行。不能让所有 worker 等待同一个顺序研究任务，也不能让多个 worker 无协调地同时修改 `Cargo.toml`、`Cargo.lock`、相同测试模块或私有文件底层。

可行分配是版本更新一组、锁及相关测试一组、应用层一组、XML 一组。主 agent 统一依赖变更；M5 避开 M2 对底层文件的并发编辑。没有多 agent 能力时按依赖执行，不为本任务新建调度系统。

每个工作包保持独立可审查的提交边界；按仓库既有约定处理本地提交，不自动 push。不要把 semver、文件锁、产品语义切换和数据库迁移混入同一提交。各 worker 做针对性验证，组合完整回归由主 agent 统一执行，避免重复跑多套全量测试。

## 3. M0：复核和原提案修订

### 目标

把审核意见转化为当前仓库可落实的任务，消除提案中的隐式授权和调用边界歧义。

### 必做修改

按审核文件 D1—D8 修订原提案。重点调整：R4 可独立前移，R7 默认暂缓，R2 已完成；C1 不循环完整报告加载器；C4 加同机旧进程；文件锁加混合版本验证；SQLite 门槛具体化。

保留原提案的审查日期、历史快照与证据说明。在开头增加简短修订记录和本次实际 HEAD；不要把旧统计值标成新测量，也不要把 A/C 或 SQLite 的待决定项改成已批准。

建立下面两个紧凑清单即可，无需另写大量架构文档：

**查询入口/契约清单。** 检查 `collect_and_load_report_history*`、`load_v2_history_since_inner`、`load_remote_overview_history`、`AllIncluded` 及其他实际调用者。记录入口是否采集、stage、flush、恢复、读取 quota、执行同步；记录隐私/所有权约束、失败处理和输出消费者。覆盖 CLI summary/trends/health、后台刷新和非当前显示页面，具体入口以 HEAD 为准。

**A 路线候选删除清单。** 只做源码盘点，不删除功能。记录 facts/归并相关符号的生产消费者，区分中心专用、旧 v5 服务端和聚合同步共享职责。该清单是未来 A 决策依据，不是已经批准的删除列表。

### 退出条件

原提案修订完成；默认范围、产品门和原型门一目了然；相关实现与测试位置已找到；不存在用“后面实现时再决定”掩盖外部 schema、退出码或安全边界的情况。

## 4. M1：`semver` 替换

### 主要落点

`src/update.rs` 中 `compare_versions` 及其升级判断/测试，`Cargo.toml`、`Cargo.lock`。精确函数与依赖版本先按实际 HEAD 核对。

### 实现要求

使用 `semver::Version::parse` 验证完整输入，再使用 `cmp_precedence` 比较升级优先级；不要先去掉 `+` 后缀再解析，也不要直接用默认 `Ord`。[E1]

原入口已有的输入约定、错误上下文应先确认；非法版本不能被当作相等、更旧或默认版本。源码 build 身份、同版本冲突、禁止降级、安装 receipt 和更新事务保持原逻辑。仅声明必要的直接依赖，不连带升级无关包或重写发布脚本。

### 最低验收

| 场景 | 要求 |
| --- | --- |
| 主/次/补丁与预发布顺序 | 既有正常升级顺序不变 |
| `01.0.0`、`1.0.0-01` | 按合法版本契约拒绝 |
| `1.0.0+`、非法 metadata | 拒绝，而非截断后接受 |
| 长数字预发布标识、合法边界 | 无自制窄整数溢出或错误排序 |
| 只有 metadata 不同 | 升级优先级相同 |
| 同版本不同源码 build | 仍由原有身份冲突规则处理 |
| 降级及相同版本策略 | 既有安全策略不变 |

删去不再使用的手写 parser 和专属于旧实现的冗余代码。对错误接受的非法版本收紧校验，可在变更记录中明确说明，无需模拟旧错误。

## 5. M2：标准库文件锁迁移

### 主要落点

先全仓检索 `fs2`、`FileExt`、`lock_contended_error`、`lock_is_contended`，再覆盖 `file_lock.rs`、`private_state_store.rs`、ownership/profile lease、诊断和实际发现的调用点。不得仅替换一个包装器便宣称完成。

### 实现要求

保留现有 guard 和显式解锁责任；将通用锁调用迁移到标准库，按 `TryLockError::WouldBlock` 与真实错误区分竞争和故障。核对工具链支持，不为迁移无故升级项目 MSRV。[E2]

锁文件的位置、创建/打开参数、共享策略、no-follow/文件身份检查、锁后重验证、错误传播和释放顺序保持不变。不要混淆独立打开的句柄与 `try_clone`/继承句柄的所有权，避免重复获取同一底层已持有锁的对象。

文件锁保证互斥，不自动替代历史 ownership 或 profile lease；后两者不能在换 API 时被删掉。

### 最低验收

覆盖同进程和跨进程竞争、共享读者共存、独占排斥、try-lock 非阻塞、真实 I/O 错误、提前失败后的释放、复制/继承句柄存活时的 guard 释放，以及独立共享读者不被另一个 guard 析构误释放。

增加**旧 `fs2` helper ↔ 新 std helper 的双向跨进程竞争**：独占锁彼此排斥，共享锁行为与现有契约一致，释放后可以获得锁。用显式 ready/continue 握手和有界超时，不依赖随意 sleep 判断竞争。

Windows 原生覆盖实际 sharing 与句柄行为；Unix 语义在有相应原生环境时验证。无条件的“跨平台都通过”必须有对应执行证据。

生产使用清零后再移除生产 `fs2` 依赖。若为互操作验证保留测试依赖，应明确记录，不要把“生产退役”写成“整个锁定图完全删除”。不要为了短期测试额外维护永久的旧锁生产实现。

## 6. M3：先做保持外部语义的应用层解耦

### 6.1 工作目标

从 CLI/TUI 中提取真正可复用的应用编排与查询结果边界，使展示层不再重复建立历史环境、控制写入与拼接查询。**这一阶段保留当前完整归并能力和对外输出，不把 `AllIncluded` 改成多来源求和或新集合。**

主要落点为 `src/cli.rs`、`src/history_query.rs`、`src/tui.rs` 及实际历史 runtime 接口。新增模块、函数和类型按仓库习惯命名，不要求照搬下文示意，也不建设通用服务框架。

### 6.2 最小内部边界

拆分“接收/准备一次 observation 并按需提交”与“使用已准备环境查询”。不能在每个来源循环里重新调用 `collect_and_load_report_history_selected`。

内部结果应能表达来源用量、账户 quota、各自状态、警告、revision 和只读限制。可以沿用已有结构并增设小型适配层，不要求一次改动全部报告 DTO。

每次一致性查询尝试共享查询时刻、范围、来源策略、项目映射、隐私上下文和额度结果；维持现有读取预算与资源上限。来源策略、权限或 revision 改变时按现有规则有界重试，重新建立有效上下文。不要通过无界缓存或放宽快照检查减少读取次数。

明确的读取阶段不承担新采集、`stage_local_collection`、`flush_staged`、主动同步或隐式格式迁移。必要的历史恢复与所有权协调放回明确的准备/恢复阶段，不是简单删除。

只读模式可以保留原有内存 staged overlay，但不得持久化本应受写权限约束的历史。诊断是否可写遵循原有独立规则，不把“历史只读”误解为禁止一切日志，也不借日志路径写历史。

### 6.3 quota 与错误结构

quota 为账户观测，不是某来源 token 总数的子字段。内部应避免每查一个来源就重复遍历全部 quota 来源，并保留同账户确认、来源纳入、redaction、reset/时间槽和 provenance 规则。

内部可区分整体环境失败、单源用量失败和 quota 失败。**默认 M3 通过兼容适配层维持既有可见错误、警告、partial 和退出码。** 新的“部分来源成功仍输出集合”或“quota 故障不阻断 token 展示”如果改变现有输出行为，必须单列为行为变更，在获得对应范围授权后再发布；不能用重构提交夹带。

`AllIncluded` 的现有归并仍走旧语义；精确 local/remote 路径继续保留来源选择。显式 remote 不可用时不能自动回退 local。

### 6.4 消费者迁移方式

先迁移共用报告入口和一个代表性 TUI 路径，验证新边界确实复用，再迁移其他相同职责消费者。不得只移动代码、保留所有旧入口及重复实现，却宣称应用层完成。

不必一次改掉全部渲染函数；要确保剩余兼容包装薄且没有再次承担 stage/flush/同步。若暂留包装函数，应注明消费者和收敛条件，不长期维护两套独立业务引擎。

### 6.5 最低验收

| 编号 | 验收 |
| --- | --- |
| Q1 | 一次应用请求中的本地准备/提交不会因为来源数量成倍执行；用调用计数或现有 spy 验证 |
| Q2 | 同一次有效查询尝试共享账户 quota 读取/合并；必要的有界重试单独计数 |
| Q3 | 真正读取阶段不新增采集、flush、主动同步或隐式迁移 |
| Q4 | 只读和 profile/ownership 改变时不越权写入；写前重验证仍有效 |
| Q5 | local、显式 remote、AllIncluded 的既有 token/费用/partial/告警及输出结构保持 |
| Q6 | quota 错误内部可区分；对外仍符合本阶段冻结的兼容行为 |
| Q7 | 具体来源不可用不回落本地；无数据、离线、过期不变成完整的零值 |
| Q8 | CLI summary/trends/health、后台刷新、缓存失效及非当前页消费路径均有覆盖 |
| Q9 | 源策略、revision、redaction 或项目映射改变，不跨边界复用旧上下文 |
| Q10 | 本阶段不建立“所有远端同一时刻快照”的新承诺，不扩大读取预算 |

测试应尽量复用现有 fixture 和故障注入工具，不为调用计数构造一套庞大的 mocking 框架。

## 7. M4/M5：小范围机制整理

### 7.1 M4：XML 解析

先定位 `service.rs` 中 launchd plist 的字符串搜索 parser，以及已有 `quick-xml` 用法和锁定版本。复用现有依赖，限定读取项目需要的字段，验证目标字典层级、键值对应、值类型和重复键。

先建立正常服务定义及指纹的黄金样本，再替换解析。覆盖转义、换行/回车、空元素、错误嵌套、截断或畸形输入、输入大小/深度边界以及不支持的声明/实体策略。不要把拒绝无效结构做成静默丢字段。

标准 plist 声明如何处理应基于现有合法样本确定；不应为了限制复杂输入而误拒绝项目自己或系统正常产生的服务定义。解析不得引入外部资源解析或任意文件/网络读取。

**默认不改生成格式、转义策略或服务指纹，不改 `launchctl print` 的非 XML 文本路径。** 生成端确需变化时另交兼容证明；原生 launchd 运行检查只在相应环境与授权下执行，纯解析测试通过不能冒充原生服务验证。

### 7.2 M5：私有临时文件和发布

先按以下契约分类实际调用点，不按文件名或表面语句相似度统一：

| 类别 | 本轮允许做什么 | 不允许顺带改变什么 |
| --- | --- | --- |
| 普通可丢弃临时文件 | 复用分配/清理，必要时评估 `tempfile` | 输入边界、目录和权限策略 |
| 可恢复历史发布 | 只归并确实相同的安全/发布原语 | 恢复命名、commit 顺序、redo/manifest 语义 |
| 安装升级候选文件 | 只整理经证明相同的底层操作 | 二进制身份、receipt、rollback、原生替换语义 |

保持安全创建、写入、文件同步、关闭/替换策略、发布后验证和目录同步等既有边界。Windows 打开读者下替换、ACL、no-follow 和实际对象身份验证不得降级。

`NamedTempFile::persist` 不承担全部持久化同步；RAII 也无法在进程被终止之后执行清理。参与 `.target.pid.sequence.tmp` 恢复识别的路径，默认不改成随机文件名。[E3]

优先复用已有私有文件组件。只有同契约调用点确实减少重复责任时才提取公共函数；如果适配代码反而更多，就记录保留结论，不为满足“瘦身任务”强行引库。`tempfile` 是否由开发依赖变成生产依赖须记录净成本。

### 7.3 退出条件

XML 正常生成/指纹不变，错误结构处理有测试；临时文件抽象没有放宽权限/恢复/持久性，故障时保留旧数据或按既有明确状态恢复。未修改的高风险发布路径清楚列出，不宣称已经完成全仓替换。

## 8. M6：验证、复审与交付

### 8.1 验证次序

按仓库现有脚本和实际支持的 features 执行：目标测试 → 受影响模块测试 → 本平台构建/Clippy/格式检查 → 适当的组合回归。不要凭空添加 `--all-features` 或更换 CI 策略。

下列只是命令形态示例，具体过滤词和检查参数必须从当前仓库确定：

```text
git status --short
git rev-parse HEAD
cargo fmt --all -- --check
cargo test --locked <已经确认实际存在的测试过滤词>
```

执行过滤测试后要确认确实运行了预期测试，不能把“0 tests”当作验证成功。新增依赖应按仓库方式更新锁定文件，更新完成后用锁定依赖验证；不要为通过 `--locked` 随意删除锁定约束或重置依赖。

锁、文件身份与平台服务相关场景以原生平台证据为准；其他平台暂不可执行时，标记“未执行/待原生验证”，不要用静态阅读、交叉编译或容器测试伪装成原生运行。进程强杀测试证明的是对应进程中断路径，不能自动当作断电持久性证明。

### 8.2 合并前独立复审

主 agent 或独立 reviewer 对组合 diff 检查：有没有扩大任务范围、改变 All/JSON、放宽隐私或 ownership、删掉仍被 v5 使用的 handler、误删共享摘要/subagent/quota、吞掉错误、留下旧生产引擎，以及通过删测试减少失败。

源码、测试、文档和实际验证应指向同一最终代码状态。测试后继续修改代码时，应按影响范围补跑，不把先前 SHA 的结果无条件沿用为最终证据。

### 8.3 净收益记录

区分生产实现、测试、注释/空行及纯移动。记录实际删除的重复职责、剩余适配代码、新增依赖/feature、生产与开发依赖差异，以及经实测的性能/资源数据。没有测量的项目填“未测”，不承诺百分比收益。

不以减少文件数、减少测试或把实现搬到另一文件为目标。普通机制替换允许总行数暂时因边界测试增加；价值应解释为减少手写机制或降低修改风险。

### 8.4 完成报告格式

最终提供以下内容，并将精简结果补入第 11 节：

```text
开工 HEAD / 最终 HEAD（有未提交改动则说明）：
完成的 M 编号与对应提交/文件：
重要实现选择与兼容边界：
实际执行的测试、平台、结果与日志位置：
未执行的验证及具体原因：
生产/测试代码和依赖变化；已测/未测收益：
复审发现及处理：
仍关闭的决策门：A / SQLite 原型 / SQLite 生产迁移。
```

遇到一个工作包无法安全完成时，保留其事实和阻碍，继续其他独立工作。不要反复请求用户确认已定义的技术细节；只有确实涉及产品能力、破坏性数据迁移或外部操作的决定才停在对应决策门。

## 9. 条件路线 A：仅在产品明确选择 A 后执行

以下是未来的实施约束，**不是默认工作包**。M0 可以完成源码清单和场景草案，但不得据此关闭 facts 或改变输出。

### A0：冻结语义、依赖与兼容策略

选择来源记录语义，确认不再提供跨来源同会话唯一事件总量。冻结 CLI 默认行为、新集合 JSON/schema、旧 `--source all` 处理、UI 保存状态、逻辑会话 ID、项目/任务 ID、partial 与退出码映射。

建立实际删除清单：

| 符号/模块 | 当前生产消费者 | 中心/agent/共享 | 保留责任 | 最后消费者退役阶段 | 验证 |
| --- | --- | --- | --- | --- | --- |
| 由实际 HEAD 盘点填写，不以猜测预填 |  |  |  |  |  |

至少检查 facts planner、follow-up、同步摄取、exporter、reconciliation，以及 `source_history`、`source_export`、`remote_export_state`、`session_evidence` 中混合职责。同步编排和存储共享代码不能因名字包含 facts 就整块删除。

### A1：来源报告与消费者迁移

基于 M3 的内部边界实现来源报告集合。复用精确来源的计算，不按源重复 stage/flush 或 quota 合并。保留源 ID、revision、新鲜度、partial、价格不确定性和错误。

多来源默认按来源展示；可选合计必须独立命名为未去重记录合计。账户 GLOBAL quota 单独展示，不对百分比求和。比例与图表分母必须对应同一来源/时段，或明确使用未去重记录口径。

更新 Overview、Models、Summary、Trends、Health、后台刷新、CLI/JSON 和所有实际消费者；旧 All 不能同名静默改为求和。显式来源不可用不回落本地，旧逻辑会话 ID 不能被误作新的来源会话 ID。

### A2：停止本中心 facts 补齐，但保留同步收尾

同时处理手动和自动同步后续、本中心本地 facts 物化。用 transport spy/调用计数证明本中心不再发起 `SessionFacts` 或额外事实扫描。

保留 `finalize_remote_sync_attempt` 等共享收尾的 config/host fence、预算结算、metadata、健康和 quota。不要通过提前 return、永久报错 transport 或关闭所有同步来模拟关闭 facts。

旧 v5 服务端 handler 及物化依赖暂保留，直到 A4 的协调协议切换。

### A3：删除已无生产消费者的中心逻辑

按清单退役中心归并、facts 规划/摄取及专用状态。先拆共享摘要/指标、类型和存储职责。移除对应生产路径及专属测试；保留支撑剩余契约的测试，不用测试占位器隐藏生产调用。

允许有结束时间的离线影子对照，不保留长期 A/C 双生产引擎来宣称瘦身。

### A4：协议、磁盘状态及旧进程协调迁移

冻结并验证下列矩阵，允许的结果可以是安全拒绝，而不必假装全部兼容：

| 场景 | 必须明确 |
| --- | --- |
| 新中心 + 旧 agent | 支持范围或明确版本不匹配；不能只去掉 capability 就假定旧行为不会发生 |
| 旧中心 + 新 agent | 不产生半成功/坏游标；拒绝或兼容的契约明确 |
| 同机旧 recorder/TUI/CLI 正在运行 | 阻止冲突写入的方法与升级顺序，不能只依靠旧程序不认识的新标志 |
| 迁移中断/重启 | 可恢复状态、重试幂等、原始数据仍可用 |
| 新格式写入后重启旧二进制 | 如何阻止误写；是否只能通过一致备份恢复 |
| 旧 UI/过滤器/逻辑 ID | 显式迁移或失效提示，保留具体来源选择 |
| 备份与回滚 | 备份内容、一致性、恢复步骤、不可回退边界 |

实际协议版本号由仓库约定和变更情况决定，不在本文件预先猜定。仅在该边界统一退役旧 facts handler/exporter/wire DTO。

旧磁盘格式用独立读取 DTO 或明确迁移处理，不能直接删 `deny_unknown_fields` 状态中的字段便声称兼容。在对应锁及所有权机制下退役可重建派生数据，保留源身份、聚合桶、quota、游标和提交状态。真实用户历史的迁移执行需另有授权和可用备份。

### A5：退出条件

覆盖以下场景：独立会话、完整复制、公共 100 + 分叉 20/30、冲突/缺失/过期 facts、排除/离线/失败来源、父子会话与 subagent、多来源同账户 quota，以及升级、分页失败、重试和重启。

成功来源可保留，但集合不完整必须可见；全部来源失败、partial、无数据和 quota 失败按 A0 固定的规则处理。证明不再承诺唯一总量、事实补齐已退役、共享同步仍正确、兼容窗口有结束点，再记录实际净删除量与性能变化。

**不能通过过滤掉 delta journal 的旧变更、却继续推进游标实现所谓优化。** 请求时间范围不改变已承诺的增量消费与提交语义。

## 10. 条件路线 P：SQLite 有界原型

此路线只有收到明确的原型任务才启动；生产切换另行审批。R3 尚未确定时，只研究不依赖 facts 去留的本地观察契约，不迁移准备退役的数据。

### P1：单次本地 observation

隔离原型覆盖一次 observation 的多数据族提交、同 revision 可见性、重启恢复和多进程一致读取。使用合成或明确授权的脱敏副本，既有 JSON 生产路径不受影响，不自动导入真实历史。

输出的不只是演示代码，还应逐项提供证据：

| 项目 | 必须产出 |
| --- | --- |
| 依赖版本 | `rusqlite`、`libsqlite3-sys`、features、实际运行 SQLite 版本/来源；包含适用安全修复 |
| 文件安全 | DB/WAL/SHM 与目录权限、路径/对象替换边界、只读和恢复过程；对现有威胁模型的满足或明确缺口 |
| revision | 已保留编号不复用、所有权/profile 改变、中断重试及对读者可见结果的对应测试 |
| 事务边界 | 短事务；网络、分页等待、长时间解析不占写事务；忙等待/取消有界 |
| 精确金额 | `u128` 往返、边界、排序、聚合、溢出；不能只测试插入后读回 |
| 持久性 | journal/synchronous 等实际配置；与旧实现相当的目标；明确进程崩溃与断电测试证据差异 |
| WAL/资源 | checkpoint、长读者、增长上限、磁盘不足和中断恢复；自定义 history-root 的文件系统策略 |
| 性能/构建 | 冷启动、写入、查询延迟、磁盘/写放大、二进制与依赖构建成本；固定数据和配置 |
| 迁移草案 | 旧格式导入、schema 版本、备份和回退；未获授权不实际迁移用户数据 |
| 净维护收益 | 可由数据库替代的旧机制、必须保留的业务规则、新增适配/迁移/测试责任 |

底层 SQLite 应包含 WAL-reset 修复；官方给出的常规修复边界为 3.51.3（2026-03-13）及之后，也存在部分旧分支回补。实施时核实实际版本及更新的官方信息，不把本文件当作永久充分的依赖安全清单。[E4]

不要用降低持久性的配置制造性能优势。现有 revision 持久化保留规则不能简单由普通回滚事务替代；数据库文件的打开前路径检查也不能未经证明就等同于验证实际操作对象。

### P2：一个远端增量页

只有 P1 结果足以支持继续投入时，才验证一个远端页的 expected-active、页身份/指纹、数据族与游标共同发布、重复执行、来源 generation 改变及中断恢复。

不能把远端网络事务扩展成一个长期 SQLite 写事务；不能由本地事务成功直接删除所有 COW/generation。应说明数据库具体接管了哪些原子发布和恢复责任。

### 原型退出与停止条件

结论允许为采用、继续限定验证或不采用。安全/数值/并发契约不能满足、平台构建代价过大，或适配后无法减少维护责任时，停止扩大原型并记录证据。

原型通过后，另提交生产迁移方案及真实数据操作授权请求。**不保留一个无人维护的永久可选 SQLite 后端，也不在 SQLite 上长期原样复制全部旧 redo/COW 引擎。**

## 11. 本地实施记录

### 11.1 开工复核与清单

2026-09-26 开工 HEAD 为 `abef359eac136a2b004c7e4b98890cee27bd37fe`，与审查读取提交相同。原提案已经有用户未提交修订，两份配套文档未跟踪；保留这些内容，未回退至更早基线。D1—D8 已合入，M0 只补本地事实和清单。机器为 Windows 11 AMD64，Rust/Cargo 1.97.0，目标 `x86_64-pc-windows-msvc`；遵照 `.agent/environment.local.md` 及本次委托使用本机原生验证，不使用 Docker、UTM、远端或 GitHub CI。开工文档哈希及工具链证据保存于 `target/verification/refactoring-20260926/baseline.json`。

开工源码复核：手写 `compare_versions`、生产 `fs2` 与 launchd 字符串解析仍存在；`Zeroizing`、`windows-service` 和 `winreg` 已完成的迁移仍在，不重复实施。以下清单描述开工行为；重构后的边界与验证在后续记录中对应。

| 查询入口 / 消费者 | 准备及副作用 | 读取、错误和约束 |
| --- | --- | --- |
| `cli::run_summary/run_trends/run_health` → `collect_and_load_report_history_selected` | 采集在上游；建立 runtime、profile lease、恢复/迁移，重验证后 stage/flush；Summary backfill 是另一个显式写入操作 | local/remote/AllIncluded，保留报告 DTO、warning、partial 和退出码；明确 remote 不回退 local |
| `history_runtime::load_unified_history_since{,_selected,_with_staged_selected}` | 本身已是查询包装，初始化及恢复在 runtime 构造/准备入口；staged overlay 只在内存应用，不重复实施已存在的分离 | `history_query` 统一读取，ownership/profile/redaction 与源 revision 限制、项目映射、一致性重试及预算均须保留 |
| `history_query::load_v2_history_since_inner` | 不采集、不主动同步；读取 account quota 及同账户远端 quota，随后读取 token 与证据 | quota 不依赖 token selector；原 quota 错误会传播至整个查询；`AllIncluded` 保留 reconciliation；策略改变触发有界重试 |
| `TuiHistoryStore::load_since_with_staged_selected/reload_since_with_staged_selected` | deferred runtime 准备、显式 stage/flush 与读取交织；写前验证 profile lease | Summary/Trends/Health 与历史刷新；projection revision、staged revision 和过期时间控制缓存 |
| `TuiHistoryStore::load_remote_overview_history` | 读取源 metadata、active revision；接受已读 unified seed 或执行 AllIncluded 查询 | Overview/Models、后台完成及非当前页刷新均消费；失败保留既有文本诊断，缓存不得跨策略/项目映射复用 |
| `automatic_remote_sync` / CLI remote sync | 独立执行 transport、分页提交、facts follow-up 和 finalize | 不属于纯查询；本轮不截断同步或 facts，不变更预算及 v5 能力 |

A 候选清单仅供未来产品决策，以下符号本轮均保留：

| 符号 / 模块 | 实际生产消费者 | 属性与保留责任 | 最后消费者退役门 / 验证 |
| --- | --- | --- | --- |
| `logical_replica::detect_replica_candidates`、`history_query/reconciliation::plan_replica_resolution` | AllIncluded 查询投影；`remote_fact_sync` planner | 中心归并；共享摘要与精确来源投影不能一起删 | A1 消费者迁移后 A3；复制/分叉/partial 投影回归 |
| `replica_fact_followup::{prepare,execute_prepared}_replica_fact_followup` | `cli` 手动同步、`automatic_remote_sync` | 中心主动补齐；`finalize_remote_sync_attempt` 的 host/config fence、预算、health、quota 独立保留 | A2 停调用、A3 退役；transport spy 和收尾回归 |
| `remote_fact_sync::{plan_next_replica_fact_sync,sync_remote_thread_facts_bounded}` | `replica_fact_followup` | 中心 planner/摄取；分页和游标责任按字段核对 | A3；失败页不推进、重试幂等 |
| `remote_agent` 的 `SessionFacts` 分支、`remote_fact_exporter::prepare_remote_fact_page`、`remote_protocol` facts DTO | v5 服务端请求 handler；中心 follow-up 也使用本地物化函数 | agent / 中心共享，`materialize_complete_session_facts_from_normalized_observation` 尚有双侧消费者 | A4 协调协议切换；新旧中心/agent 明确拒绝或兼容 |
| `source_history/session_evidence`、`source_export`、`remote_export_state` | exporter、facts ingest、reconciliation、聚合导出和 revision fence | 混合职责；摘要、来源身份、quota、generation、游标与 fence 不能按文件删除 | A3 先拆职责，v5 部分等 A4；分页/恢复/ownership 回归 |
| `source_history/remote_generation`、`local_observation`、`remote_sync_attempt::finalize_remote_sync_attempt` | 聚合页发布、本地观察提交、手动/自动同步 | 共享保留；COW/manifest、redo、revision 不复用及 quota 非 A 专用 | 不在 A 默认删除量内；须独立替代证据 |

同机旧 recorder/TUI/CLI、迁移中断、新格式后旧二进制、备份回滚的场景草案继续采用第 9 节 A4 矩阵；本轮不选择迁移机制，也不声称旧进程可被新标志阻断。

### 11.2 实施与验证结果

M5 契约分类与处置：

| 类别 / 调用点 | 本次处置 | 仍由调用者保留的责任 |
| --- | --- | --- |
| `PrivateStoreLayout::write_atomically`、`project_mapping::write_mappings_atomically`、`remotes_config::write_private_atomically`、`source_identity::write_private_atomically`、`history_ownership::write_private_atomically`、`history_profile_lease::write_active_profile` | 共用 `private_state_store::publish_private_replacement`；统一 write → sync_all → close → replace → 发布后校验 → 目录同步及失败清理 | 各自的输入上限、目录验证、安全分配、ACL/no-follow/实际对象验证、临时文件名和发布后校验；发布后校验失败仍返回错误，不虚构已回滚 |
| `cache` 的可丢弃缓存/写探针 | 保留；缓存写入仅 flush，写探针 sync_all 后删除，与持久状态发布契约不同 | 缓存目录/权限和清理；本轮不强化或降低原持久性策略 |
| `history` / `source_history` 的流式 JSON、`.target.pid.sequence.tmp` 恢复文件、local observation redo、remote generation/manifest | 保留 | 尺寸预算、恢复命名、revision、提交顺序、分页游标、故障恢复 |
| ownership 的 hard-link create-once、project mapping 的 prepared 发布 | 保留 | 不覆盖已有目标、prepared 生命周期和协调提交 |
| `update` / 安装升级候选 | 保留 | 二进制身份、receipt、rollback 及原生替换契约 |

不增加 `tempfile` 生产依赖。抽出的目录同步沿用旧实现：Unix 同步目录句柄，Windows 由既有 `atomic_file::replace_file` 的原生替换负责；没有新增断电持久性承诺。

| 项目 | 实施状态 | 结果/证据 |
| --- | --- | --- |
| 开工 HEAD / 工具链 / 工作区 | 已核对 | M0—M6 收尾验证时 HEAD 为 `abef359eac136a2b004c7e4b98890cee27bd37fe`，当时改动未提交；后续本地提交见第 11.5 节。保留用户原文档修改；无 push/tag/部署/CI |
| M0：复核与提案修订 | 完成 | 第 11.1 节清单、原提案复核记录；已有 D1—D8 和 Windows 迁移不重复实施 |
| M1：semver | 完成 | `update.rs` 及测试；完整解析 + precedence，保留 build 冲突/降级保护；4 个目标测试通过；CHANGELOG 记录非法版本拒绝 |
| M2：标准库锁 | 完成实现及 Windows 目标验证 | 19 个锁模块迁移，删除 11 个 contention helper；保留 guard/unlock/identity；新旧跨进程双向共享/独占、释放和真实 IO 错误回归通过；Unix 原生待验证 |
| M3：应用边界 | 完成实现及目标验证 | 新 `history_application.rs`，CLI、TUI、history runtime/query 接入；8 个新增目标测试通过；保留 AllIncluded、v5/facts、报告 schema/退出码及 quota 可见错误 |
| M4：XML 解析 | 完成实现及纯解析验证 | 新 `service/launchd_plist.rs` 及黄金 fixture；两个生产入口各解析一次；旧 parser 黄金 1 项、最终解析 10 项通过；未执行原生 launchd |
| M5：临时文件归并 | 完成限定范围 | 6 处共享持久化替换；与 M2 合并目标验证 11 项通过（其中 1 项为进程 helper 的空入口，父用例实际执行双向握手）；未改高风险保留路径 |
| M6：组合验证与复审 | 完成 | 独立交叉复审、失败诊断和最终影响范围复验完成；组合证据覆盖 1,779 项库测试与 128 项集成测试，保留 1 项真实历史手动基准未执行；结果、哈希、命令和限制见下文 |
| A 产品路线 | 决策门关闭，未执行 | 只盘点候选消费者，不改 All，不删 facts/v5，不操作真实状态 |
| SQLite 原型 | 决策门关闭，未执行 | 未新增 rusqlite/SQLite 或实验后端 |
| SQLite 生产迁移 | 决策门关闭，未执行 | 无 schema/存储迁移、真实数据导入或破坏性清理 |

M3 的具体边界与验收：

- `prepare_report_history` 独立执行 runtime 准备、stage/flush，写前再次验证 profile lease；`PreparedReportHistory::query` 不再提交。旧 CLI monolithic 生产入口删除，仅保留测试便捷适配。
- `stage_runtime_collection` 收敛 CLI、TUI 和 backfill 的三处 digest/stage 降级规则；各自启动 cutover 与可见警告策略仍保留。`query_runtime_history` 和 `history_projection_revision` 被 CLI/TUI 实际消费，不是无人使用的备用 API。
- request context 共享 quota 与 `SourceHistoryReadBudget`，每次读取仍验证 ownership、源策略、映射和相关 revision，最多 4 次一致性尝试。quota 原始错误和 usage 错误内部区分；兼容适配保留原 quota 失败传播及具体来源不回退。revision 探测失败只禁缓存，不新增远端查询失败。
- `prepared_report_` 两项覆盖准备一次、无重复 revision 提交、共享 quota、远端不可用与本地 revision 损坏；`request_context_` 四项覆盖源策略/映射失效、quota/usage 错误、跨重试预算和四次上限；`application_refresh_` 一项覆盖真实 TUI 精确来源加后台 Overview 的 quota 共享、源策略/本地 revision 缓存失效及 panic 后清理。
- 冷启动复验另增加 `remote_overview_seed_without_remote_sources_preserves_data_and_invalidates_old_cache`：无纳入的 SSH 来源且已有统一查询 seed 时，直接 `from_unified` 并清空 Overview 缓存，保留统一桶、warning/partial，不再读取仅服务于缓存的 revision；无 seed 或有远端仍走原查询和 fence。回归覆盖完整结果相等、旧缓存清理、无 seed 重新查询及新增远端失效。
- 既有只读/profile lease、redaction、pending observation、replica/facts、Summary/Trends/Health JSON、鼠标/键盘/compact 回归由最终本平台组合套件统一执行。没有新建多来源集合 schema，也不承诺远端同物理时刻快照。

### 11.3 验证证据与复审

所有本地日志位于 `target/verification/refactoring-20260926/`（Git 忽略）。原生平台为 Windows 11 AMD64 / `x86_64-pc-windows-msvc`，Rust 1.97.0；当时验证对象是未提交的工作区。组合 runner 的 JSON 保存完整文件哈希、命令、时间、退出码及测试前后是否变化；文档修改不计入 Rust/测试/脚本输入快照。后续提交与该快照的对应关系见第 11.5 节，原始日志不改写为提交后重跑的证据。

| 实际命令 / 检查 | 结果 | 日志 |
| --- | --- | --- |
| `cargo test --locked --offline --lib update::tests::semantic_` | 4 passed | `m1-focused.log` |
| `cargo test --locked --lib service::tests::launchd_plist_generation_matches_golden_bytes_and_fingerprint -- --exact` | 旧 parser 上 1 passed | `m4-golden-before-parser.log` |
| `cargo test --locked --lib launchd_plist` | 最终 10 passed | `m4-launchd-plist-final.log`、`m4-source-hashes.txt` |
| `cargo check --locked --offline` | 通过；后续以最终 Clippy/全套证据为准 | agent 编译输出 |
| `cargo test --locked --offline --lib request_context_ -- --test-threads=1` | 4 passed | agent 工具会话 36757；最终全套另含这些测试 |
| `cargo test --locked --offline --lib prepared_report_ -- --test-threads=1` | 2 passed | `m3-prepared-report.log` |
| `cargo test --locked --offline --lib application_refresh_ -- --test-threads=1` | 1 passed | `m3-tui-refresh.log` |
| `cargo test --locked --offline --lib -- file_lock::tests private_state_store::tests --test-threads=1` | 11 passed；源码前后未变 | `m2-m5-focused.log`、`m2-m5-focused.json` |
| `cargo fmt --all -- --check` | 通过 | `format-preflight.log` |
| `cargo tree --locked --offline --target x86_64-pc-windows-msvc -e normal -i fs2` | 无生产依赖路径 | `dependencies-fs2-production.log` |
| `pwsh -NoProfile -File scripts/windows/verify.ps1 -CargoTargetDir D:\Workspace\codex-usage-monit\target\refactoring-final -CargoBuildDir D:\Workspace\codex-usage-monit\target\refactoring-final-build` | fmt、Clippy `--all-targets -- -D warnings`、25 项 Python 契约、60 项 verify-smoke 和 17 项 dev-smoke 通过；沙箱权限契约处停止，未到 Rust 全套 | `windows-final.log/json` |
| `pwsh -NoProfile -File scripts/windows/tests/repair-state-permissions.ps1`；`powershell -NoProfile -File scripts/windows/tests/repair-state-permissions.ps1` | 正常宿主账户分别 10 passed，PowerShell 7.4.1 / 5.1.26100.9444；源码前后未变 | `permission-repair-host-pwsh7.log/json`、`permission-repair-host-powershell51.log/json` |
| 同一 `verify.ps1` 及两个目录参数，加 `-SkipFormat -SkipClippy -SkipSmoke` | 首次库全套 1,769 passed / 9 failed / 0 ignored；完整命令保存在 JSON | `windows-rust-final.log/json` |
| `cargo test --locked --offline --lib --` 后跟上述 9 项失败过滤词及 `--test-threads=1`（完整参数见 JSON） | 安全仓库外 TEMP、正常宿主账户下 9 passed；未重跑其余 1,769 项 | `windows-failed-focused-host.log/json` |
| `cargo test --locked --offline --bin codex-usage-monit` 及全部 10 个显式 `--test` 目标，`--test-threads=4` | bin 自身无测试；agent_management 首次 1 passed / 1 failed，后续目标尚未启动；失败项换短 TEMP 定向补验 1 passed | `windows-integration-final.log/json`、`windows-agent-short-temp.log/json` |
| `cargo test --locked --offline --no-fail-fast` 及剩余 9 个 `--test` 目标，`--test-threads=4` | 首次 124 passed / 1 failed / 2 ignored；ConPTY 初始数据等待超时，后续修正及复验见下文；全部目标名与参数保存在 JSON | `windows-integration-remaining.log/json` |
| 同一 `verify.ps1` 及两个目录参数，加 `-SkipFormat -SkipClippy -SkipTests` | 原生 CLI build、version、offline JSON/partial/任务非空冒烟通过 | `windows-cli-smoke.log/json` |
| `cargo test --locked --offline --lib -- tui::tests::remote_overview_seed_without_remote_sources_preserves_data_and_invalidates_old_cache tui::tests::application_refresh_shares_quota_with_background_overview_and_invalidates_quota_policy --exact --test-threads=1` | 最终窄修正的两项回归 passed；首轮短名配 `--exact` 的 0 tests 不作为证据 | `m3-overview-empty-seed-final.log` |
| `cargo test --locked --offline --test tui_pty --test tui_data_integration -- --test-threads=1` | 最终快照 4 passed，含两个真实 ConPTY 用例；正式 8 秒阶段期限未改 | `windows-tui-integration-final.log/json` |
| `cargo test --locked --offline --lib tui::tests -- --test-threads=4` | 最终 TUI 模块 339 passed | `windows-tui-unit-final.log/json` |
| `cargo clippy --locked --offline --all-targets -- -D warnings`；`cargo fmt --all -- --check` | 最终快照均通过 | `windows-clippy-final.log/json`；格式检查工具输出为空且退出 0 |
| `cargo test --locked --offline --test update_cli different_build_running_portable_launcher_keeps_proxy_protocol -- --exact --ignored --test-threads=1` | 最终 1 passed；使用独立 HEAD 构建作为 `CODEX_USAGE_MONIT_PREVIOUS_TEST_BINARY`，测试确认两个实际 buildId 不同 | `windows-previous-build-compat.log/json` |

测试环境修正与失败分类：首次未限定 `--lib` 的构建遇到旧 `target/debug/deps/codex_usage_monit.exe` 占用而 LNK1104，未终止用户进程，库目标验证及最终独立 target/build 目录避开该冲突。首次 `lock` 筛选的 97 项中，多项因默认 `%TEMP%` 继承另一沙箱 SID 而触发私有目录拒绝；停止该次运行，未放宽产品安全规则。后续 `TEMP` / `TMP` 均指向工作区本次证据目录下的 `temp`，其 DACL 仅允许当前测试账户、SYSTEM 和 Administrators。新增 NUL 句柄测试的错误假设也已纠正为 attributes-only 句柄，真实 IO 错误回归已通过。以上失败保留在原日志，不计为通过证据。

组合入口中的 ACL 修复契约在受限账户 `CodexSandboxOffline` 缺少 `SeSecurityPrivilege` 时失败；两份脚本的 Git blob 与 HEAD 一致，PowerShell 7/5.1 均独立复现，确认与本轮源码改动无关。随后仅将该独立契约放到正常宿主账户执行，使用新的合成临时目录，两种 shell 均 10 项通过；没有授予特权、修改产品 ACL 规则或操作真实 monitor 状态。5.1 子进程另指定原生 `PSModulePath=C:\WINDOWS\System32\WindowsPowerShell\v1.0\Modules`，避开本机混入 PS7 模块的问题。保留全部失败及补验日志，不将首次组合入口写成全绿。

库全套的 9 项失败已逐项分类：1 项非仓库识别受 TEMP 位于 Git 工作区影响；4 项安装注册表测试在创建随机隔离 HKCU fixture 键时被沙箱拒绝（OS 5）；1 项 trace 测试创建符号链接 fixture 时缺少特权（OS 1314）；2 项真实 Git 证据测试与 1 项 combined snapshot 测试在高并发运行中分别出现证据未取得和 5 秒握手超时。相同源码使用仓库外安全 TEMP、正常宿主账户及单线程定向补跑 9 项全部通过。后三项仍记为负载敏感风险，未调高时限、删除测试或认定性能无退化。

集成安装用例的首次失败发生在 PowerShell 5.1 执行过长的合成安装路径，直接执行此前已成功；换到短私有 TEMP `C:\Users\Ghost\AppData\Local\Temp\cm-refactor-20260926` 后同一测试通过。未修改真实 PATH、ARP 或服务。首次集成套件保留两项原有忽略门槛；随后利用独立 HEAD 对照产物，补跑 `different_build_running_portable_launcher_keeps_proxy_protocol` 并通过。最终仅 `benchmark_real_codex_cache` 因需要真实历史而未执行。

ConPTY 冷启动失败没有按环境问题直接略过：正式测试在串行复验中仍超时。隔离 `git archive HEAD` 的原测试通过（harness 7.38 秒）；当前实现的临时诊断副本在 30 秒诊断窗口内通过，trace 显示 source-aware 数据就绪约 7.5 秒。该副本不是正式通过证据，也未修改正式 8 秒期限。另一次相对 `--codex-home` 的 headless 诊断触发既有只读 fallback，其约 101 毫秒结果已排除；绝对路径复现 V2 冷启动约 7.43 秒。发现 Overview seed 路径为未使用的缓存增加探测后，实施第 11.2 节的窄修正并独立复审；最终正式 ConPTY 两项及数据集成两项均通过。对照日志为 `head-baseline-conpty.*`，诊断日志为 `windows-conpty-diagnostic.*`、`conpty-diagnostic-*`、`startup-absolute-diagnostic-*`。单样本、路径和冷加载差异限制了性能推断，不声称一般性能增益；Windows 冷启动期限余量仍是风险。

完整库/早期集成验证对应源码快照 `6aa474156a3307ab2eb45654f4a623a4ea7cba94c9ce31a2cb7549c3f451aa98`。最终快照为 `24453cdcc122ebbba85ed4c484ee687ff5b9e3d2a47970845b280636f00c7544`，只有 `src/tui.rs` 和 `src/tui/tests.rs` 改变；`source-delta-final.json` 已核对这两个文件的差集。按影响范围补跑 TUI 模块、ConPTY/数据集成、格式和 Clippy，其余模块沿用未改变文件的证据，不把早期全套称为最终源码的一次全绿运行。

独立复审与处理：版本/XML、锁/发布、应用层分别交叉复审；主 agent 统一依赖和组合验证。发现的本地 revision 探测扩大远端失败范围已改为禁缓存，并用损坏本地 revision 的真实 fixture 验证；错误复制会丢失 typed cause 的风险已改为兼容适配直接返回原始 quota 错误；Overview 的 ownership/映射/local/quota revision 失效与 panic 后请求上下文清理均补入回归。没有删除仍有生产消费者的 v5/facts、共享摘要、subagent、quota 或测试来减少失败。

### 11.4 代码、依赖及剩余边界

`semver = "1.0.28"` 成为直接生产依赖（锁定版本原已存在，默认仅 `std` feature，MSRV 1.68，MIT OR Apache-2.0）；`fs2 0.4.3` 移至开发依赖，仅供混合版本测试。`quick-xml 0.41.0` 不变，`tempfile` 保持开发依赖，工具链仍为 1.97.0。锁定文件只新增根包的 semver 依赖边，无包版本/checksum 升级或新包。已核对 [semver 锁定版本清单](https://docs.rs/crate/semver/1.0.28/source/Cargo.toml) 和 [precedence API](https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence)；本机无 `cargo-audit`，未执行完整最新公告审计，不将网页检索当作审计通过。

源码行数采用本次 `code_stats.py` / `code-stats.json` 的相同静态口径：`tests.rs`、tests 子目录及尾部 `#[cfg(test)] mod tests` 算测试区，其余算生产区（因此零散 cfg(test) helper 可能计入生产区）；单独统计空行及整行 `//` 注释，不声称编译器级分类。当前净变化为生产区 **+479** 物理行（非空/非整行注释 +419、注释 +41、空行 +19），测试区 **+1,143**（对应 +1,083/+12/+48）。新增应用模块中的约 600 行来自原 CLI 编排与 TUI revision 逻辑迁入，这部分纯移动不计维护收益。具体净收益是删除手写版本解析、11 套锁竞争分类、重复的 stage 降级和 6 处发布顺序实现；XML 增加结构检查与边界测试，未取得净减行收益。

除上述用于定位超时的单一 fixture 冷启动记录外，运行延迟、I/O、内存、二进制尺寸、构建成本和性能收益均**未做受控基准对照**；新 revision 探测及 account DTO clone 也有成本。未执行 Linux/Unix 与 macOS 原生锁/发布验证、真实 launchd/SCM 服务安装、断电持久性验证或真实用户历史基准；本次仅执行 Windows 本机验证，按委托不使用 Docker、不自动触发 CI/发布部署。进程测试不能替代断电证明；macOS XML 纯解析不能替代原生 launchd。以上跨平台及测量缺口保留，不作为已通过声明。

实施记录简洁保留实际状态、剩余风险和测试证据即可，不因每个子任务再创建一套重复文档。源代码事实与技术依据见配套审核文件的固定提交链接。

### 11.5 本地提交与验证快照对应

2026-09-26，收到“先提交，然后按文档规划下一阶段任务”的委托后，先在原 `main` 分支完成以下 6 个本地提交，再开始第 12 节规划：

| 提交 | 范围 |
| --- | --- |
| `90c38c7` | M1：完整 semver 解析、precedence 与升级身份回归；直接依赖边 |
| `546421f` | M3：应用准备/查询上下文、CLI/TUI 接入、缓存失效及冷启动窄修正 |
| `887db9d` | M4：有界 launchd XML 解析及黄金样本 |
| `23d3527` | M2：生产标准库文件锁、混合版本互操作、fs2 移至开发依赖 |
| `ee84f31` | M5：6 处同契约私有文件发布归并及失败回归 |
| `0656a20` | M0/M6 文档、审核原文、提案修订、实施证据和 CHANGELOG |

规划基线为 `0656a201edbb5517a9f3ecac78549cbc6d8a15c0`，该提交后工作区干净。M2/M5 的重叠文件仅拆分暂存内容，未改动已验证的最终工作区源码；中间提交边界做静态及格式复核，没有逐提交编译或重复执行全量测试。最终源码快照仍为第 11.3 节的 `24453cdcc122ebbba85ed4c484ee687ff5b9e3d2a47970845b280636f00c7544`。审核原文件字节保持不变；其顶部双空格 Markdown 换行保留，不作为代码空白缺陷删除。完整 Git SHA、快照及提交核对记录保存在同一证据目录的 `commit-planning-verification.json`。

本轮提交及后续规划只做源码一致性、文档链接、指令与 diff 检查，沿用第 11.3 节的组合测试证据，不宣称另有一次完整套件全绿。所有提交仅在本机，未 push、创建 tag、发布、部署或触发 GitHub CI。

## 12. 下一阶段任务规划（尚未执行）

### 12.1 建议顺序与范围

M0—M6 已完成，不再重做。建议下一次实施先推进 **N1—N3：Windows 验证稳定性、受控测量和依赖审计**；N4 的产品/架构决策材料可独立准备。N5 是有条件的平台补证，当前 Windows 机器缺少对应原生环境时保持未执行，不阻塞前四项。这里列出任务和验收，不代表本轮已经启动实现，也不改变第 9、10 节的决策门。

```text
0656a20 已提交基线 + 已有验证证据
  ├─ N1：Windows 夹具/时序失败诊断与定向修复
  ├─ N2：冷启动、查询与资源受控测量 → 有证据再做窄优化
  ├─ N3：锁定依赖的本地公告审计
  └─ N4：A/C 决策材料；SQLite 原型委托草案（可并行）
N1 的环境修正稳定后 → N2 正式采样
N1—N3 稳定 → 主 agent 一次组合验收与记录
具备并获准使用相应原生环境时 → N5 平台补证
A 明确选择及兼容策略批准后 → A0—A5
SQLite 原型明确委托后 → P1 → 结果支持且委托包含 P2 时继续 → 单独生产迁移决定
```

### 12.2 N1：Windows 验证稳定性（优先）

**问题依据：** 第 11.3 节已有 TEMP/ACL/执行账户、PowerShell 5.1 路径长度导致的夹具失败；两个 Git evidence 用例和 combined snapshot 握手在负载下失败，定向串行补验通过。后者尚不能定性为产品缺陷或纯环境噪声。

**工作与所有权：** 一名 agent 负责 Windows 验证入口、相关脚本契约与测试夹具；先复现再决定是否修改 `scripts/windows/verify.ps1`、`scripts/windows/tests/`、`tests/agent_management.rs`、`src/git_repository.rs`、`src/source_export.rs` 与 `src/source_history/remote_generation.rs` 中相关测试。先稳定环境，再定位 Git 子进程预算及快照 reader/publisher 握手阶段；区分夹具准备和被测竞争的成本。复用已有夹具，不建设新测试调度系统。明确短且位于仓库外的私有 TEMP、实际账户与 DACL、两种 PowerShell 的模块环境及独立 target/build 目录；这些是测试设置，不放宽产品 ACL 或自动给账户授予特权。

**验收：** 每个失败有原始日志、根因分类、改前复现和改后证据；时序用例优先显式就绪/继续握手与受控描述符，保留有界失败。相关用例在串行与受控并发下各进行固定批次的重复验证（初始各 10 次，开跑前固定上限），记录全部结果而非只保留成功轮次。涉及 PowerShell 时跑 5.1/7 外层调用契约；缺少权限的检查单独报告并在合适账户补证。单纯延长超时、永久串行化或多跑到绿不算解决。若证据表明是产品问题，先加确定性回归，并由主 agent 审核扩大后的相关代码范围。

### 12.3 N2：启动与查询成本测量（优先）

**问题依据：** 正式 ConPTY 最终通过，但修正前诊断样本约 7.5 秒、接近既有 8 秒阶段期限；该值不能代表修正后的耗时，尚无受控对照证明一般性能变化。M3 新增 revision 探测、quota DTO 复制及缓存检查的成本未量化。

**工作与所有权：** 另一名 agent 负责 `tests/tui_pty.rs`、`tests/tui_data_integration.rs` 的合成样本/测量，以及有证据需要修改的 `src/history_application.rs`、`src/history_query.rs`、`src/tui.rs` 和 `src/tui/tests.rs`。以隔离快照比较原 `abef359` 与已提交基线，固定工具链、构建 profile、绝对路径、数据、来源数量和执行账户；不 reset 当前 checkout、不读取真实历史。区分全新 V2 与已初始化状态，并确认没有因相对路径触发 legacy fallback。覆盖无远端、多个合成来源、缓存命中/失效、quota 变化及后台非当前页。保留正式 8 秒期限，临时诊断窗口单列。

**验收：** 先记录采样协议和次数，再测；进程首次启动与同环境重复启动分开，未控制系统文件缓存时不称真正冷缓存。建议每种条件至少 10 次，报告原始样本、样本中位数/尾部/最大值、超时数与运行负载；小样本不作稳定 p95 保证。比较准备/stage/flush、quota 合并和 revision 探测次数，记录可取得的 I/O、峰值内存、磁盘/二进制尺寸和构建成本，无法测量的明确留空。只针对已定位的重复工作优化，保留 ownership/profile、预算、有界重试与错误契约；不得缓存越过权限或 revision。优化后受影响测试及正式 ConPTY 通过，完整展示前后数据；没有足够差异证据时只交测量结论，不制造性能承诺。

### 12.4 N3：依赖公告审计（可与代码工作并行）

由主 agent 管理 `Cargo.toml` / `Cargo.lock`，先核对本地工具可用性，按仓库 `.github/workflows/dependency-audit.yml` 当前固定的 `cargo-audit 0.22.2` 和 `cargo audit --deny warnings` 做本地审计；执行时重新核对仓库要求，不把本文写成永久最新版本依据。记录工具版本、公告数据库版本/更新时间、完整命令、锁定文件哈希和结果。工具或网络不可用时如实记录缺口，不以网页检索替代审计，不因此触发 GitHub CI。

退出条件是每条发现都有依赖路径、生产/开发归属、适用性与处理决定；若需要修复依赖，只调整有证据的最小范围并做影响测试，不批量升级、不默默忽略 warning。审计完成不代表 A 或 SQLite 获批；本阶段不添加候选数据库或其他尚未选用的库。

### 12.5 N4：产品与架构决策材料（文档工作，可独立推进）

| 材料 | 具体交付与验收 | 决策门 |
| --- | --- | --- |
| A/C 产品取舍及 A0 草案 | 复用第 11.1 节消费者清单，只增量复核新 HEAD。用独立、完整复制、公共 100 + 分叉 20/30、facts 缺失/冲突和多来源同账户 quota 的合成例子列出 A/C 差异；记录放弃全局唯一总量的价值取舍。列明 CLI 默认、旧 `--source all`、集合 schema、UI 保存状态/逻辑 ID、partial/错误/退出码与回滚选项，不能以待实施时决定代替。缺真实使用证据时明确未知，不增加遥测或读取用户历史。 | 交付草案不等于选择 A。用户明确选择 A 并批准兼容规则后才能冻结 A0、改产品或删 facts；若选择保留 C，结束删除路线，不强推瘦身。 |
| A4 兼容/删除依赖草案 | 为新旧中心/agent、同机旧 recorder/TUI/CLI、中断重启、旧二进制重开新状态和备份恢复逐格提出支持或安全拒绝策略。把最后消费者与 A1—A4 对应，明确哪些中心逻辑可先退役、哪些 v5/shared 职责必须保留。验证方式具体到 spy、旧二进制夹具或恢复场景；不猜协议版本、不实际迁移数据。 | 延续第 9 节 A4 独立兼容边界；真实用户历史操作仍需明确授权及可用备份。 |
| SQLite 原型任务书 | 复用第 10 节门槛，明确 P1 的一次本地 observation、合成数据集、成功/停止条件、依赖与安全核对、同等持久性比较及可删除机制；P2 只候选一个远端增量页。A/C 未定时排除依赖 facts 去留的数据。 | 只准备任务书，不添加 rusqlite、不建原型后端。明确委托后才执行 P1；P1 结果支持且委托范围包含 P2 时才继续；生产切换、真实数据导入另行决定。 |

A 的产品决策和 SQLite 的本地原型委托可以独立发生；不得要求先采用 SQLite 才选 A，也不得把批准 A 当作批准 SQLite。N4 不重复撰写整套架构文档，结果增补到第 9、10 节相关决策记录即可。

A 获批后的实施仍按 A0 → A1/A2 → A3 → A4 → A5 控制依赖。A1 消费者迁移与 A2 停止 facts 的改动可以分工准备，但在旧 All 消费者仍依赖 facts 时不能单独启用 A2；共享 `cli.rs` 接入由主 agent 统一。复用 M3 的准备一次/多次查询和共享 quota，不再循环完整报告准备入口；v5 的 handler/exporter/wire DTO 等到 A4 才统一退役。

### 12.6 N5：平台补证及继续暂缓项

具备相应原生环境并获准使用后，补 Unix 新旧锁互操作、复制句柄/独立读者释放、私有文件替换与目录同步；macOS 补真实 launchd 接受/读取生成 plist 的行为。真实服务安装/卸载只在明确授权的隔离环境执行。记录平台/架构、精确源码、命令、结果和日志；交叉编译、纯 XML 测试不能顶替原生运行。当前 Windows 本机继续原生验证，不运行 Docker；没有环境就保留缺口，不自动访问远端或触发 CI。

R7 的 `tui-input`/`gix-url` 原型、R8 日志/通知/进程等库替换、唯一 daemon/IPC、真实历史基准和断电持久性实验仍不排入默认下一阶段。今后有具体需求和可验证收益再单列；未执行不写成验证通过或既有缺陷。

### 12.7 并行与下一次验收

主 agent 维护基线/证据、依赖及文档；N1、N2 分属两个不重叠文件集合，N4 可由第三名 agent 只做材料。N2 的正式测量等待 N1 环境配置稳定，期间可准备合成场景；测量与重测试不要并发争抢机器。若根因跨越文件所有权，先交主 agent 协调再修改，禁止双方同时编辑公共接口或依赖文件。

各包在迭代中只跑目标测试；主 agent 在源码稳定后统一跑一次相关 Windows 完整入口与组合复审。沿用未变源码的既有证据，新增修复按影响补验；只改规划文档不跑 Rust 全量。按第 11 节格式追加实际完成/未完成项、完整命令、源码身份、失败分类、净成本及尚未打开的门。提交、push、CI、发布分别遵照当时委托；当前仅有本地提交与规划授权，没有后续外部操作授权。

[E1]: https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence "semver 的升级优先级比较"
[E2]: https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock "标准库锁、竞争错误与句柄生命周期"
[E3]: https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist "persist 不等于完整持久化发布"
[E4]: https://www.sqlite.org/wal.html "SQLite WAL 与 WAL-reset 修复；2026-09-26 核对"
