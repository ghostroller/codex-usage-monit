# 项目瘦身与外部库复用提案：审核意见

日期：2026-09-26  
仓库：`ghostroller/codex-usage-monit`  
审核对象：[`refactoring-proposal-2026-09-26.zh-CN.md`](refactoring-proposal-2026-09-26.zh-CN.md)  
配套执行文件：[`refactoring-execution-plan-2026-09-26.zh-CN.md`](refactoring-execution-plan-2026-09-26.zh-CN.md)

> **审核结论：修订后通过。不建议按照原提案的顺序，把所有候选建议依次实施。**
>
> 优先推进有明确边界的机制替换和应用层解耦；跨源归并退役属于产品决策，SQLite 全面迁移属于架构决策，均不因本审核文件入库而自动获得实施授权。

## 1. 审核基线与证据边界

本文件整理前一轮仓库静态审查结果，并针对交接需要复核了原提案、CLI 历史报告入口、账户 quota 查询路径和关键官方 API。

| 项目 | 记录 |
| --- | --- |
| 审查读取提交 | `abef359eac136a2b004c7e4b98890cee27bd37fe` |
| 原提案记录的代码基线 | `1c153229d5c167a0b02eca12fbc6ce4c4770e26f` |
| 原提案记录的版本及工具链 | 0.5.2；Rust 1.97.0 |
| 主要审查链路 | 报告加载、TUI 历史查询、副本归并、facts follow-up、本地提交恢复、远端 generation 发布、私有文件、更新与 Windows 平台封装 |
| 已完成 | 源码及设计静态审查；关键第三方接口资料核对 |
| 未完成 | Rust 编译、测试、Clippy、三平台原生运行、故障注入执行、性能基准、GitHub CI |

版本与统计口径来自原提案，不应被解释为本文件对未来 HEAD 的重新统计。[S1]

文中的“已核实”指审查基线的源码事实；“风险推导”指由调用关系推导出的潜在问题；“实施要求”是建议加入的验收条件。**风险推导不等于已经复现的缺陷，新增验收条件也不等于现有实现已被证明不安全。** 本地 agent 开工时应针对实际 HEAD 复核相关符号，不机械套用旧行号。

## 2. 总体判断与逐项决定

原提案正确地区分了产品语义、存储机制、平台生命周期以及展示与编排耦合。整体复杂度不能仅靠替换小型 parser 或拆分文件解决；大幅缩减维护责任，需要停止支持一项复杂能力，或将一类通用机制交给成熟组件。[S1]

需要纠正的主要是实施顺序和边界，而不是推翻候选库清单。

| 编号 | 审核意见 | 实施边界 |
| --- | --- | --- |
| R1：`semver`、标准库文件锁 | 通过，优先推进，分别提交 | 保持升级身份判断及锁生命周期；补混合版本锁互操作测试 |
| R2：Windows 三项迁移 | 维持“已完成” | 不重复计入收益，不为清零 raw API 扩大范围 |
| R3：来源隔离 A 方案 | 方向有条件支持；删除实施尚未批准 | 先完成产品决策、删除依赖清单、升级及回滚矩阵 |
| R4：应用查询与 TUI 解耦 | 通过，前移，可独立于 R3 实施 | 先分离查询和写入编排；第一阶段保留外部语义 |
| R5：XML、私有临时文件 | 通过，限定范围 | XML 先改解析；临时文件按契约归类，不全量替换 |
| R6：SQLite / `rusqlite` | 支持有界原型，不批准全面迁移 | 原型仍需被明确列入任务；通过原型不等于允许替换生产存储 |
| R7：`tui-input`、`gix-url` | 本轮默认暂缓 | 有实际需求、兼容证明及净维护收益时再启动 |
| R8：日志、通知、进程等候选 | 保持按需求评估 | 不作为本轮默认工作包，不一次引入大量依赖 |

这里的“通过”是技术审核意见，不是向 agent 下达执行所有章节的命令。实际执行范围以用户委托及配套执行文件为准。

