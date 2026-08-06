// Task loading, validation, and placeholder substitution.
//
// Tasks are YAML data (benchmarks/tasks/**/*.yaml) conforming to
// benchmarks/schema/task.schema.json. The runner performs a strict-enough
// validation here (fields + criterion types) so a malformed task fails fast
// with a clear message; full schema conformance is checked by the JSON
// Schema itself (benchmarks/README.md shows how).
//
// Placeholders are substituted at load time:
//   {{SCRATCH}}     → the run's scratch directory for this task
//   {{FIXTURE_URL}} → the local fixture web app base URL
// A task that references a placeholder it did not declare in
// `environment` is a load error — no silent scratch dependence.

import { readdirSync, readFileSync, existsSync, statSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import YAML from "yaml";

export const BENCHMARKS_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
export const TASKS_DIR = join(BENCHMARKS_DIR, "tasks");

export const CATEGORIES = [
  "textedit",
  "finder",
  "system-settings",
  "calculator",
  "safari",
  "cross-app",
];

const CRITERION_TYPES = new Set([
  "file_exists", "file_contains", "file_not_contains", "file_absent",
  "dir_exists", "dir_contains", "defaults_matches", "http_check",
  "app_running", "human_review",
]);

/** Load every task YAML under benchmarks/tasks/ (one file per task). */
export function loadTasks() {
  const tasks = [];
  for (const category of CATEGORIES) {
    const dir = join(TASKS_DIR, category);
    if (!existsSync(dir)) continue;
    for (const file of readdirSync(dir).filter((f) => f.endsWith(".yaml") || f.endsWith(".yml")).sort()) {
      const doc = YAML.parse(readFileSync(join(dir, file), "utf8"));
      tasks.push(validateTask(doc, join(dir, file)));
    }
  }
  tasks.sort((a, b) => a.id.localeCompare(b.id));
  return tasks;
}

function validateTask(task, file) {
  const problems = [];
  if (!task || typeof task !== "object") problems.push("not an object");
  else {
    for (const req of ["id", "name", "category", "instruction", "success_criteria", "max_steps", "max_duration_seconds", "risk_level"]) {
      if (!(req in task)) problems.push(`missing required field '${req}'`);
    }
    if (task.id && !/^[a-z0-9]+(-[a-z0-9]+)*$/.test(task.id)) problems.push(`invalid id '${task.id}' (kebab-case)`);
    if (task.category && !CATEGORIES.includes(task.category)) problems.push(`unknown category '${task.category}'`);
    if (task.risk_level && !["low", "medium", "high"].includes(task.risk_level)) problems.push(`invalid risk_level '${task.risk_level}'`);
    if (Array.isArray(task.success_criteria)) {
      task.success_criteria.forEach((c, i) => {
        if (!c || !c.type || !CRITERION_TYPES.has(c.type)) problems.push(`success_criteria[${i}] has unknown type '${c?.type}'`);
      });
    } else if (task.success_criteria !== undefined) {
      problems.push("success_criteria must be an array");
    }
    const usesScratch = (task.instruction || "").includes("{{SCRATCH}}") ||
      (task.success_criteria || []).some((c) => JSON.stringify(c).includes("{{SCRATCH}}"));
    if (usesScratch && !task.environment?.scratch_required) problems.push("uses {{SCRATCH}} but environment.scratch_required is not true");
    const usesFixture = (task.instruction || "").includes("{{FIXTURE_URL}}") ||
      (task.success_criteria || []).some((c) => JSON.stringify(c).includes("{{FIXTURE_URL}}"));
    if (usesFixture && !task.environment?.fixture_required) problems.push("uses {{FIXTURE_URL}} but environment.fixture_required is not true");
  }
  if (problems.length) {
    throw new Error(`task ${file}: ${problems.join("; ")}`);
  }
  return task;
}

/** Resolve a task's text for a run (placeholder substitution). */
export function resolveTask(task, { scratchDir, fixtureUrl }) {
  const sub = (s) => String(s)
    .replaceAll("{{SCRATCH}}", scratchDir)
    .replaceAll("{{FIXTURE_URL}}", fixtureUrl);
  const clone = structuredClone(task);
  clone.instruction = sub(clone.instruction);
  for (const c of clone.success_criteria) {
    for (const key of Object.keys(c)) {
      if (typeof c[key] === "string") c[key] = sub(c[key]);
    }
  }
  clone._file = task._file;
  return clone;
}

/** Absolute path of a task-relative script (initial_state / cleanup_script). */
export function taskScriptPath(task, key) {
  const rel = task[key];
  if (!rel) return null;
  const abs = resolve(BENCHMARKS_DIR, rel);
  if (!existsSync(abs) || !statSync(abs).isFile()) {
    throw new Error(`task ${task.id}: ${key} script not found: ${rel}`);
  }
  return abs;
}
