# 历史存储重写执行方案（2026-10-02）

## 1. 决定与实施范围

用户本次明确决定不采用 A 方案，不通过删除去重功能减负，优先重写存储机制，并授权在新分支实现。本方案是本次任务的现行口径；2026-09-26 的提案、审核和执行方案保留为历史记录，其中 A 退役路线不再推进，SQLite 仍未获委托的描述不适用于本次任务。

保留现有 C 产品语义：本地/远端精确来源、AllIncluded 跨源归并、复制/分叉识别、facts 补齐、digest proof、完整性/partial、quota provenance、金额精度、报告 JSON 和退出码。CLI/TUI 共用编排继续复用。目标是将历史持久化的事务、一致读取和恢复责任交给 SQLite，而不是减少业务能力。

基线为 `10ddd7b`（保存此前完成的 CLI/TUI 编排收敛），新分支为 `codex/sqlite-history-storage`。开发和验证使用隔离的合成数据与旧格式夹具；不运行用户真实 history-root 的迁移、真实 SSH 或服务部署。本任务包括生产接入和迁移代码，验证通过后无需再以旧原型决策门请求重复授权。

## 2. 存储边界

本批重写 `SourceHistoryStore` 所管理的历史数据与中心摄取状态：account quota、bucket、weekly、session digest、来源 metadata、revision/回填/保留状态、远端 active/staging 数据、quota/live、摄取游标与页身份，以及完整 session facts 和其激活状态。

来源身份、ownership/profile lease、remotes 配置、项目映射、带宽/health、服务和安装升级状态保持现有责任。远端 agent 的协议导出 journal/分页快照也保持既有协议职责；这些不是中心历史数据库，不将它们计作已替换。底层存储重写不改变 v5 wire DTO 或价格 catalog。

保留 `SourceHistoryStore` / `SourceHistoryWriter` 和既有业务 DTO 作为接入边界。SQLite 按记录存储和索引，不把完整 JSON 分片搬进一个 BLOB 后继续运行原来的 redo/COW 引擎。复杂 payload 可以采用经既有校验的无损记录编码；索引字段、行身份与事务发布由数据库管理，金额继续由 Rust 受检精确计算。

## 3. 必须保持的契约

| 契约 | 实现要求 |
| --- | --- |
| revision 不复用 | 独立持久化保留高水位，再提交数据事务；失败允许空洞，不回滚已经发放的编号。超出 SQLite 有符号整数范围的业务编号使用无损编码 |
| 本地共同发布 | account、bucket、weekly、digest 与 metadata 同事务；不完整扫描、partial、reconcile tombstone、quota reset 漂移及同 revision 冲突规则不变 |
| 远端页共同发布 | 页数据、quota/live、页身份、游标和激活状态同成同败；重复页幂等，失败不推进游标 |
| bootstrap 可见性 | 多页候选在完成前不可见；保留 logical generation、exact binding、expected-active CAS 和最终激活，不依赖复制目录发布 |
| facts 与去重 | 完整 facts batch、validated digest proof、来源 binding、active version/CAS、分页 cursor 和 reconciliation 规则保留；候选准备与激活分开 |
| 一致读取 | 一次应用查询在同一数据库读事务中读取所需数据族；外部 ownership、profile、来源策略和项目映射仍有前后 fence 与有界重试 |
| 权限和对象安全 | 数据库、WAL、SHM 与父目录保持私有权限，拒绝符号链接/对象替换和不可信 schema；不能仅验证一次打开前路径就宣称等价 |
| 资源有界 | 沿用源数量、记录和解码字节预算；写事务短、busy 等待有界，网络及长解析在事务外；记录 checkpoint 和文件增长策略 |
| 旧进程 | backend/ownership 发布必须让旧二进制明确拒绝写入；不能让 JSON 与 SQLite 在切换后成为两套权威状态 |

SQLite 起始配置采用 WAL、`synchronous=FULL`、外键和有界 busy 策略；实际耐久性、侧文件安全和平台支持以测试为准。数据库锁不替代应用租约。SQL 只查询和索引精确编码，不把 `u128` 金额转换为 REAL 或普通 SQL SUM。

## 4. 工作包与顺序

