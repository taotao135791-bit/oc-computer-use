# Pointer Isolation + Visual Cursor + Human Interrupt

> Round 8 目标:Agent 拥有**独立 Virtual Pointer**,默认不移动用户系统 Cursor;
> 屏幕上有不抢焦点、不进入截图的可视化 Agent Cursor;用户一旦操作立即中断 Agent。

## 架构

```text
Model
  ↓ Screenshot (不包含 Ghost Cursor)
  ↓ Vision reasoning → x, y
Computer Use Runtime
  ↓ Virtual Pointer (per-session)
  ↓ 执行层 (best available actuator)
```

执行路径(以真实实验顺序为准,不做先验假设):

```text
Virtual Pointer
  ├─ DirectCGEvent (不 warp 系统光标,轮询位置点击)
  ├─ Accessibility AXPress (坐标 → hit-test → press,仅执行不做 grounding)
  └─ Physical Fallback (明确状态 + 保存/恢复用户 Cursor + Human 可打断)
```

## 已实现(代码)

| 能力 | 实现位置 | 状态 |
|---|---|---|
| `VirtualPointerState` / `PointerMode` / `PointerPolicy` | `cu-core/src/pointer.rs` | ✅ 编译 + 单测 |
| Session 挂载 Virtual Pointer / Target / FocusPolicy | `cu-runtime/src/sessions.rs` | ✅ 编译 + 单测 |
| Ghost Cursor Overlay (NSPanel, ignoresMouse, non-activating, floating) | `swift/.../CUBridge/main.swift` | ✅ Swift 编译通过 |
| SCK 截图像排除 overlay (window id) | `swift/.../main.swift` `captureDisplay` | ✅ 编译通过,需真机验证 |
| DirectPositionEvent click(不下 warp) | `cu-driver-macos/src/mouse.rs` `click_direct` | ✅ 编译通过,需真机验证 |
| AX coordinate actuator | `cu-driver-macos/src/accessibility.rs` | ✅ 编译通过,需真机验证 |
| HumanInputMonitor (continuous, Human Always Wins) | `cu-runtime/src/human_input.rs` | ✅ 编译 + 单测 |
| macOS Event Tap (synthetic-PID 过滤) | `cu-driver-macos/src/event_tap.rs` | ✅ 编译通过,需真机验证 |
| 新错误码 (TARGET_OUTSIDE_SESSION 等 6 个) | `cu-core/src/errors.rs` | ✅ 编译 + 单测 |
| Protocol schema / TS 绑定(生成) | `pnpm generate:protocol` | ✅ 已生成 |

## 真实测试方法(必须在本机执行)

### Direct CGEvent 点击实验
```bash
# 构建驱动 + bridge
cargo build -p cu-driver-macos
cargo test -p cu-driver-macos --lib ffi
# 浏览器 Target Board + Native Target Board 见 benchmarks/target-boards/
```

对每个实验记录:

```text
Direct CGEvent click:
Target hit: PASS/FAIL
Visible cursor moved: YES/NO
Focus changed: YES/NO
SwiftUI Button 响应 / AppKit Button 响应 / Electron 响应
```

### Ghost Cursor 截图排除验证
1. `overlay_show(x,y)` 显示 AI Cursor。
2. `computer.observe` 截图。
3. 肉眼确认 AI Cursor 不在截图中(SCK excludingWindows)。

### Human Conflict Test(手动)
1. Agent 连续点击 Target Board。
2. 测试者随机移动真实鼠标。
3. 期望:Agent 立即停 → 不拉回 Cursor → Queue 停 → `USER_TAKEOVER`。

记录 `human_input_detected` + P0-4 指标:`event_detection_latency_ms`(事件→callback)、`human_to_takeover_ms`(事件→takeover)、`human_to_input_stop_ms`(事件→最后一次 synthetic 输入,THE KPI;0 = agent 已先停)。

## 诚实状态

以下能力在**写盘时仍未在真实 macOS 上实测**,标记 **NOT VERIFIED**:

- Direct CGEvent 不移动系统光标是否真正命中各 App(浏览器/SwiftUI/AppKit/Electron)
- AXPress 在 Native App 的命中率与 AX_UNSUPPORTED 场景
- Ghost Cursor 是否被 SCK 排除(需 Screen Recording 权限)
- Event Tap 的真实 Human Interrupt 延迟(p50/p95/max)
- Physical Fallback 的保存/恢复/human 打断

所有实测结果应回填到本文件与 `docs/acceptance-manual.md`。