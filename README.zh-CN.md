# computer-use（中文说明）

[![CI](https://github.com/taotao135791-bit/oc-computer-use/actions/workflows/ci.yml/badge.svg)](https://github.com/taotao135791-bit/oc-computer-use/actions/workflows/ci.yml)

一个**模型无关、以视觉优先的 macOS 电脑操控（Computer Use）运行时**（macOS 14+）。

外部智能体（Claude Code、Pi、OpenCode、Codex CLI，或任意模型）通过一个精简的
JSON-RPC 接口来操控桌面：截取屏幕、基于截图执行点击/按键/输入，同时被会话锁、
过期帧保护、轨迹脱敏等机制牢牢约束。运行时**不依赖任何模型**——它从不调用 LLM。
整个闭环永远是：*智能体观察 → 智能体决策 → 运行时执行*。

> **核心设计：它有一个属于自己的“虚拟鼠标”，而不是操控你的真实鼠标。** 智能体
> 的每一次点击、移动都在一个独立于系统光标的虚拟指针上完成；你的真实鼠标在任何
> 时刻都不属于智能体——你一动，智能体立即停手。

```
+----------------+   JSON-RPC 2.0    +-------------------+   line-JSON   +---------+
| 智能体 (Pi,    | <===============> | cu-daemon         | <===========> | cubridge|
| OpenCode, ...) |   ~/.computer-use | 会话、锁、过期帧、 |  Unix pipe   | Swift:  |
| 经 SDK / MCP   |   /runtime.sock   | 轨迹             |              | SCK     |
+----------------+                   +-------------------+              | 截图    |
                                     | cu-runtime · cu-driver-macos      +---------+
```

## 目录结构

| 组件 | 位置 | 职责 |
|---|---|---|
| `cu` CLI | [crates/cu-cli](crates/cu-cli) | daemon 生命周期、会话、observe/act、轨迹 |
| Daemon | [crates/cu-daemon](crates/cu-daemon) | 基于 Unix socket 的 JSON-RPC 2.0（仅当前用户可连） |
| Runtime | [crates/cu-runtime](crates/cu-runtime) | 会话、控制锁、动作队列、稳定器、暂停/恢复/接管/停止 |
| macOS 驱动 | [crates/cu-driver-macos](crates/cu-driver-macos) | 截图、鼠标、键盘、显示器、剪贴板、权限 |
| Swift 桥 | [crates/cu-driver-macos/swift](crates/cu-driver-macos/swift) | ScreenCaptureKit + 剪贴板 + AX（项目中唯一的 Swift） |
| 轨迹记录器 | [crates/cu-trace](crates/cu-trace) | 带脱敏的会话 JSONL 轨迹 |
| TypeScript SDK | [packages/sdk-typescript](packages/sdk-typescript) | 面向 Node 智能体的 `ComputerUseClient` |
| MCP Server | [packages/mcp-server](packages/mcp-server) | 7 个工具（observe/act/inspect/session/cancel/trace），以图片内容块返回 |
| Pi 扩展 | [packages/pi-extension](packages/pi-extension) | 4 个带真实图片内容块的工具 + 8 个斜杠命令，支持中止与生命周期 |
| OpenCode 适配器 | [packages/opencode-adapter](packages/opencode-adapter) | 配套 CLI（`cu-opencode`）+ OpenCode 官方 MCP 配置 |
| Inspector | [apps/cu-inspector](apps/cu-inspector) | 极简本地面板（http://127.0.0.1:8420） |

## 快速开始

```bash
# 1. 构建
cargo build --release

# 2. 一次性授权（见 docs/permissions.md）：
#    系统设置 → 隐私与安全性 → 屏幕录制 → 添加 cubridge

# 3. 启动 daemon
cu daemon start

# 4. 开始使用
cu doctor
cu observe --include-image --image-out /tmp/screen.jpg   # 首次 observe 自动创建会话
cu move 500 400
cu click 500 400
cu type "hello"            # 输入文本在轨迹中默认脱敏
cu session stop            # 只有持有会话控制令牌的客户端才能停止会话
```

**会话在首次使用时自动创建。** 任何客户端的首次 `observe`/`act` 会在无活跃会话时
自动启动一个会话（CLI 先解析当前活跃会话，仅在 daemon 返回 `SESSION_NOT_FOUND` 时才
启动）。daemon 会记录**是谁**启动了它——每个客户端在 `session start` 时都会发送身份
（`client_id` / `client_name` / `client_instance_id`），`session status` 返回所有者。
访问控制是**基于能力（capability）而非身份**的：只有持有会话**控制令牌**的客户端才能
停止或接管会话；变更类操作需要控制令牌，敏感读取需要观察令牌（第二个客户端在默认
策略下没有令牌时得到 `CONTROL_LOCKED`）。

输入类动作**默认脱敏**：轨迹记录 `text_redacted: true` 和字符数，绝不记录文本本身。
如需记录完整输入（例如你信任的开发环境），请以开发模式启动 daemon——见
[轨迹脱敏](#轨迹脱敏)。

## 虚拟鼠标与指针隔离（核心特性）

这是本项目区别于“直接操控系统鼠标”类工具的关键，目标是达到与 Codex Computer Use
同级的生产体验：

- **智能体拥有独立的虚拟指针**：`VirtualPointerState` 是“智能体想指向哪里”的唯一
  事实来源，永远不与系统光标混淆。默认**不移动你的系统光标**。
- **屏幕上有一个可视化 AI 光标（Ghost Cursor）**：一个不抢焦点、不响应鼠标、悬浮的
  `NSPanel` 叠加层，实时显示智能体当前指向与点击位置；点击时还有涟漪动画作为视觉
  确认。它**不会进入截图**（通过 SCK `excludingWindows` 排除），模型不会把自己的
  光标误当成页面元素。
- **三种执行后端**，按顺序择优、永不擅自降级：
  1. **Direct CGEvent**（`click_direct`）——在目标坐标直接下发按下/抬起，**不先
     warp 系统光标**；
  2. **Accessibility `AXPress`**——坐标 → hit-test → 按下，仅做动作、不做 UI 定位；
  3. **物理回退**——仅在显式允许时短暂借用真实光标，**保存/恢复**用户光标，且
     “人随时可打断”。
- **Human Always Wins（人永远优先）**：一个真实硬件事件（Event Tap）会在事件发生的
  **那一刻**取消进行中的动作、把会话翻转为 `user_takeover`，并立即隐藏 AI 光标；
  恢复（`resume`）无法绕过它，智能体必须先 `release`。

相关文档：[docs/pointer-isolation.md](docs/pointer-isolation.md)。

## 四个工具（任意智能体通用）

| 工具 | 用途 |
|---|---|
| `computer_observe` | 截屏 → frame_id + 图片 + 元数据 |
| `computer_act` | 在某个帧上执行动作（click、move、type、key、scroll、drag、wait） |
| `computer_inspect` | 裁剪已存帧的某个区域（视觉细节，无 DOM/XPath/OCR） |
| `computer_session` | Start / status / pause / resume / takeover / release / stop |

外加轨迹查看（`trace_list`、`trace_get`、`trace_export`、`trace_replay`）与运行时
自省（`health`、`permissions`、`displays`、`pointer`、`active-application`）。

运行时强制的一切——帧过期、坐标越界、暂停、接管、会话状态、控制锁——都**在服务端
强制执行**，而非客户端，因此每个适配器都得到相同的保证。

## 安全模型

- **Socket**：`~/.computer-use/runtime.sock` 上的 Unix 域套接字，权限 `0700`——只有你
  自己的用户能连接。
- **会话**：同一时刻只有一个活跃会话（控制锁）。自动创建是*适配器*的便利（SDK/CLI/
  MCP/Pi 先解析 `status`，仅在 `SESSION_NOT_FOUND` 时启动）——裸的
  `computer.observe` / `computer.act` 方法从不创建会话。创建者被记录为会话**所有者**
  （用于诊断）；访问基于能力——只有持有控制令牌的客户端能停止/接管，无有效令牌的
  客户端被拒以 `CONTROL_LOCKED`。每次 observe/act 都携带 `session_id`。对过期、暂停、
  被接管或已停止会话的动作会以具体错误码拒绝。
- **能力令牌**：`session start` 返回会话的两个令牌**且仅此一次**（各为 256 位
  CSPRNG）：`observation_token`（敏感读取）与 `control_token`（变更操作，同时开放
  读取）。**仅知道会话 ID 不授予任何观察或控制权限**——daemon 校验所提交令牌的
  SHA-256 哈希，且在 `start` 之后永不复述。`status` 不会重新签发，`stop` 或 daemon
  重启使其失效。CLI 以 `0600` 权限持久化会话凭证；SDK 仅保存在内存中。
- **既有会话默认 `reject`**：发现不属于自己的会话的客户端不得静默附加。SDK 的
  `ensureSession` 只提供显式、需携带令牌的选项：`read_only` 需要外部会话的
  **observation token**，`attach_with_token` 需要其 **control token**——仅会话 ID 不
  授予任何东西。
- **Daemon 管理令牌**：`runtime.shutdown` 需要每次安装的 admin 令牌（256 位 CSPRNG，
  启动时以 `0600` 持久化）——只有 daemon 管理者（CLI / LaunchAgent）持有它；凭证存储
  损坏时拒绝启动，而非留下一个停不掉的 daemon。

### 能力矩阵

| 操作 | 仅会话 ID | 观察令牌 | 控制令牌 | 管理令牌 |
|---|---|---|---|---|
| `status` | `OBSERVATION_TOKEN_REQUIRED` | ✅ | ✅ | — |
| `observe` / `inspect` | `OBSERVATION_TOKEN_REQUIRED` | ✅ | ✅ | — |
| `trace.list` / `trace.summaries` / `trace.get` / `trace.export` / `trace.replay` | `OBSERVATION_TOKEN_REQUIRED`（会话作用域） | ✅ * | ✅ * | — |
| `trace.admin_list`（跨会话列表） | `DAEMON_ADMIN_TOKEN_REQUIRED` | ❌ | ❌ | ✅ |
| `act` / `cancel` / `pause` / `resume` / `takeover` / `release` / `stop` | `CONTROL_TOKEN_REQUIRED` | ❌ | ✅ | — |
| `runtime.shutdown` | `DAEMON_ADMIN_TOKEN_REQUIRED` | ❌ | ❌ | ✅ |

\* 该会话**自身**的观察/控制令牌，且仅针对所寻址的那个会话——来自会话 A 的令牌永远
无法读取会话 B 的轨迹。控制令牌包含观察权限（它同样能通过读取校验）；观察令牌永远
不授予变更。令牌错误刻意不具描述性（`INVALID_*` 不提示哪个令牌错了）。

- **过期帧**：默认 `strict` 策略下，对非当前帧的动作被拒（`STALE_FRAME`）；
  `visual_match` 策略（环境变量 `COMPUTER_USE_STALE_POLICY`）额外允许内容仍与实时
  屏幕一致的旧帧。实时视觉比对 + 应用切换 + 年龄兜底始终在上层运行。
- **越界**：显示器之外的动作被拒（`OUT_OF_BOUNDS`）。
- **脱敏**：`type` 在轨迹中记录 `{ text_redacted: true, character_count }`；完整文本
  仅在显式 opt-in 时记录。剪贴板内容永不记录，任何能力令牌都不会出现在轨迹中。
- **接管**：人随时可以抓走鼠标；会话翻转为 `user_takeover` 且运行时拒绝后续动作，
  `resume` 无法绕过——智能体必须先 `release`（`USER_TAKEOVER_ACTIVE`）。
- 完整错误码表见 [docs/protocol.md](docs/protocol.md)，权限坑点见
  [docs/permissions.md](docs/permissions.md)（含“重编 cubridge → 重新授权屏幕录制”）。

## 轨迹脱敏

默认开启。`cu daemon start` 以脱敏运行。如需在轨迹中记录完整输入（仅开发环境）：

```bash
COMPUTER_USE_TRACE_DEV_MODE=1 cu daemon start
```

每条轨迹保留 `redaction: { text_redacted, character_count }`，方便在不泄露机密的情况
下审计发生过什么。

轨迹记录策略（`COMPUTER_USE_TRACE_MODE`）：`best_effort`（默认——轨迹写入失败仅降级，
`computer.act` 上报 `trace: {degraded: true, warnings}`）、`required`（无法记录轨迹时
会话启动/act 失败）、`disabled`（不记录）。

## 目录布局

```
~/.computer-use/
├── runtime.sock        # JSON-RPC socket (0700)
├── bin/cubridge        # 编译后的 Swift 桥
├── frames/             # 捕获的帧（按会话，命名 s_<id>_<n>.jpg）
├── traces/             # s_<id>.jsonl 会话轨迹
└── daemon.log
```

## 测试

```bash
cargo test --workspace                    # Rust：core、driver、runtime、daemon 协议、所有权矩阵、轨迹分析
cargo test -p cu-daemon --test integration -- --ignored   # 真实安全矩阵测试
pnpm install && pnpm -r build && pnpm -r test             # SDK / Pi / OpenCode / MCP 套件
pnpm run ci                               # TypeScript 全部门禁：check:protocol → build → typecheck → lint → test
pnpm scan:secrets                         # 全仓库 gitleaks 扫描
./scripts/smoke.sh                        # 严格冒烟：退出码 0=全绿，1=某项失败，2=用法错误
```

**测试不会劫持你的鼠标。** 所有鼠标/滚轮/拖拽测试都注入一个“记录型 poster”来捕获
事件序列，而不是向 CoreGraphics 实际投递事件；真实环境的验收脚本（需要登录 GUI
会话）单独列出：

```bash
node scripts/pi-host-acceptance.mjs       # Pi 扩展，真实代码 + 真实 daemon/屏幕
node scripts/opencode-mcp-acceptance.mjs  # 真实 computer-use-mcp 二进制 over stdio
node scripts/ownership-scenario-a.mjs     # 所有权：MCP 会话 vs Pi 扩展
```

真机验收清单见 [docs/acceptance-manual.md](docs/acceptance-manual.md) 与
[docs/round7-acceptance-report.md](docs/round7-acceptance-report.md)。

## 文档

- [docs/architecture.md](docs/architecture.md) — 组件、线程、数据流
- [docs/protocol.md](docs/protocol.md) — JSON-RPC 接口、方法、错误码、会话行为
- [docs/permissions.md](docs/permissions.md) — 屏幕录制 / 辅助功能设置与排查
- [docs/pointer-isolation.md](docs/pointer-isolation.md) — 虚拟鼠标、指针隔离与人工中断
- [docs/acceptance-manual.md](docs/acceptance-manual.md) — Pi + OpenCode + 所有权验收清单
- [SECURITY.md](SECURITY.md) — 威胁模型、机密处理、凭证文件写入安全
- [docs/uninstall.md](docs/uninstall.md) — 干净卸载
- [README.md](README.md) — 英文版说明

## License

MIT（见 [LICENSE](LICENSE)）。