## 3. 首要修改：R4/C1 的查询与副作用边界

### 3.1 旧报告加载器不能直接按来源循环调用

**已核实：** `src/cli.rs::collect_and_load_report_history_selected` 不只是查询。它建立历史运行环境，重新验证写权限，暂存本地 observation，在允许写入时执行 `flush_staged()`，然后才加载指定来源的历史。[S2]

因此，原提案 C1 中“复用精确来源查询”应改为：

> 复用精确来源的查询、过滤与计算语义，不复用整个带采集暂存和提交副作用的报告加载流程。

**风险推导：** 对 N 个来源循环调用旧报告加载器，会重复进行环境建立、暂存、提交尝试、诊断和权限处理。这不是已经证明的重复计费，但会扩大副作用和错误处理面。

建议的内部边界如下，名称仅表达职责，不强制具体类型或文件名：

```text
一次应用请求
  ├─ 明确的准备阶段
  │    ├─ 按请求需要接收/采集 observation
  │    ├─ 按原有权限、节流和恢复规则暂存/提交
  │    └─ 在实际写入边界继续执行必要的权限与所有权重验证
  ├─ 一次查询尝试的共享上下文
  │    ├─ 查询时刻、时间范围、来源策略、项目映射
  │    ├─ 账户 quota 及其独立状态
  │    └─ 原有读取预算、隐私与一致性约束
  ├─ 读取所选来源的用量与状态
  │    └─ 保留各来源 revision、partial、失败原因
  └─ 由兼容适配层交给 CLI / TUI 渲染
```

“准备一次”不意味着取消写前验证，也不意味着取消必要的一致性重试。来源策略或 revision 在读取期间改变时，仍应按原机制进行有界重试；新尝试要重新建立有效上下文，不能跨权限或 revision 复用失效缓存。

### 3.2 quota 应共享计算，并有独立的失败边界

**已核实：** `history_query.rs::load_v2_history_since_inner` 在筛选 token 来源前加载账户 quota，并遍历明确同账户、纳入且未 detached 的远端来源合并额度观察。该逻辑独立于 token selector，加载错误可以通过 `?` 传播。[S3]

**风险推导：** 新多来源报告逐个调用整个旧查询时，可能重复执行账户额度加载；若有 N 个报告来源、Q 个参与额度观察的来源，可能产生接近 N×Q 次相关来源读取调用。这里指调用结构，不是已经测得的磁盘 I/O 次数或性能下降。

同样，某一额度来源的读取错误可能中断其他来源的 token 查询。因此，单纯把返回值改成 `Vec<SourceReport>` 不能保证单源故障隔离。

建议区分：

| 层级 | 目标处理 |
| --- | --- |
| 无法建立可信查询环境、权限或必要一致性条件不满足 | 整体失败或按既有只读降级策略处理，不制造成功数据 |
| 单个来源用量失败 | 在已批准的新集合语义中保留其他来源成功结果，集合标记不完整 |
| 账户 quota 失败 | quota 单独标记不可用/不完整，不凭空生成额度，不应抹掉可独立读取的 token 结果 |
| 无数据、离线、过期、partial | 分开表达，不能都映射成“0 且完整” |

**第一阶段只重构内部边界，保持现有 CLI/JSON、退出码和可见错误处理。** 新的部分成功呈现、quota 错误展示、集合 schema 和退出码变化，需要另有明确的行为测试和变更记录；不能借 R4 静默上线 R3 的语义。

### 3.3 R4 应独立前移，而不是等待选择 A

前轮审查发现 TUI 还承担来源元数据加载、revision 缓存键、历史查询及暂存/降级等应用编排。代表入口是 `src/tui.rs::load_remote_overview_history`。[S4]

无论选择 A 还是继续 C，应用层边界都能降低修改影响范围。建议先从共用报告入口及一个 TUI 消费路径建立可复用边界，再迁移具有相同职责的调用点。不要同时建设庞大的 trait 层、workspace 拆分、事件总线或通用框架。

