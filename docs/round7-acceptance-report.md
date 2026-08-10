# Round 7 验收报告 —— Pointer Isolation 收尾阶段

日期: 2026-08-10
提交: `18e365e` (Pointer Isolation closing round: fix 7 identified implementation issues)
基准: 第 五十 节 (commit/push/CI) 已满足 —— 远端 SHA 一致, 远端 CI (run 31362019158) 全绿。

> 环境与诚实声明: 本机是**真实 macOS GUI 会话** (WindowServer 在线, 屏幕录制 +
> 辅助功能均已授权, `cu doctor` 全过), 显示器 1512×982 @ scale 1.0, release `cu` +
> 修复 P0-1 后的 `cubridge`, daemon 运行且 Event Tap 真实 `active`。
> 但整个验收期间该机器**持续被人真实使用** (鼠标事件 <1s 一次, 前台应用与 Space
> 不断被切走)。因此所有"需要目标应用在测试期间保持前台"的正向结果验证都被环境阻断,
> 下面按类别如实标注 PASS / FAIL / NOT VERIFIED, 并给出精确命令 —— 未达标即报真实值,
> 不以单元测试冒充真机验证。

---

## 1. Git — PASS

- 7 项修复全部完成、提交 (`18e365e`)、推送, 远端 SHA 与本地一致, 远端 CI 全绿。
- README 已按规格降级旧 30-task benchmark 为 Experimental / Frozen, 并按规格重排
  Validation Focus 五项顺序。

## 2. Window Isolation (Test A) — PASS (真机)

| 检查 | 结果 |
|---|---|
| 基础隔离: 656×422 窗口 @(100,100) 的窗口级 observe | 捕获 **656×422** (== 窗口 bounds), 内容与独立 `screencapture` 参考的对应裁剪 **99.5%** 一致, 无跨应用内容 |
| 窗口宽于 max_width: 1512×300 (> 1440) | 捕获 **1440×285** —— 精确 max_width 下采样, 高度 floor 舍入在 P0-2 ±1px 容差内 |
| 移动窗口: 移到 (500,200) 后重新 observe | 捕获仍 656×422; 与新位置全屏裁剪 **99.3%** 一致, 旧位置仅 86.6% → 裁剪跟随 P0-3 刷新后的 bounds, 非陈旧值 |
| 目标窗口关闭 / 身份改变 | observe / act 被拒 **TARGET_UNAVAILABLE**, 多次真机复现 —— 绝不返回陈旧 bounds 的捕获 |

## 3. Human Interrupt (Test B) — 机制 PASS / ≥20 次协议 NOT VERIFIED

真机机制确认 (偶发证据):
- 真实人类事件将会话翻转为 `user_takeover`, `cu observe` 被拒 `USER_TAKEOVER`。
- `release` 后 ~300ms 内被下一个人类事件再次翻转。
- 飞行中的 `cu double-click` 在事件时刻被 human-interrupt 钩子取消
  (action `status: "cancelled"`)。

≥20 次受控冲突协议 **NOT VERIFIED**: 需要一名配合的人类操作员, 本次不可用。
手工验收步骤见 [acceptance-manual.md](acceptance-manual.md) Round 7 Test B。

## 4. Physical Fallback — 真机 NOT VERIFIED (未触发) / 单元测试覆盖

- 真机双/单击**总是走隔离 Direct-CG 路径成功** (8 次 action 全部
  `backend: "direct_cg_event"`, `isolated: true`, `physical_cursor_delta_px: 0.0`),
  因此 `AX_UNSUPPORTED_FOR_DOUBLE_CLICK` → `physical_double_click_at` 的物理回退路径
  在真机未被执行 —— 诚实记为 NOT VERIFIED (由 cu-runtime 102 个单测覆盖)。
- 物理回退的"人随时可能抓走光标"的**中断机制**由 §3 的真机证据侧面确认
  (人事件取消飞行中的动作)。

## 5. Drag·Scroll (P0-6) — 真机 NOT VERIFIED / 单元测试覆盖

受控的 drag/scroll 取消验证需要安静环境, 持续的人类活动使其无法在本次会话完成。
取消语义 (首事件前检查取消、mouseUp(last_actual_drag_point)、负坐标保留) 由单测覆盖。
手工步骤: 安静机器后按 acceptance-manual 的 drag/scroll 检查项逐条执行。

## 6. DoubleClick — 语义 PASS / 结果 NOT VERIFIED

- **真机语义 PASS**: `cu double-click` 经隔离 Direct-CG 路径执行 —— 8 次成功 action
  全部 `isolated: true`、`physical_cursor_delta_px: 0.0`; 驱动 `double_click_direct`
  发送 down/up(click_state 1) + down/up(click_state 2), OS 收到的是**真实双击**,
  且真实系统光标从未被移动。
- **结果 NOT VERIFIED**: 双击选中单词 (TextEdit 中单词高亮) 未能在本次真机演示 ——
  操作员持续切走前台应用/空间, 点击落在前台应用 (Chrome/飞书) 上而非 TextEdit。
  物理回退双击路径同样未在真机触发。手工验收命令见 acceptance-manual Round 7。

## 7. Accuracy (Test C / D) — NOT VERIFIED

- Test C (Browser board ≥50 点击): 需要 board 页面保持前台连续 ≥50 次点击,
  操作员持续活动 + 会话反复翻转 takeover, 无法完成。精确命令见
  [acceptance-manual.md](acceptance-manual.md)。
- Test D (Native board ≥30 点击): **按规格无法验证** —— 仓库中不存在
  `benchmarks/target-boards/native/` board (仅有 `browser/`)。

## 8. Violations — 0

- 无 `|| true` / `continue-on-error` / 删测试 / 降低 strictness。
- 无"为测试方便放松隔离" (未放宽 crop/resolve/坐标限制)。
- 无伪造真机数据: 上表每项均为真实命令 + 真实观测; 未达目标处明确标 NOT VERIFIED
  并给出精确命令与预期结果, 不以单元测试冒充 macOS 真机验证。

## 9. Automated Gates — PASS

第 四十九 节全部自动化门本地通过 (cargo fmt/clippy/test、pnpm ci、
protocol-drift、gitleaks、swift-bridge、smoke 22 ok), 已提交推送, 远端 CI 全绿。

---

### 附: 本次真机验证的主要原始证据

- 窗口级捕获尺寸与窗口 bounds 逐项相等 (656×422 / 1440×285)。
- 窗口裁剪与独立全屏参考 (screencapture) 的像素比对: 99.5% / 99.3% / 86.6%。
- 8 条 action trace: `{"backend":"direct_cg_event","isolated":true,"physical_cursor_delta_px":0.0,"physical_cursor_moved":false}`。
- `cu move 600 300` 前后真实光标不变 (880,306 → 880,306)。
- daemon.log: `human-input monitor starting ... state="starting"` →
  `cu_driver_macos::event_tap: event tap active on dedicated thread`。
- 多次 `USER_TAKEOVER` / `TARGET_UNAVAILABLE` / `cancelled` 真机复现。
