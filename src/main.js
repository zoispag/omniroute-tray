import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

const STATE_LABELS = {
  stopped: "Stopped",
  starting: "Starting…",
  running: "Running",
  "update-available": "Update available",
  updating: "Updating…",
  error: "Error",
};

let lastStatus = null;
let showUsed = localStorage.getItem("quotaMode") === "used";

async function refresh() {
  try {
    const status = await invoke("get_status");
    lastStatus = status;
    renderHeader(status);
    renderUpdate(status);
    if (status.state === "running" || status.state === "update-available") {
      await Promise.all([renderRateLimits(), renderQuota(), renderCost()]);
    } else {
      clearSections();
    }
  } catch (err) {
    renderHeader({ state: "error", reason: String(err) });
  }
}

function renderUpdate(status) {
  const el = document.getElementById("update");
  if (status.state === "update-available") {
    el.innerHTML = `<button id="update-btn">Update to v${status.latest}</button>`;
    document.getElementById("update-btn").onclick = async (e) => {
      e.target.disabled = true;
      e.target.textContent = "Updating…";
      try {
        await invoke("apply_update", { target: status.latest });
      } catch (err) {
        e.target.textContent = `Failed: ${err}`;
      }
      refresh();
    };
  } else if (status.state === "updating") {
    el.innerHTML = `<button disabled>Updating to v${status.target}…</button>`;
  } else {
    el.innerHTML = "";
  }
}

function renderHeader(status) {
  const dot = document.querySelector(".status-dot");
  dot.dataset.state = status.state ?? "unknown";
  document.getElementById("state-label").textContent =
    STATE_LABELS[status.state] ?? "Unknown";
  document.getElementById("version").textContent = status.version
    ? `v${status.version}`
    : "";
  if (status.state === "error" && status.reason) {
    document.getElementById("content").innerHTML =
      `<p class="error">${status.reason}</p>`;
  } else if (status.state !== "error") {
    const err = document.querySelector("#content .error");
    if (err) err.remove();
  }
}

async function renderRateLimits() {
  const accounts = await invoke("get_rate_limits");
  const section = document.getElementById("ratelimits");
  if (!accounts.length) {
    section.innerHTML = "";
    return;
  }
  const toggle = `<button id="mode-toggle" class="mode-toggle">${
    showUsed ? "% used" : "% left"
  }</button>`;
  const blocks = accounts
    .map((acc) => {
      const windows = acc.windows
        .map((w) => {
          const used = w.used_percent;
          const left = 100 - used;
          const shown = showUsed ? used : left;
          const lowLeft = left < 20;
          const reset = w.reset_at ? formatReset(w.reset_at) : "";
          const color = left > 40 ? "var(--good)" : left > 15 ? "var(--warn)" : "var(--bad)";
          return `
            <div class="row">
              <div class="row-head">
                <span class="provider">${w.label}</span>
                <span class="pct ${lowLeft ? "low" : ""}">${shown.toFixed(0)}% ${showUsed ? "used" : "left"}</span>
              </div>
              <div class="bar"><div class="bar-fill" style="width:${left}%;background:${color}"></div></div>
              <div class="reset-line">${reset}</div>
            </div>`;
        })
        .join("");
      return `<div class="account">${acc.account}</div>${windows}`;
    })
    .join("");
  section.innerHTML = `<div class="section-head"><h3>Claude</h3>${toggle}</div>${blocks}`;
  const btn = document.getElementById("mode-toggle");
  if (btn) {
    btn.onclick = () => {
      showUsed = !showUsed;
      localStorage.setItem("quotaMode", showUsed ? "used" : "left");
      renderRateLimits();
    };
  }
}

