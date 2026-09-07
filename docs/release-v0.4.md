# 0.4.0 发布准备与复核记录

审查起点：`79023e0`。候选版本：`0.4.0`。
本轮发现的问题已修复，本地三平台候选验证已完成；尚待集中 GitHub CI，
未推送发布标签或发布 GitHub Release。

## 本轮发现与处置

| 问题 | 处置与证据 |
| --- | --- |
| 空 facts delta 不刷新 digest 绑定，旧清单无法恢复精确副本去重 | 同 cursor 只有在绑定也相同时才 NoOp；新增回归先失败后通过。`5b229b9` |
| 已排除聚合的 source 继续同步时误报 facts 错误 | 返回 NoWork，保留 source-only 同步且不扫描其他来源。`5b229b9` |
| Unix SSH 主进程换进程组后可能漏回收 | 清理原进程组后仍检查、终止精确主进程。`bf00841` |
| 显式禁用可选 rollout cache 后 probe 错报不可写 | 仅检查已配置的 cache；真实 Delta 可工作，probe 与之对齐。`55313f9` |
| aggregate 成功掩盖 facts 失败/持久暂停，重新配对又可能显示旧健康 | Other 独立显示 facts/暂停及恢复办法，按完整 node/generation 匹配健康。`472e849` |
| recorder 的预算暂停掩盖独立 SSH 清理失败 | 优先保留匹配当前主机的进程暂停诊断；普通预算暂停仍不算故障。`81ca5bf` |
| 无 systemd 的精简 Linux 首次同步无法迁移历史 | 只有 manager 和旧 unit 的证据均不存在时允许迁移；旧 unit、链接、显式 bus 或不确定状态仍阻断。原无 systemctl 容器实际复验通过。`a0f45e6` |
| 快速上手遗漏脱敏策略、后台入口、custom state 和升级要求 | 保留默认脱敏的报错恢复路径，补齐中英文 README 与 [用户指南](remote-usage.md)。`4c99c3b` |

审查覆盖 allowlist/配对、SSH 参数和进程回收、单帧协议、版本与来源绑定、
aggregate WAL/COW/cursor、facts 分页/绑定、副本去重、来源选择、自动调度、
带宽与健康展示。

## 验证

- macOS 真实 OpenSSH 回环：19 项检查通过，实际完成 local/remote facts
  activation，复制与追加后总量可对账，无变化 aggregate 为 1,394 B。
  证据：`/private/tmp/codex-ssh-loopback-v4qd__i3/`。
- Linux Docker 真实 OpenSSH 回环：19 项检查通过，无需 systemctl；额外验证
  现存 recorder unit 会阻断首次迁移。无变化 aggregate 为 1,411 B。
  证据：`/Volumes/File/codex-usage-monit-docker-build/runs/20260907T173828Z-arm64-ssh-loopback/`。
- Windows ARM64 中心 → macOS exporter 真实 SSH：双端均为 0.4.0，6 项检查
  通过，服务端记录 4 次来自 UTM 的公钥登录。覆盖配对后仍双重默认关闭、
  readiness、完整同步、All/精确 source 对账，以及重复同步幂等。
  证据：`/private/tmp/codex-windows-ssh-n444ioky/`；临时 SSH 服务已停止，
  私钥删除和 guest 清理均由匹配 nonce 的结果文件确认。
- Windows exporter 另验证了真实原生 agent 经 cmd/PowerShell 返回有效单帧，
  并拒绝错误 generation；未将此项表述为 Windows sshd 登录验证。
- 最终 macOS ARM64 完整管线：Rust 1,758 通过、0 失败、1 项手动基准忽略；
  Python 29 项、fmt、Clippy、PTY、预览、安装器和 CLI 冒烟均通过。
  命令：`CARGO_TARGET_DIR=/private/tmp/codex-review-target CARGO_BUILD_BUILD_DIR=/private/tmp/codex-review-target sh scripts/verify-unix.sh`。
  证据：`/private/tmp/codex-v0.4-macos-result.json` 和
  `/private/tmp/codex-v0.4-macos-verify.log`。
