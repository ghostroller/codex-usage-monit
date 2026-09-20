# Windows 安装、更新与后台运行方案

审查日期：2026-09-21。审查基线：`121facae9b362e7d6feec1a5e451334cb8b2dd35`。
本次从 `39c7033` 快进拉取远端 `main`，审查其间五个提交，重点是 `44a44b1` 的统一更新实现。
本文保留实施前的审查依据与批次顺序。后续实现基线已同步至远端 main 的
`6f3be33`，操作说明见 [Windows 安装指南](windows-installation.md)；
实施后的证据和未验收边界见 [验证记录](windows-lifecycle-verification-20260921.md)。
历史审查结论中的代码行号对应原审查基线，不表示当前实现仍有相同缺陷。

建议保留现有的不可变版本目录、稳定 CLI launcher 和统一目标机更新器，补齐 Windows 用户安装生命周期。
默认产品形态是**免管理员安装、用户 PATH 注册、登录后无窗口运行的用户计划任务**。
无用户登录的 Windows 服务器需要单独的服务模式，不能把现有登录任务宣称为开机常驻服务。

## 审查结论与证据

这批实现已经解决了大部分跨平台更新内核问题：本地和 SSH 共用目标机 `update apply`；
先校验并准备不可变版本；更新已有 recorder 时保留配置和启用状态；用新进程的真实历史心跳验证；
保留节点与服务两层 journal、互斥与前向恢复；区分 `sync` 和 `node` 范围。
这些设计应继续沿用。`src/service.rs` 本批仅增加两行，Windows 服务模型基本延续旧实现。