async function renderQuota() {
  const rows = await invoke("get_quota");
  const section = document.getElementById("quota");
  if (!rows.length) {
    section.innerHTML = "";
    return;
  }
  const items = rows
    .map((r) => {
      const pct = quotaPercent(r);
      const reset = r.resetAt ? formatReset(r.resetAt) : "";
      return `
        <div class="row">
          <div class="row-head">
            <span class="provider">${r.provider}</span>
            <span class="reset">${reset}</span>
          </div>
          <div class="bar"><div class="bar-fill" style="width:${pct}%"></div></div>
        </div>`;
    })
    .join("");
  section.innerHTML = `<h3>Providers</h3>${items}`;
}

function quotaPercent(row) {
  if (row.limit && row.used != null) {
    return Math.max(0, Math.min(100, (1 - row.used / row.limit) * 100));
  }
  if (row.remaining != null) {
    return Math.max(0, Math.min(100, row.remaining));
  }
  return 100;
}

function formatReset(value) {
  let then;
  if (/^\d+$/.test(String(value))) {
    const n = Number(value);
    then = n < 1e12 ? n * 1000 : n;
  } else {
    then = new Date(value).getTime();
  }
  const diff = then - Date.now();
  if (Number.isNaN(then) || diff <= 0) return "";
  const totalMin = Math.floor(diff / 6e4);
  const d = Math.floor(totalMin / 1440);
  const h = Math.floor((totalMin % 1440) / 60);
  const m = totalMin % 60;
  if (d > 0) return `reset in ${d}d ${h}h`;
  if (h > 0) return `reset in ${h}h ${m}m`;
  return `reset in ${m}m`;
}

async function renderCost() {
  const result = await invoke("get_cost");
  const section = document.getElementById("cost");
  if (result.status === "needs-api-key") {
    section.innerHTML = `<a class="connect" href="http://127.0.0.1:20128" target="_blank">Connect API key →</a>`;
    return;
  }
  if (result.status !== "available" || !result.rows?.length) {
    section.innerHTML = "";
    return;
  }
  const rows = result.rows;
  const total = rows.reduce((s, r) => s + (r.cost ?? 0), 0);
  const totalTokens = rows.reduce(
    (s, r) => s + (r.tokens_in ?? 0) + (r.tokens_out ?? 0),
    0
  );
  const top = rows
    .slice()
    .sort((a, b) => (b.cost ?? 0) - (a.cost ?? 0))
    .slice(0, 4)
    .map((r) => {
      const share = total > 0 ? ((r.cost ?? 0) / total) * 100 : 0;
      return `<div class="cost-row"><span>${r.model}</span><span>${share.toFixed(1)}%</span></div>`;
    })
    .join("");
  section.innerHTML = `
    <h3>Cost (30d)</h3>
    <div class="cost-total">$${total.toFixed(2)} · ${formatTokens(totalTokens)}</div>
    ${top}`;
}

function formatTokens(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)}M tokens`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)}K tokens`;
  return `${n} tokens`;
}

function clearSections() {
  document.getElementById("ratelimits").innerHTML = "";
  document.getElementById("quota").innerHTML = "";
  document.getElementById("cost").innerHTML = "";
}

async function runDoctor() {
  const section = document.getElementById("cost");
  try {
    const report = await invoke("run_doctor");
    const rows = report.checks
      .map(
        (c) =>
          `<div class="cost-row"><span>${c.name}</span><span class="doctor-${c.status}">${c.status}</span></div>`
      )
      .join("");
    section.innerHTML = `<h3>Doctor ${report.healthy ? "✓" : "✕"}</h3>${rows}`;
  } catch (err) {
    section.innerHTML = `<p class="error">${err}</p>`;
  }
}

async function fitWindow() {
  const height = Math.min(600, document.getElementById("app").scrollHeight + 8);
  try {
    const { LogicalSize } = await import("@tauri-apps/api/dpi");
    await getCurrentWindow().setSize(new LogicalSize(340, height));
  } catch {}
}

getCurrentWindow().listen("run-doctor", runDoctor);

async function tick() {
  await refresh();
  await fitWindow();
}

tick();
setInterval(tick, 5000);