注意：共享查询上下文只提供同一次查询的策略与时刻边界，并记录每源 revision，**不承诺多台远端机器在同一物理时刻形成分布式一致快照。**

## 4. R3：A 方案是产品收窄，不是保持行为的重构

### 4.1 可接受的取舍

当前事件归并不是简单求和或取最大值：它检查证据覆盖，按事件 ID 合并，并在内容冲突时依据证据选择事件提供方、保留限制说明。[S5]

若项目目标明确收窄为“多机各自观察到的日常用量”，A 是合理方向；若复制、迁移、分叉会话之后的可信唯一总量仍是核心需求，应保留 C。

| 示例：公共历史 100，两个来源分别新增 20、30 | 结果语义 |
| --- | --- |
| C，且证据充分 | 唯一事件用量 150 |
| A | 来源 A 记录 120；来源 B 记录 130 |
| 可选未去重记录合计 | 250，必须显式标注未跨源去重 |
| 取最大值 | 130，不能冒充全局精确总量 |

`NodeId` 表示记录的观察/导出来源，不证明 token 必然在该机器执行。会话目录复制后，不同来源可能包含同一历史。因此 A 中的数字应称“来源记录的用量”，而非“该机器独占消费”。原提案已经明确此边界，应保留。[S1]

### 4.2 实施前需要增加三个门槛

**门槛一：产品决策有记录。** 记录 A/C 的选择及接受的能力损失。可使用既有代表性数据离线统计候选、补齐和分叉归并情况，帮助判断使用价值；没有数据时应明确承认依据有限，不得虚构使用率。无需为此次决策默认新增遥测或上传用户数据。

**门槛二：建立按符号与生产消费者划分的删除清单。** 至少记录模块/符号、专用或共享、调用者、保留责任、最后消费者及退役阶段。混合职责先拆分，不能根据文件名批量删除。

远端聚合页的 COW、manifest 切换、分页提交、来源 revision、预算和恢复仍有独立用途。原提案中历史与 remote 的体量不是可删除量。[S1][S6]

**门槛三：升级矩阵覆盖同机旧进程。** 除新旧中心与 agent，还必须覆盖旧 recorder/TUI/CLI 仍运行、迁移中断、新格式写入后启动旧二进制等情况。获得迁移锁只能证明迁移期间的互斥，不能自动证明旧程序之后不会重新写入。

处理方式应基于当前所有权、格式和进程机制确定；不能仅添加一个旧二进制根本不认识的新标志就声称能够阻断旧写入者。

### 4.3 明确保留，不随 facts 一起删除

聚合 delta、分页/游标、SSH 与 agent 生命周期、来源身份、revision/tombstone、隐私与脱敏、预算和健康状态继续保留。项目映射仍携带来源；父会话与 subagent 的独有用量不是副本重复。账户 quota 的同账户观察合并不能被删除，百分比也不能相加。[S1]

C2/C3 应只退役已失去消费者的中心 facts 路径；严格匹配 v5 的旧服务端 handler、wire DTO 和物化依赖要保持到协调协议切换。`finalize_remote_sync_attempt` 的共用收尾不应被提前返回绕开。[S7]

已有持久化状态可能使用 `deny_unknown_fields`。删除 Rust 字段不能替代旧格式读取 DTO、显式迁移或受锁控制的清理。磁盘破坏性迁移之前，必须确定备份/重建或导出方案；不能把“不再读取 facts”写成“旧二进制可以无损回滚”。[S1]

## 5. R1、R2、R5：具体库替换意见

### 5.1 `semver`：通过

审查基线的 `compare_versions` 在验证前丢弃 `+` 之后的内容，并接受部分非法前导零形式。原提案对这一解析偏离的定位成立；尚未证明正常发布链路能够利用它绕过升级保护。[S8]

