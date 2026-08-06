// Independent success evaluation. The runner never asks the model whether it
// succeeded; each success_criterion is checked against real system state:
// the filesystem, `defaults` (system settings), the fixture web app's
// server-side state, or a running app. `human_review` criteria are recorded
// as pass-with-pending-human-verdict (the report lists them separately).
//
// Deliberately no Accessibility-tree inspection and no window heuristics:
// criteria must be satisfiable in multiple ways by the model, and checkable
// without the model's help.

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";

/** Evaluate all criteria. Returns { pass, humanReview, details: [{type, pass, detail}] } */
export async function evaluateTask(task, { fixtureUrl }) {
  const details = [];
  let humanReview = false;
  for (const c of task.success_criteria) {
    const r = await evaluateCriterion(c, { fixtureUrl });
    if (c.type === "human_review") humanReview = true;
    details.push(r);
  }
  return { pass: details.every((d) => d.pass), humanReview, details };
}

async function evaluateCriterion(c, { fixtureUrl }) {
  switch (c.type) {
    case "file_exists": return fileCheck(c, () => existsSync(c.path) && statSync(c.path).isFile());
    case "file_contains": {
      let detail = null;
      const pass = existsSync(c.path) && (() => {
        try {
          const content = readFileSync(c.path, "utf8");
          const ok = content.includes(c.contains);
          detail = ok ? null : `file present but missing '${c.contains}'`;
          return ok;
        } catch (e) { detail = `unreadable: ${e.message}`; return false; }
      })();
      return { type: c.type, pass, detail };
    }
    case "file_not_contains": {
      let detail = null;
      const pass = existsSync(c.path) && (() => {
        try {
          const content = readFileSync(c.path, "utf8");
          const ok = !content.includes(c.contains);
          detail = ok ? null : `file still contains '${c.contains}'`;
          return ok;
        } catch (e) { detail = `unreadable: ${e.message}`; return false; }
      })();
      return { type: c.type, pass, detail };
    }
    case "file_absent": return fileCheck(c, () => !existsSync(c.path));
    case "dir_exists": return fileCheck(c, () => existsSync(c.path) && statSync(c.path).isDirectory());
    case "dir_contains": {
      let detail = null;
      let pass = false;
      try {
        if (existsSync(c.path) && statSync(c.path).isDirectory()) {
          const re = new RegExp("^" + String(c.pattern).replace(/[.+?^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*") + "$");
          const hit = readdirSync(c.path).find((f) => re.test(f));
          pass = Boolean(hit);
          detail = pass ? `found '${hit}'` : `no entry matches '${c.pattern}'`;
        } else {
          detail = `${c.path} is not a directory`;
        }
      } catch (e) { detail = `${c.path}: ${e.message}`; }
      return { type: c.type, pass, detail };
    }
    case "defaults_matches": {
      const r = spawnSync("defaults", ["read", c.defaults_domain, c.defaults_key], { encoding: "utf8" });
      const actual = r.status === 0 ? r.stdout.trim() : null;
      const expected = typeof c.defaults_expected === "boolean" ? (c.defaults_expected ? "1" : "0") : String(c.defaults_expected);
      return { type: c.type, pass: actual !== null && actual === expected, detail: actual === null ? `defaults read failed (key absent?)` : `got '${actual}', expected '${expected}'` };
    }
    case "http_check": {
      if (!fixtureUrl) return { type: c.type, pass: false, detail: "no fixture server running" };
      try {
        const res = await fetch(`${fixtureUrl}/check?task=${encodeURIComponent(c.task_key)}`);
        const body = await res.json();
        return { type: c.type, pass: body.satisfied === true, detail: body.detail || `fixture state for '${c.task_key}': satisfied=${body.satisfied}` };
      } catch (e) {
        return { type: c.type, pass: false, detail: `fixture check failed: ${e.message}` };
      }
    }
    case "app_running": {
      // `tell application X to running` never launches the app and answers a
      // boolean over the same app-identity matching the macOS UI.
      const r = spawnSync("osascript", ["-e", `tell application "${c.app}" to return running`], { encoding: "utf8" });
      const pass = r.status === 0 && r.stdout.trim() === "true";
      return { type: c.type, pass, detail: pass ? undefined : `app '${c.app}' not running` };
    }
    case "human_review": {
      return { type: c.type, pass: true, detail: `HUMAN REVIEW: ${c.reason || "verify via trace screenshots"}` };
    }
    default:
      return { type: c.type, pass: false, detail: `unknown criterion type` };
  }
}

function fileCheck(c, predicate) {
  try {
    const pass = predicate();
    return { type: c.type, pass, detail: pass ? undefined : `not satisfied for ${c.path}` };
  } catch (e) {
    return { type: c.type, pass: false, detail: `${c.path}: ${e.message}` };
  }
}