- 最终 Docker Linux ARM64 完整管线：Rust 1,755 通过、0 失败、1 项手动基准忽略；
  Python 29 项、fmt、Clippy、PTY、预览、安装器和 CLI 冒烟均通过。
  命令：`sh scripts/test-linux-docker.sh`。
  证据：`/Volumes/File/codex-usage-monit-docker-build/runs/20260907T174438Z-arm64-11055/`。
- 最终 UTM Windows ARM64 完整管线：Rust 1,666 通过、0 失败、1 项手动基准
  忽略；fmt、Clippy、ConPTY、构建和 CLI 冒烟均通过。
  命令：`python3 scripts/macos/test-windows-utm.py --toolchain-home 'C:\Users\user' --output-dir /private/tmp/codex-v0.4-windows`。
  证据：`/private/tmp/codex-v0.4-windows/7ea39f0ab9834057b5ecc6c7aae2966f/`。
- `cargo audit --deny warnings` 使用最新拉取的 RustSec 数据库检查全部
  243 个依赖，退出 0，无漏洞或告警；数据库提交为
  `faedffd5118c1835e13cca3babb6059afb1eb8d0`。
  证据：`/private/tmp/codex-v0.4-audit.log`。工作流 actionlint 和发布文档
  本地链接检查也通过。

完整 macOS/Linux 管线使用 Rust 1.97.0、二进制版本 0.4.0，源代码是
`16ed0e83f62d246806d4850a9eb238a04dd75b08` 加当时的未提交快照；
两者逐文件内容与权限一致，快照 SHA-256 为
`3c0923acfc819a49c416eb3af5463660774ce152637b1017971df85b963f6ee4`。
其中产品代码的唯一未提交变化是后由 `8428cb4` 保存的 rustfmt 调整，其余是
发布文档。macOS 测试期间保持 checkout 稳定，结束后与 Docker 快照核对。
Windows 同样使用该提交加当时的未提交快照，Rust 1.97.0，目标为
`aarch64-pc-windows-msvc`；ZIP SHA-256 为
`d7b9d2628a5897e14301eea549077da56c9c71528c79589211a1ba75f2d2e01a`。
最终产品、测试、脚本、清单和工作流的 142 个文件与该 ZIP 逐字节相同。

回环脚本保存二进制版本、SHA-256、逐命令结果及最终检查，使用合成数据、临时
密钥和配置，结束后关闭临时 SSH 服务并删除私钥。不存在的 fixture 项目目录曾
触发预期的项目证据拒绝；改用真实临时项目后事实同步通过，未放松产品校验。
上述 macOS/Linux 真实 SSH 验证在本轮相关功能修复后、版本号更新前完成，
二进制仍显示开发期间沿用的 0.3.1；它们不是已发布的 v0.3.1 程序。
版本更新后的 0.4.0 完整管线结果独立记录，不混用旧版本的测试计数。

## 使用和验证范围

- 只连接显式配置的来源；自动同步需要两级 opt-in 和运行中的 recorder。
- 首次迁移是单向切换，升级前备份 state、停止旧 collector 并更新 recorder
  注册。默认远端脱敏时，中心入口统一使用 `--redact-content`。
- 不能证明项目/事件归属时保持 partial/attention；移动或删除的项目目录可能
  妨碍精确 facts 对账。多账号、自动部署远端程序和多中心共享不是本版承诺。
- 本轮真实 SSH 使用合成数据；没有把真实用户超大历史容量/性能基准视为已通过。
  协议、分页、缓存和资源上限另有自动回归。Windows SYSTEM 测试不代表普通
  用户登录服务的所有行为，原生 ARM64 也不代替 x64 发布目标验证。

## 发布步骤

1. 确认完整本地平台验证通过，候选代码和文档已提交。
2. 推送候选提交后按 [集中 CI 指引](testing.md) 验证精确 SHA；普通 push
   不应替代显式的测试请求。工作流需已存在于默认分支。
3. 核对 Linux、macOS、Windows 和依赖审计结果。只有成功后才创建并推送
   `v0.4.0`。版本标签会再次触发发布验证，所有打包任务通过后才发布 Release。

本地已准备版本和发布说明；发布标签与 GitHub Release 仍属于后续发布动作。