建议采用 `Version::parse` 与 `cmp_precedence`；后者忽略 build metadata，而默认完整排序不适合直接替代这里的升级优先级比较。源码 build 身份及同版本冲突继续由既有业务规则判断。[E1]

必须测试非法主版本/预发布前导零、空或非法 metadata、长数字预发布段、相同优先级不同 metadata、同版本不同源码 build、降级拒绝。对原先误接受的非法输入进行拒绝，是有意收紧，不要求继续兼容。

### 5.2 标准库文件锁：通过，补升级互操作验证

项目基线工具链已具备标准库锁 API。迁移应保留 `FileLock` 及显式 `unlock`；复制/继承句柄仍存活时，单纯 drop 一个 `File` 不等于立即释放锁。[S9][E2]

除竞争、共享/独占、错误释放和复制句柄测试外，增加**旧 `fs2` 进程与新标准库锁进程，对同一锁文件的双向竞争测试**。这是升级契约验证，不是已发现二者不兼容。

必须保持锁文件路径、打开权限、Windows sharing、获得锁后的文件身份验证及错误分类。`WouldBlock` 与真实 I/O 错误要分开，不要复制多套新的 OS 错误码判断。

若互操作 helper 暂时仍使用 `fs2` 作为测试依赖，报告应区分“生产依赖退役”和“仓库完全无此依赖”，不能夸大删除收益。

### 5.3 Windows 三项迁移：已完成，不重复排期

前轮核实了 `Zeroizing` 密码缓冲、`windows-service` 常规服务管理及 `winreg` 句柄包装。原提案已准确更新状态。[S1][S10]

剩余原生边界具有具体契约：密码缓冲与 ImagePath、局部变更的 NULL 语义、注册表有界读取和原始类型/字节等。不应为清零所有 `unsafe` 或 raw API 再次扩大改动。

回归关注点保留：密码输入上限与生命周期、SCM 子进程 Job 归属、HKCU/所有权、注册表读取上限与重试、compare-write 二次读取。这里是保留要求，不是新增迁移任务。

### 5.4 XML：支持先替换解析，生成端暂不联动

launchd plist 存在字符串定位式字段读取；项目已有 `quick-xml`。可以复用现有依赖建立有结构约束的解析，但它不能自动替代 plist 字段语义检查。[S11]

应验证字典层级、键值配对、重复键、错误类型、错误嵌套、转义和输入边界。先固定正常服务定义和指纹的黄金样本，再改解析；不要顺手更换生成方式、空白或转义输出。`launchctl print` 是另一条文本解析路径，不纳入本次 XML 替换。

不同写法的合法 XML 可以解析为相同结构，但持久化指纹是否按原始字节计算是另一项契约。生成端确需修改时，应另行提交并给出字节级或指纹级兼容证明。

### 5.5 私有临时文件：按契约统一，而非全量 `persist`

当前私有发布包含安全创建、写入、同步、替换、实际文件验证及目录同步。部分 `.target.pid.sequence.tmp` 文件名还参与崩溃恢复。[S12]

`NamedTempFile::persist` 不负责文件内容和父目录的持久化同步，不能替代整套发布流程。[E3]

建议区分普通临时文件、可恢复历史发布和安装升级候选文件。先对策略相同的调用点做共享封装，再判断是否需要将 `tempfile` 引入生产依赖。不要创建携带大量布尔选项的万能写文件接口，也不要把 RAII 清理当作进程崩溃后的恢复机制。

## 6. R6：SQLite 仅进入有界原型候选

### 6.1 原型价值与限制

本地历史已经维护 redo、多数据族提交与恢复，远端维护 immutable generation 与 manifest 发布。数据库可能接管其中一部分通用机制，但不能取代来源身份、分页提交、quota provenance、精确金额或 profile lease 等业务契约。[S6][S13]

当前实现已有恢复措施。本审核没有证明这些措施失效；SQLite 是维护成本与机制复用评估，不是“当前历史必然损坏”的修复结论。