| 优先级 | 发现、触发条件与影响 | 代码位置与证据边界 |
| --- | --- | --- |
| P1 | 未受管的旧 Windows CLI 自身执行 `update --adopt`，且新旧 binary SHA 不同：旧进程等待 candidate，candidate 又尝试覆盖仍在运行的旧入口，替换失败。已有 recorder 可能先升级完成，随后节点返回 partial；重复同一入口命令仍可能再次遇到文件占用。 | `src/remote_agent_manager.rs:675–696`、`src/update.rs:410–433,632–646,1045–1073`。本机握手启动真实 exe 后，独立微型程序验证 `MoveFileExW` 和 `fs::rename` 均返回 Win32 error 5；这是 OS 机制复现加源码调用链确认，未运行完整 Release 迁移。相同 SHA 的首次注册不会触发替换。文档已提示首次迁移可能要关闭进程，但自己发起更新的进程无法在等待期间自行关闭。 |
| P2 | Windows SSH 配置允许 `tools\codex-usage-monit.exe`，新版路径识别却只认盘符绝对路径、UNC、`.\` 前缀，将此类相对路径加上 POSIX 单引号。原先能运行的命令在 cmd/PowerShell 中失败，影响 agent 调用和依赖它的检查/同步。 | `src/remote_transport.rs:1691–1734` 与 `src/remotes_config.rs:1286–1297`。本机两个 shell 用同形状系统程序路径复现旧命令成功、新命令失败；未做真实 SSH 端到端测试。 |
| P2 | Windows PATH 扫描只找 `.exe`。当前面目录有旧 `.cmd`、后面有受管 `.exe` 时，PowerShell 实际执行前者，更新诊断却可报告未被遮蔽，首次安装发现也会漏掉旧 wrapper。 | `src/update.rs:1078–1118`。本机临时 PATH fixture 验证 PowerShell 选择 `.cmd`，等价的现有 resolver 选择 `.exe`。这不是文档所说的“不同 shell 的 PATH 不同”。 |

还有四项产品能力缺口，不能当作已经实现的 Windows 支持：

- **首次安装未闭环。** Windows README 要求用户手工加 PATH，示例本身没有添加；仓库只发布 Unix `install.sh`，没有正式 Windows bootstrap、安装归属凭据、完整卸载/修复流程。`update` 仅报告 PATH 问题，这一行为符合目前文档，却不能代替安装器。
- **登录边界明确存在。** `src/service.rs:2983` 固定 `InteractiveToken`，只在同一用户有交互会话时运行。SSH 能执行 exporter 不等于计划任务能运行 recorder。对已有 enabled 任务，无登录时应在停止/换注册之前预检，而不是最后等心跳超时。
- **无窗口体验没有保证。** `src/service.rs:3006–3009` 直接启动控制台程序；`src/main.rs` 没有 GUI subsystem host。源码不足以保证后台启动不出现控制台窗口，需实际桌面验收。Task Scheduler 的 Hidden 属性只控制任务可见性，不能用来隐藏控制台。
- **已有 Windows 全套证据的身份有限。** 历史记录是在 ARM64 UTM 上以 SYSTEM 运行 x64 测试；这证明许多原生行为，却不能证明普通用户 Task Scheduler 注册、登录/登出、HKCU PATH 与真实 SSH 会话的端到端生命周期。见 [既有验证记录](update-verification-20260920.md)。

## 同类实现提供的依据

| 官方资料/源码 | 可借鉴的做法 | 本项目的选择 |
| --- | --- | --- |
| [VS Code Windows setup](https://code.visualstudio.com/docs/setup/windows) | 推荐不需要管理员权限的 User setup，并区分系统安装。 | 默认用户安装；沿用现有 `%LOCALAPPDATA%\codex-usage-monit`，此次不因目录审美再做迁移。 |
| [uv 安装/升级](https://docs.astral.sh/uv/getting-started/installation/#upgrading-uv) | standalone 安装可以自更新，其他渠道交给原包管理器。 | 显式记录 ownership，自更新只操作本应用拥有的入口。不能仅靠路径包含 `scoop/chocolatey` 推断所有安装来源。 |
| [rustup Windows self-update 源码](https://github.com/rust-lang/rustup/blob/main/src/cli/self_update/windows.rs) | 更新进程等待父进程退出；PATH 注册通过用户注册表完成。 | 首次旧 exe 迁移和偶发 launcher 升级采用受控 handoff；常规版本更新继续切换现有指针。 |
| [Syncthing 自动启动](https://docs.syncthing.net/users/autostart) | 大多数个人用户采用登录启动；无头服务器单独配置服务。 | 默认保留当前用户计划任务，另行提供服务器模式。 |

Windows 原生限制决定了实现细节：普通用户可以注册自己的最低权限任务，
`InteractiveToken` 要求交互会话；Password/S4U 模式还涉及 batch logon 权利，S4U 对网络与加密文件访问有明确限制。
不能把 S4U 当成保留个人凭据行为的通用替代品。参见
[任务安全上下文](https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks)和
[登录类型](https://learn.microsoft.com/en-us/windows/win32/api/taskschd/ne-taskschd-task_logon_type)。

## 安装与命令注册

沿用以下根目录，保持配置、history、source identity 和旧 agent 引用不变：

```text
%LOCALAPPDATA%\codex-usage-monit\
  bin\codex-usage-monit.exe          稳定 CLI launcher
  versions\<version>-<sha>\          已有不可变业务 binary
  installation.json                 保持现有 launcher 读取协议
  install-receipt.json               新增：安装来源、SID、PATH/卸载归属
  update-journal.json                已有节点恢复记录
  ...                               已有配置与历史原地保留