| 工作包 | 工作与产物 | 完成条件 |
| --- | --- | --- |
| S0：规划与基线 | 保存编排基础、新分支、冻结业务与历史格式契约；建立本方案 | 范围明确，A 删除路线退出，去重回归清单保留 |
| S1：数据库基础 | 选择并锁定 Rust 包/实际 SQLite，schema、私有打开、读写事务、精确编码、busy/checkpoint 与错误映射 | 实际版本/编译选项可记录；安全、事务、数值和多进程回归通过 |
| S2：本地 observation | 接入 account/bucket/weekly/digest/metadata；独立 revision 保留、marker 和 retention | 同批一致读取、中断恢复、不复用编号、partial/reconcile/quota 规则通过 |
| S3：远端聚合摄取 | 接入 active/staging、remote quota/live、页状态与 cursor，替代跨文件 WAL/COW 发布 | expected-active、重复页、失败页、generation 改变及 bootstrap 中断回归通过 |
| S4：facts 与查询 | 完整 batch staging/激活、proof/cursor；统一 SQL snapshot，沿用现有 C reconciliation | 独立、复制、分叉、缺失 facts、同 revision 冲突、partial 和 exact 查询结果不变 |
| S5：迁移与生产接入 | 受 ownership/profile/service 协调的旧格式导入、校验、backend/epoch 发布、明确失败恢复 | 导入全部已承诺状态，包括 pending；旧进程拒绝、失败不回写旧 JSON；保留可恢复旧副本 |
| S6：退役已替换机制 | 删除无生产消费者的本地 redo、每页物理 COW 和跨文件游标补偿；保留必要旧格式读取/迁移 | 无永久双写/双引擎，无删除去重专用能力；记录实际维护责任差额 |
| S7：组合验证与交付 | 原生 macOS、Docker Linux、UTM Windows 的相关完整检查、独立审查、文档和源码证据 | 结果绑定最终源码；失败/未执行项如实记录，不以旧批次替代 |

S1 统一管理数据库接口与依赖；S2、S3、S4 可在接口冻结后按不同模块并行。迁移及 ownership 发布由主 agent 统一。影响范围测试在修改中执行，平台完整检查在批次稳定后执行。既有 Windows 启动器清理失败独立保留诊断，不通过重跑或提高超时掩盖。

## 5. 迁移与恢复

旧 v1/v2 文件存储仅作为迁移输入和备份，不新增永久用户可选后端。迁移先取得协调权并验证确切 profile/ownership，处理已经持久化的本地 pending observation、远端 pending 页与待激活 bootstrap；导入所有来源的 metadata、数据、facts、proof、cursor、quota/live 和 revision 高水位。

先将既有 V2 namespace 的 ownership manifest 升为版本 2、`Migrating` 并递增 epoch，使旧写者在导入前就拒绝操作。随后在一个数据库事务中导入状态并保存校验 receipt，最后发布 `V2Active`。数据库提交后、ownership 激活前中断，使用 receipt 恢复而不重读已过时的备份；事务提交前失败可重新导入。后续激活另一个 privacy namespace 时，保留更高 revision、当前来源策略、facts/cursor、GC 截止点以及已完成 purge/retire 的结果，不能借旧副本复活数据。

在已切换后数据库缺失、空文件或不可信 schema 均明确失败，不能重建空库或静默退回旧 JSON。保留旧副本的回退是明确操作，不声称新数据可无损转换给旧二进制。首版 schema 为 1，拒绝不支持的版本；本批未增加自动回退或备份 CLI。SQLite 一致备份必须包含 WAL 中已提交的状态，不能直接复制正在使用的主数据库文件作为完整备份。

同一来源的两个 privacy namespace 若同时存在旧本地 pending journal，而旧格式没有可证明的发布顺序，迁移拒绝猜测顺序。保留两个输入并报错，修复有歧义的旧状态后再重试；不通过任意挑选一份 journal 来宣布迁移成功。

## 6. 验证与收益记录

本批不承诺固定提速或减行百分比，以业务、安全和平台契约验证作为交付条件。性能基准留作后续测量：用固定合成档位比较同等持久性下的首次打开、重复读取、提交、磁盘增长/写放大和构建成本，记录原始样本、失败数、配置和环境；未控制 OS 缓存不称真正冷缓存。

收益按实际删除的责任记录：本地 redo/多族补偿、每页整代文件复制、文件 manifest 发布、数据与游标的跨文件恢复，以及减少的分片读改写。新增 SQL/schema、迁移、数据库权限、checkpoint/忙等待和平台依赖的维护成本同时计入。不以只移动代码或删除测试计作净减负。

相关回归除新增数据库契约外，保留既有 replica/facts、Summary/Trends/Health、CLI JSON/退出码、TUI 键鼠/compact、profile/redaction/只读及真实 PTY/ConPTY。平台命令和证据按 [testing.md](testing.md)；记录 commit、dirty snapshot、架构、完整命令、日志、结果与跳过项。最后阶段只在有具体本地证据的集成检查点考虑 hosted CI，不创建测试 tag。