原型首先覆盖一次本地 observation 的多数据族一致提交、重启恢复和一致读取。未批准生产迁移时，只使用隔离测试目录及合成/已授权脱敏样本，不修改真实历史，也不新增默认生产后端。

### 6.2 加入必须通过的原型门槛

| 门槛 | 要回答的问题 |
| --- | --- |
| 实际底层版本 | 固定 Rust 包版本、features、实际 SQLite 版本与链接方式；确认包含适用修复 |
| 文件安全 | 数据库、WAL、SHM、目录权限和路径替换竞争如何满足项目现有威胁模型 |
| revision 与所有权 | 保持现有已保留 revision 的不复用规则、写入所有权和发布边界；普通回滚事务不能被默认视为等价 |
| 持久性 | 在明确且相当的 durability 目标下比较；记录配置，不用更弱持久性换取跑分 |
| 精确数值 | `u128` 编码、边界、排序、聚合和溢出均可验证，不改为 REAL |
| 并发与资源 | 多进程读写、busy 有界策略、短事务、checkpoint、长读者及磁盘预算 |
| 文件系统与平台 | history-root 的支持范围、Windows/MSVC、macOS、Linux/musl 及依赖构建成本 |
| 迁移与恢复 | 旧格式导入、schema 演进、备份一致性、失败恢复和旧版本回退限制 |
| 净维护收益 | 实际可删的 redo/COW/恢复机制与新增 SQL、迁移、适配、测试之差 |

**版本补充：** SQLite 官方记录的 WAL-reset 问题在 WAL 多连接并发写入/检查点场景下可能导致罕见损坏；3.51.3（2026-03-13）及之后版本包含修复，部分较旧维护分支有回补。原型必须核实实际链接版本包含该修复，不能只看 `rusqlite` 的版本号。本项目尚未采用该方案，此处不是现有项目漏洞报告。[E4]

当前私有文件层验证实际打开对象及路径/目录身份。数据库按路径打开之后，不能仅靠一次打开前检查便宣称与原机制等价。先证明现成接口满足所需边界，不应预设要自写复杂 VFS。[S12]

现有本地观察流程会持久化保留 revision，并允许中断造成编号空洞而非重复发放。原型应复用对应故障测试，验证业务可见的编号规则；不能只展示“几个表一起提交成功”。[S13]

本地原型通过后，才考虑一个**远端增量页**的第二阶段原型，验证 expected-active、页身份、游标和数据提交原子性、重复执行及中断恢复。不得由本地原型成功直接推导可以删除全部远端 generation。[S6]

## 7. R7/R8：本轮保持克制

### `tui-input`

现有文本编辑已使用 Unicode 字素簇与显示宽度能力；仓库还规定插入 ZWJ 后光标应对齐合并字素簇末尾。原提案记录的候选实现存在插入后的坐标适配差异。[S1][S14]

即使采用输入库，居中窗口、取消恢复、鼠标命中和快捷键优先级仍需保留。当前没有足够净收益证据，建议本轮不启动原型。未来重评时核对实际候选版本，不把旧上游行为当作永久事实。

### `gix-url`

当前 remote 规范化结果进入持久化 `git-sha256-v1` 指纹。解析库最多接管结构解析，不能让其 Display、解码或默认规范化规则直接定义项目身份；错误输出也不能泄露凭据或原始敏感 URL。[S15]

没有新增 Git URL 需求或明确净维护收益时暂缓。确需改变指纹语义，应作为独立版本和迁移决策。

### `tracing` 及其他候选

现有 `TraceFields` 是限制自由字符串、保留静态标签/计数/摘要的隐私边界，而非可直接删除的 logging 包装。[S16]

`tracing` 可以成为底层机制候选，但不应允许业务代码绕过受限门面自由记录路径、命令、URL 或内容。其余 `notify`、进程/下载库、通用更新器、数值或 RPC 方案维持原提案“按实际需求再评估”的结论，不作为本轮瘦身默认范围。[S1]