```

特别注意：`Installation` 和其嵌套类型目前使用严格反序列化（`src/update.rs:154–162` 等）。
老 launcher 每次启动会读取此文件，**不能直接给 installation.json 增加 ownership 等字段**。
新增元数据应放独立 receipt；版本选择协议保持兼容。今后变更 launcher 协议必须先完成 launcher 升级。

提供 `scripts/install.ps1`，支持 Windows PowerShell 5.1/PowerShell 7，薄脚本只负责下载、校验与调用 Rust 安装入口。
更新事务、文件归属、PATH、任务注册等逻辑由 Rust 共用模块承接，本地与远端不维护两套替换算法。
安装器提供版本固定、离线 bundle、禁用 PATH 修改和可选 recorder；普通 `update` 不隐式创建此前没有的后台任务。
新增接口可采用以下形式，最终参数名在实现时定稿：

```text
install --add-to-path --recorder on-logon
install --no-modify-path --recorder none
install repair
uninstall                         # 默认保留历史/用户配置
doctor --format json              # 安装、命令解析与后台身份诊断
```

PATH 注册规则：

1. 读取 HKCU `Environment\Path` 的原始值及类型，只增删自己的目录，保留 `%VAR%`、其他条目、顺序和值类型。
   比较时做 Windows 大小写、引号与尾分隔符规范化；不要把整个进程 `$env:PATH` 写回用户 PATH。
2. 使用注册表 API 或等价无损实现；禁止 `setx PATH ...`，它有 1024 字符裁剪及变量引用展开问题。
   [Microsoft setx](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/setx)。
3. 保存本应用实际添加的条目及归属；卸载时重新读取当前值，仅移除仍可确认归属的条目，不能恢复整份旧 PATH 覆盖用户后续修改。
   对并发编辑在写前重新核对，冲突时重试或给出修复诊断。
4. 有限时地广播 `WM_SETTINGCHANGE("Environment")`；已存在的终端宿主未必刷新环境。
   明确显示“持久 PATH 已注册”和“当前终端仍需重开/刷新”两个结果，不能承诺修改父 shell。
   [Microsoft 环境变更消息](https://learn.microsoft.com/en-us/windows/win32/winmsg/wm-settingchange)。
5. 检查应用名对应的 `.exe/.cmd/.bat` 等 PATHEXT 候选；PowerShell alias/function 属于当前 shell 状态，应提供
   `Get-Command codex-usage-monit -All` 与 `where.exe codex-usage-monit` 的核对提示，不假装子进程知道父 shell 的全部命令解析。
   即使写入 HKCU PATH，系统 PATH 中的同名程序仍可能优先；冲突必须可见。

receipt 记录 owner/channel、安装 SID、规范化根路径、添加的 PATH 条目、任务标识和安装器协议版本。
便携版保持不注册行为；迁移手工入口仍需要明确 `--adopt`。
Cargo/Scoop/Chocolatey 等明确由外部管理的安装交给其管理器；未来 WinGet 接入需按具体 installer 的归属策略决定，
不能把“通过 WinGet 发现”机械等同于“不许自更新”。
正式 User Setup 可再添加 HKCU 卸载注册项和签名安装器，复用同一个 Rust 生命周期实现。

## 更新事务与首次迁移

正常已受管升级维持现有顺序：准备版本 → 获取更新锁 → 预检归属/后台能力 → 写 journal →
协作停止 recorder → 注册新版本绝对路径 → 验证注册与新心跳 → 原子选择 CLI 版本 → 报告组件结果。
任务继续绑定不可变业务版本，不能通过 CLI launcher 的可变选择间接切换后台 writer。

首次迁移增加三种明确结果：

- 目标是新入口：直接创建，验证命令注册；旧手工 exe 原地保留，只有显式选择迁移时才退役。
- 目标是已支持且经验证兼容的旧 launcher：可保留其字节并只建立受管指针，避免不必要替换。
  不能把任意旧手工 exe 都当作支持该协议的 launcher。
- 目标被运行中旧程序占用且必须替换：准备临时 updater，持久化 handoff，等待准确的父进程句柄退出再提交；
  其他 TUI 占用时返回待重启/待关闭状态与恢复命令，不强杀用户前台进程。不要在必然无法换入口时先切 recorder。

handoff 必须处理现有 `bounded_process` 的 Windows Job 生命周期：不能让父进程退出/超时清理把 helper 一起杀掉。
协议需包含已校验的候选身份、事务 ID、父进程句柄/创建身份、明确的接管确认及结果文件；
不采用固定 `sleep`、裸 PID 猜测或拼接批处理。交接成功但尚未提交时应返回 pending，不能输出 complete。
没有父进程可等待的 SSH 重连可按事务 ID 查询/续作。

首次迁移与正常升级共用原来的 updater；helper 只解决 Windows 进程退出后的执行时机。
稳定 launcher 也需要可维护的升级通道，用于协议或安全修复，但无需每次业务版本更新都替换。

恢复策略保持当前的**前向恢复与禁止不兼容降级**。
准备期失败不触碰原 recorder；业务 writer 一旦可能迁移 history，就不能因为心跳失败自动启动旧 binary。
PATH/卸载项等安装元数据可按归属补偿，业务版本回退必须先证明数据协议兼容。
清理旧版本继续使用显式、引用感知的 `update prune`，保留其他中心与人工进程可能引用的版本。

## 后台注册：桌面默认模式

保留当前 SID 隔离的 Task Scheduler，不需要管理员或用户密码。
已有 XML 已正确配置 `LeastPrivilege`、`IgnoreNew`、无限运行时间、电池条件和失败重启；这些不是本次要重新补的缺失项。
新增的工作是无窗口运行、身份/会话预检、生命周期命令与用户可理解的状态。

建议增加很小的 GUI-subsystem `recorder-host.exe`，仅作 Windows 启动适配：

```text
Task Scheduler（当前 SID，InteractiveToken）
  → 固定版本的 recorder-host.exe
    → 同一版本的 codex-usage-monit.exe record --foreground