## 7. 当前实施记录

- 2026-10-02：本次用户决定和生产实施范围已记录；`10ddd7b` 保存编排基础，已创建 `codex/sqlite-history-storage`。
- 已锁定 `rusqlite =0.40.2`（`bundled`、`limits`），实际链接 `libsqlite3-sys 0.38.2` 内的 SQLite **3.53.2**，source ID 为 `2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24`。该版本包含 WAL-reset 修复；不将 Rust crate 版本当作 SQLite 版本，也不将它称作最新 SQLite。[实际 bundled 源码](https://docs.rs/crate/libsqlite3-sys/0.38.2/source/sqlite3/sqlite3.h)与[官方发布记录](https://www.sqlite.org/changes.html)已核对。
- macOS 构建通过实际链接的 `rusqlite` 查询 `sqlite_version()`、`sqlite_source_id()` 和 `PRAGMA compile_options`，完整输出位于 `target/sqlite-storage-2026-10-02/sqlite-build-info.log`；含 `THREADSAFE=1`、`ENABLE_API_ARMOR` 和 `MUTEX_PTHREADS`。编译默认 checkpoint 为 1000 页，应用明确覆盖为 256 页；编译长度上限也由数据库打开时收紧至 128 MiB，不能将编译默认值当成实际运行配置。
- 数据库位于 `history-v2/{profile}/history.sqlite3`，使用两张按记录存储的 STRICT/WITHOUT ROWID 表及时间/metadata 索引。普通 busy 上限 250ms；config fence 内的远端页与 facts 激活采用立即失败的写锁；自动 checkpoint 为 256 页，沿用保留时钟进行逻辑 GC。长期读事务仍可能阻止 WAL 截断；逻辑 GC 不保证立即缩小主文件，未添加每次 GC 的 VACUUM。
- Unix 不对已打开数据库另开/关闭身份检查 fd，以免释放同进程其他连接的 POSIX 锁；采用 SQLite 自身 fd 的 `HAS_MOVED` 和路径对象检查。首次创建的临时 fd 在发布主文件前关闭，空库发布中断可验证并清理自身临时硬链接。Windows 检查实际 SQLite HANDLE、ACL/reparse 和对象身份，使用 create-new 发布。
- S1—S5 的生产路径已接入，冻结前 49 项 SQLite 定向回归及全 target Clippy 通过；这不是最终平台检查证据。独立复审已修正旧 FileV2 的 CLI/TUI/自动同步就绪入口、迁移期间旧 epoch 只读、旧备份复活和空库重建等边界。最终验证结果将补于本节。
- 正常 SQLite 运行已不使用本地 redo 文件、每页物理 COW、active manifest 文件和游标跨文件发布。旧文件实现仍承担旧 V1→FileV2→SQLite 的首次升级及格式夹具验证，未物理删除全部旧实现，不称净代码量下降；既有 agent 导出 journal 保留。尚未进行同耐久性性能基准，不给出提速或百分比减负结论。
- 本地 revision 高水位与已提交数据版本分开保存：预留编号单独提交，projection stamp 随数据事务发布，避免跨 selector 缓存把「编号已预留、数据尚未提交」当作新快照。迁移初始化中断可恢复自己的空库发布；已激活的丢库/空库仍拒绝重建。另一 privacy namespace 首次导入只更新该新本地 namespace 的 policy，重复旧副本不能覆盖当前策略。
- SQL 查询和外层 TUI projection 缓存同时监测两种 privacy 的已提交版本：配额按 profile 共享，另一 privacy 写者提交后也必须失效。远端 generation 枚举只解码 `generation.json`，不会将同 namespace 的 quota header 混读为 generation；回归覆盖带配额的多页 bootstrap、增量重放、tombstone 与代清理。

## 8. 参考

- [此前提案](refactoring-proposal-2026-09-26.zh-CN.md)、[审核](refactoring-review-2026-09-26.zh-CN.md)、[执行与验证记录](refactoring-execution-plan-2026-09-26.zh-CN.md)。
- [SQLite WAL](https://www.sqlite.org/wal.html)、[SQLite 发布记录](https://www.sqlite.org/changes.html)、[SQLite 类型](https://www.sqlite.org/datatype3.html)、[SQLite 损坏规避与锁](https://www.sqlite.org/howtocorrupt.html)。候选依赖必须重新核对实际链接版本。
