// Polls the inspector's JSON endpoints and renders the dashboard.

const $ = (id) => document.getElementById(id);

function setStatus(ok, text) {
  const el = $("ready");
  el.className = `status ${ok ? "ok" : "bad"}`;
  el.textContent = text;
}

async function json(path, init) {
  const res = await fetch(path, init);
  return res.json();
}

function renderSession(s) {
  if (s.error) {
    $("session").textContent = `no active session (${s.error})`;
    setStatus(false, "no session");
    return;
  }
  setStatus(true, s.state);
  $("session").textContent = [
    `session_id  ${s.session_id}`,
    `state       ${s.state}${s.paused ? " (paused)" : ""}${s.user_takeover ? " (user takeover)" : ""}`,
    `lock_held   ${s.lock_held}`,
    `display     ${s.display_id}`,
    `started_by  ${s.started_by}`,
    `frame       ${s.current_frame_id ?? "-"}`,
  ].join("\n");
}

async function refresh() {
  try {
    const health = await json("/api/health");
    $("version").textContent = `v${health.version}`;
    $("uptime").textContent = `up ${health.uptime_secs}s`;
    if (!health.ready) {
      setStatus(false, "permissions missing");
      const p = health.permissions ?? {};
      $("session").textContent =
        `screen recording: ${p.screen_recording}\naccessibility: ${p.accessibility}`;
      return;
    }

    const session = await json("/api/session");
    renderSession(session);

    const frame = await json("/api/frame");
    if (!frame.error) {
      $("frame").src = `/api/frame-image?path=${encodeURIComponent(frame.image_path)}`;
      $("frame-meta").textContent =
        `${frame.frame_id} · ${frame.width}×${frame.height} · ${frame.active_application ?? "?"}`;
    }

    const pointer = await json("/api/pointer");
    $("pointer").textContent = `x ${pointer.location?.x}  y ${pointer.location?.y}`;

    const { traces } = await json("/api/traces");
    $("traces").querySelector("tbody").innerHTML = traces
      .map(
        (t) =>
          `<tr><td>${t.session_id}</td><td>${t.entries}</td><td>${t.bytes}</td><td>${new Date(t.started_at).toLocaleTimeString()}</td></tr>`,
      )
      .join("");
  } catch (err) {
    setStatus(false, "daemon unreachable");
    $("session").textContent = `cannot reach daemon: ${err.message}`;
  }
}

refresh();
setInterval(refresh, 3000);
