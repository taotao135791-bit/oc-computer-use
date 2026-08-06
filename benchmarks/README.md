# macOS Computer Use Benchmark

可重复的 macOS 桌面任务基准，用于度量 Computer Use Runtime（`cu`）在真实
macOS 桌面上通过真实宿主（OpenCode / Pi）完成任务的能力。**禁止作弊**：任务
结果是唯一由声明式 evaluator（文件系统 / defaults / HTTP fixture / 应用状态 /
人工复核）判定的；runner 不做规划、不写死坐标、不按任务 ID 执行固定动作。

## 结构

```
benchmarks/
  schema/task.schema.json     任务 YAML 的 JSON Schema（权威校验）
  schema/result.schema.json   单次运行结果的 JSON Schema
  tasks/                      30 个任务（每类一个子目录，每文件一个任务）
  runner/                     cu-bench CLI（Node, ESM）
    cu-bench.mjs              list / run / report / compare / replay
    lib/tasks.mjs             加载、校验、{{SCRATCH}}/{{FIXTURE_URL}} 占位符
    lib/evaluate.mjs          声明式 evaluator（10 种 criterion）
    lib/model.mjs             opencode run 子进程 + computer-use-mcp 检查
    lib/scratch.mjs           隔离的临时工作目录与 seed 脚本
    lib/trace.mjs             trace 读取（observation token）与失败分类
    lib/report.mjs            summary / failures / metrics / comparison
  fixtures/webapp/            本地 fixture 网页（Safari 任务用）
  scripts/                    initial_state / cleanup 脚本
```

## 任务格式

每个任务是一个 YAML 文件，字段见 `schema/task.schema.json`。要点：

- `success_criteria` 是唯一的成功判定来源；每个 criterion 的类型必须是
  `file_exists | file_contains | file_not_contains | file_absent | dir_exists
  | dir_contains | defaults_matches | http_check | app_running | human_review`。
- 占位符 `{{SCRATCH}}`（本次运行隔离目录）与 `{{FIXTURE_URL}}`（fixture 服务）
  在加载时替换；任务引用了未在 `environment` 声明的占位符会加载失败。
- `initial_state` 脚本在任务开始前把系统置于确定状态（如 `seed-settings.sh`
  保证浅色外观），`cleanup_script` 在任务结束后还原（如恢复 defaults）。
- `human_review` 型判据需要人工对照 trace 截图确认，报告中标 `partial`。

校验：

```bash
node -e 'import("./benchmarks/runner/lib/tasks.mjs").then(m => console.log(m.loadTasks().length))'
# → 30
```

## 运行

前置条件：

1. 本机已构建 `computer-use-mcp`（tarball 安装后 `command -v computer-use-mcp`
   在 PATH 中；runner 拒绝指向源码的伪路径）。
2. `opencode`（或 `pi`）已安装，模型 API key 已配置（见 `docs/opencode.md`）。
3. `cu daemon` 正在运行（首次运行会用 `osascript` 提示授予辅助功能权限）。

```bash
node benchmarks/runner/cu-bench.mjs list              # 列出 30 个任务
node benchmarks/runner/cu-bench.mjs run --suite smoke # 10 个冒烟任务
node benchmarks/runner/cu-bench.mjs run --suite full  # 全部 30 个
node benchmarks/runner/cu-bench.mjs run --tasks textedit-01,safari-03
node benchmarks/runner/cu-bench.mjs report            # 汇总最近一次运行
node benchmarks/runner/cu-bench.mjs compare <runA> <runB>
node benchmarks/runner/cu-bench.mjs replay <run-id>   # 重放 trace
```

运行输出在 `benchmarks/runs/<run-id>/`：`results.jsonl`、`summary.md/json`、
`failures.md`、`metrics.csv`、`environment.json`、`opencode.jsonl`、`opencode-results.md`、
`pi-results.md`（未验证时为 NOT VERIFIED）、`comparison.md`。

## 指标来源（诚实说明）

- `total_steps / observe_calls / action_batches / total_actions /
  stale_frame_count / cancelled_request_count / timeout_count / duration_ms /
  screenshot_bytes` 直接来自 runtime trace。
- `inspect_calls / recovery_count` 依赖宿主对 runtime 事件的实现。当前
  OpenCode 宿主不调用 `computer.inspect`、不显式恢复 stale 帧，故这两个
  指标在本轮为 0 —— 这是宿主行为的记录，不是测量噪声。
- 失败分类（`failure_category`，21 种枚举）由 `classifyFailure` 基于 trace
  事件启发式判定，`failure_detail` 附具体证据（最后 action、异常、超时）。

## 失败分类（failure_category）

由 `classifyFailure` 按优先级判定（严格来自 trace 事件，不猜测）：

| 类别 | 判定条件 |
|---|---|
| `SUCCESS` | evaluator 通过，且 trace 含 ≥1 observe 与 ≥1 action |
| `RUNTIME_NOT_DRIVEN` | evaluator 通过但 runtime 未被驱动（0 action 或 0 observe）——模型用旁路完成了目标，不算 runtime 成功。报告与 runner 的 pass 计数均**排除**该类（计入 model failures），只有 `SUCCESS` 计入成功 |
| `STALE_FRAME_RECOVERY_FAILED` | 出现 `act.stale_rejected` 且任务失败 |
| `CANCEL_FAILED` | 用户接管（`user_takeover`）导致失败 |
| `PERMISSION_ERROR` | 最后失败 action 报 permission |
| `ACTION_TIMEOUT` | 最后失败 action 报 timeout |
| `MODEL_STOPPED_EARLY` | 任务失败且 0 action（模型从未驱动桌面） |
| `SCROLL_DIRECTION_ERROR` / `DRAG_FAILED` | 最后失败 action 为 scroll / drag |
| `TEXT_INPUT_FAILED` / `UNICODE_INPUT_FAILED` | 最后失败 action 为 type（unicode/ime 细分） |
| `SMALL_TARGET_MISS` / `GROUNDING_MISS` | 最后失败 action 为 click（小目标/坐标未命中细分） |
| `MODEL_PLANNING_ERROR` | 以上都不是的失败 |
| `HARNESS_INTEGRATION_ERROR` | runner 自身抛错（非任务失败） |

完整 21 枚举的其余细分（如 `CONTROL_LOCKED` 等运行时错误码）从
`last_failed_action_error` 提取，规则与真实运行校准，随数据修订。

## 会话卫生（round 7）

每个任务前 runner 会 `cleanStaleSessions()`：用持久化凭据（`0600` 同 UID，
daemon 签发）停止所有遗留会话并清理已死亡会话的凭据记录。原因（真实
trace 发现）：一个客户端（如 `cu observe` 一次性调用或未收到 SIGTERM 的
MCP server）退出后若不停止自己启动的会话，控制锁会被无限持有，之后所有
`session start` 都以 `CONTROL_LOCKED` 失败。CLI 现在在退出时停止自己
自动启动的会话，runner 的清理兜底 SIGKILL 等不可捕获的退出方式。

## 无作弊保证

- evaluator 不启动应用、不检查窗口、不读取辅助功能树 —— 只看文件系统、
  `defaults`、HTTP fixture、应用运行状态（`tell application X to return
  running`）与人工复核。
- 每任务独立 scratch 目录；fixture 状态按任务 key 隔离并 `reset`。
- `success` 只由 evaluator 判定；模型停止早退、超时、stale 帧、用户接管
  全部计入失败分类，不隐藏。
- 真实截图不提交仓库；trace 通过 observation token（`0600`，同 UID）读取，
  不打印 token。