```

host 使用 `CREATE_NO_WINDOW`，等待 child 并转发退出码；保留进程树生命周期，使结束任务或 host 崩溃后不会留下无人管理的 writer。
以挂起创建 → 纳入受控 Job → 恢复执行（或等效原子关联）消除 spawn/Job 绑定之间的逃逸窗口，并严格限制继承句柄。
继续通过原 recorder 停止协议实现协作停止，超时才由 manager/Job 终止。
日志接到受限文件，标准输入关闭；Codex/SSH 子进程的 console、stdin 和句柄继承也必须验证。
Task Scheduler 的运行状态与实际 recorder 心跳仍需同时检查。
参见 [进程创建标志](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags)及
[TaskSettings](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings)。

这仍是一份业务程序加系统适配 helper，但会改变现有发布/信任协议，不能仅替换 XML 的 Command：

- `src/service/upgrade.rs:765–798` 当前将任务 Command 当作业务 executable 并精确校验参数。
  新 importer 必须同时理解旧 direct 与新 hosted 定义；不放宽对任意 wrapper 的信任。
- 将 helper 身份、业务路径、完整参数、SID、工作目录及环境纳入 versioned launch contract、fingerprint 与 service journal。
  迁移期间保持 blocker/singleton，旧 disabled 状态保持 disabled。
- 当前版本 store/prune 假设只有 binary 与 build.json（`src/update.rs:1302–1309`），manifest/bootstrap 也严格约束单个 Windows exe。
  helper 必须进入校验和、发布协议、引用跟踪及清理策略，提供显式兼容迁移；不能随意在旧 manifest 中加字段。
  具体过渡可保留现有 schema 1 主程序资产，另发与同一 build ID 绑定的 Windows 组件清单，由新 candidate 获取和校验 helper；
  或先发布能够读取新版 bundle 的过渡 updater。任选一条并验收旧版本升级链，不能直接切换格式使旧 bootstrap 无法下载新 updater。

用户生命周期接口建议补充 `service start/stop/restart/repair`；start/stop 应明确是本次运行状态还是禁用登录自启。
status 输出 backend、SID、logon requirement、enabled、manager state、actual build、heartbeat、last exit/HRESULT、日志位置。
`installed`、`running`、`healthy`、`waiting_for_logon` 分开表达。
没有交互会话时允许明确的“只注册、等待登录”安装，但不能将其当成更新成功且 recorder ready。
已有在线升级请求遇到此限制，默认在变更前退出；显式 deferred 模式另记待激活状态。
现有结果校验只接受 `complete/partial/failed`，且 partial 也要求 recorder 已 ready/disabled/not_installed
（`src/update.rs:484–508`）。因此 deferred/pending 需要能力协商与新结果协议；旧中心必须明确拒绝不理解的状态。
未就绪时不能把 CLI/中心 agent 激活描述为完整节点更新成功；登录后的续作重新校验 SID、任务定义、锁与心跳。

## 远端 Windows 与无人登录模式

保留 SSH stdio exporter：它只需要当前 SSH 会话，不要求先安装后台服务。
在平台探测中增加 SID、用户根目录、安装来源、任务 backend、是否具备运行所需会话/权限等能力结果。
路径编码由已知平台和 shell adapter 决定；平台未知时保留安全 bootstrap 探测，不仅靠 exe 路径形状猜测。
修复相对反斜杠路径后，对所有允许的 Windows 路径/参数统一采用 literal 编码和结构化调用。
回归同时覆盖 `tools\agent.exe`、`..\agent.exe` 和 Unix 绝对路径中合法的反斜杠，不能只见 `\` 就判断为 Windows。

`remote deploy --scope sync/node` 仍然只定义更新哪些组件，不能借此隐式改成机器安装、启用 recorder、改登录身份或写用户 PATH。
需要首次远端安装注册时，应由显式安装选项表达，然后调用同一个目标机 installer。
保留源 pin、中心配置 revision 校验和部署后真实同步；SSH 中断时先查询 journal 与实际组件身份，再决定续作。

| 模式 | 使用场景 | 条件与支持边界 |
| --- | --- | --- |
| SSH stdio exporter | 按需读取远端已有数据 | 当前 SSH 用户执行即可；不等同于持续采集。 |
| 用户登录任务（默认） | 个人 Windows 开发机 | 无管理员、无密码；要求相同 SID 保持交互登录。锁屏/RDP 断开与真正注销需分别验收。 |
| SCM 服务（后续显式模式） | 无人登录的服务器持续采集 | 管理员配置、指定服务账户、服务登录权利、正确用户数据/凭据上下文；独立维护生命周期。 |

SCM 模式必须实现 ServiceMain/控制分发/停止与状态上报，或采用受控、版本固定的成熟 service wrapper，
不能只对当前 CLI 执行 `sc create`。参见
[StartServiceCtrlDispatcher](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicectrldispatchera)。
不默认使用 LocalSystem；该身份的 HKCU、用户目录与权限不同于登录用户。
[Microsoft LocalSystem](https://learn.microsoft.com/en-us/windows/win32/services/localsystem-account)。

机器模式的代码放管理员可写的 `%ProgramFiles%`，运行状态按服务账户设 ACL；
不能让高权限服务执行普通用户可改的 LocalAppData binary/launcher。
其安装归属、元数据和 ACL 校验需要独立的机器安装策略，不能只修改当前用户 private store 的根目录。
用户 Codex home/auth、DPAPI、SSH key/agent 和远端配置要在所选账户下逐项验证；不自动复制凭据、不自动迁移历史。
机器更新需要相应授权的执行身份，普通用户 `node` 更新不得越权修改机器服务。
如果未来需要凭据型计划任务，可另设 backend；其密码管理、batch logon、Hello-only 账户与组织策略问题不纳入默认安装路径。

## 实施顺序与验收

| 批次 | 可交付内容 | 关键验收 |
| --- | --- | --- |
| 1：修复已有更新路径 | 运行中旧 exe 的迁移预检/handoff；相对 Windows SSH 路径编码；PATHEXT 遮蔽识别。 | 两个不同构建从旧入口自身启动迁移；原有任务不被提前破坏；cmd/PS 的相对/绝对/中文/空格路径调用一致；旧 wrapper 遮蔽可见。 |
| 2：完成用户安装 | 薄 install.ps1、Rust install/repair/uninstall、独立 receipt、HKCU PATH；已有更新器复用。 | 普通用户零提权安装，PS5.1/7 新终端可用；长 PATH/变量引用不丢失；安装重复执行幂等；卸载保留用户数据和其他软件条目。 |
| 3：完成桌面后台体验 | versioned recorder host、发布/信任/导入迁移、服务状态与操作、SSH 无登录预检。 | 无控制台弹窗；登录启动/注销停止；崩溃重启；disabled 保持；新旧注册迁移后每个 history 只有一个 writer；真实心跳验证。 |
| 4：服务器与分发扩展 | 显式 SCM 模式、签名 User Setup/卸载项、需要时增加原生 ARM64 资产。 | 无登录重启后持续运行，服务账户数据/凭据正确，权限边界正确；签名先于最终资产 hash/manifest 生成。 |

先以 x64 为可交付平台；现有 Windows release target 只有 `x86_64-pc-windows-msvc`。
ARM64 上的 x64 仿真测试不得宣称原生 ARM64 发布支持。签名可改善 Windows 分发身份验证，但同源 SHA256 清单本身不等于发布者数字签名。

每批先跑受影响回归，稳定后按 [测试流程](testing.md) 做相关完整套件，再进行一次有明确目的的 hosted checkpoint。
Windows 需要增加隔离的普通用户真实安装/Task Scheduler 验收，不能仅重复 SYSTEM fixture：

- 同时运行 TUI、CLI、recorder 时升级；测试 Ctrl+C、exit code、终端关闭和占用退出后的恢复。
- 在下载、停止、注册、心跳、CLI 指针、PATH 写入各阶段注入失败，验证 journal 续作及 writer 兼容边界。
- 停用任务、修改过的注册、多个更新器竞争、杀毒暂时占用文件，以及已有包管理器/同名 wrapper。
- 中文/空格/单引号路径，超过 1024 字符的 PATH，变量引用、重开终端与旧终端。
- 普通用户桌面登录、锁屏、RDP 断开、注销、重启未登录和纯 SSH；系统策略拒绝注册时错误可诊断。
- ConPTY 覆盖真实 launcher → candidate 的交互链；如增加 TUI 控件，遵守快捷键着色、点击命中区和紧凑布局要求。

## 实施前审查验证记录

审查阶段仅编写方案，未修改产品代码，也未注册实际 PATH/任务/服务、部署远端或触发 hosted CI。
测试前 checkout 干净，基线 SHA 如文首；测试后只新增本文，相关测试输入未修改。
原生环境：Windows `10.0.26200` x64，沙箱普通身份 `CodexSandboxOffline`，Python 3.12.4，外层 PowerShell 7.4.1。

实际执行：

```text
python -B -m unittest discover -s tests -p test_remote_release_bootstrap.py -v
python -B -m unittest discover -s tests -p test_release_package.py -v
```

结果：13 项 bootstrap 测试通过（含 Windows PowerShell 5.1/7 的 24 个 shell/case 组合）；6 项 packaging 测试通过。
输入文件 SHA256、完整命令、身份和结果位于本地 ignored 证据目录：
`D:\Workspace\codex-usage-monit\.codex-usage-monit\windows-review-20260921\`，
文件为 `context.json`、`result.json`、`bootstrap.log`、`packaging.log`。
这是原生 shell/离线 fixture 证据，不是生产 Release 下载、原生服务安装或真实 SSH 验收。
运行中 exe 替换与路径解析另有隔离微型复现；它们用于确认机制，不计作完整应用测试。
其源码/结果汇总在同一证据目录下的 `exe-replacement/`、`path-resolution/` 和 `remote-shell/`；
`micro-reproductions.md` 记录执行命令与输入身份。
未运行 Rust/ConPTY 全套、UTM/Docker 新快照、真实服务/SSH 集成与发布签名检查。