## 8. 原提案必须同步修订的部分

| 修订编号 | 原文位置 | 修改要求 |
| --- | --- | --- |
| D1 | 第 1、7 节优先级 | R4 可独立前移；R7 默认暂缓；R2 不再计入未来收益 |
| D2 | 第 4 节 C1 | 明确一次准备、共享查询上下文、quota 独立结果；禁止按源循环旧完整加载器 |
| D3 | 第 4 节 C0/C1 | 区分保持外部行为的内部重构，与集合报告/退出码/错误展示的产品变更 |
| D4 | 第 4 节 C0/C3 | 加删除依赖清单：专用/共享、最后生产消费者、退役阶段 |
| D5 | 第 4 节 C4 | 加中心/agent、同机旧进程、迁移中断、回滚兼容矩阵 |
| D6 | 第 5 节文件锁 | 加旧 `fs2` 与新 std 的双向跨进程验证及测试依赖披露 |
| D7 | 第 5 节 XML/临时文件 | XML 解析与生成分离；临时文件按相同安全/恢复契约合并 |
| D8 | 第 5、7 节 SQLite | 实际版本、文件安全、revision、持久性、精确数值等提升为退出条件；全面迁移另行批准 |

修订不能覆盖或伪装历史证据：旧快照统计保留原日期/commit，新测量另列；“已完成”必须有实际实现与验证记录，不能把计划文字改成完成状态。

## 9. 最终意见

优先做 `semver`、标准库锁和应用边界整理，XML/私有文件保持小范围。A/C 另作产品决定；SQLite 以原型证明可替代的机制，再讨论生产迁移。不要用拆文件、删测试、移除安全校验或引入长期双引擎制造表面上的瘦身。

验收应同时看维护责任、行为兼容、运行资源和依赖成本。**最应先修正的是 R4/C1；最需要独立决定的是 R3；最值得验证但不能提前承诺收益的是 R6。**

## 10. 证据与官方资料

源码链接固定在审查提交；符号是本地 agent 复核入口。除明确链接的范围外，不应推断相邻代码已被完整覆盖。

[S1]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/docs/refactoring-proposal-2026-09-26.zh-CN.md "原提案、版本与统计口径、产品边界"
[S2]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/cli.rs#L3764-L3970 "collect_and_load_report_history_selected 与报告环境"
[S3]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/history_query.rs#L650-L800 "load_v2_history_since_inner 的账户与远端 quota 读取"
[S4]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/tui.rs "load_remote_overview_history 等 TUI 编排"
[S5]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/history_query/reconciliation.rs "plan_replica_resolution 与证据归并"
[S6]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/source_history/remote_generation.rs "远端 generation 和页发布"
[S7]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/remote_sync_attempt.rs "finalize_remote_sync_attempt 共用收尾"
[S8]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/update.rs "compare_versions 与升级判断"
[S9]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/file_lock.rs "FileLock 与显式释放"
[S10]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/windows_scm.rs "已落地的密码和服务包装"
[S11]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/service.rs "launchd plist 解析与生成"
[S12]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/private_state_store.rs "私有文件、锁后验证与发布"
[S13]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/source_history/local_observation.rs "本地 revision、redo、多数据族提交与恢复"
[S14]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/tui/text.rs "字素簇编辑原语"
[S15]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/git_repository.rs "normalized_git_remote 与 git-sha256-v1"
[S16]: https://github.com/ghostroller/codex-usage-monit/blob/abef359eac136a2b004c7e4b98890cee27bd37fe/src/trace.rs "TraceFields 与诊断隐私边界"
[E1]: https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence "semver：忽略 build metadata 的优先级"
[E2]: https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock "Rust 标准库锁 API 与句柄生命周期"
[E3]: https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist "tempfile persist 的能力边界"
[E4]: https://www.sqlite.org/wal.html "SQLite WAL、持久性与 WAL-reset 修复信息；2026-09-26 核对"
