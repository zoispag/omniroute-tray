import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PROVIDER_ICONS } from "./icons.js";

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
      await Promise.all([renderRateLimits(), renderCost(), renderTrend()]);
    } else {
      clearSections();
    }
  } catch (err) {
    renderHeader({ state: "error", reason: String(err) });
  }
}

function renderUpdate(status) {
  const el = document.getElementById("update");
  if (!el) return;
  if (status.state === "update-available" && status.latest) {
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
  const errEl = document.getElementById("error");
  if (status.state === "error" && status.reason) {
    errEl.innerHTML = `<p class="error">${status.reason}</p>`;
  } else {
    errEl.innerHTML = "";
  }
}

let hiddenAccounts = new Set(
  JSON.parse(localStorage.getItem("hiddenAccounts") || "[]")
);

function accountKey(acc) {
  return `${acc.provider}/${acc.account}`;
}

let rateLimitCache = [];

function providerBadge(provider) {
  const icon = PROVIDER_ICONS[provider];
  if (icon) {
    return `<span class="prov-badge" title="${provider}">${icon}</span>`;
  }
  return `<span class="prov-badge prov-fallback" title="${provider}">●</span>`;
}

function toggleHidden(key) {
  if (hiddenAccounts.has(key)) hiddenAccounts.delete(key);
  else hiddenAccounts.add(key);
  localStorage.setItem("hiddenAccounts", JSON.stringify([...hiddenAccounts]));
  paintRateLimits();
}

async function renderRateLimits() {
  rateLimitCache = await invoke("get_rate_limits");
  paintRateLimits();
}

function paintRateLimits() {
  const section = document.getElementById("ratelimits");
  if (!rateLimitCache.length) {
    section.innerHTML = "";
    return;
  }
  const toggle = `<button id="mode-toggle" class="mode-toggle">${
    showUsed ? "% used" : "% left"
  }</button>`;
  const blocks = rateLimitCache
    .map((acc) => {
      const key = accountKey(acc);
      const hidden = hiddenAccounts.has(key);
      const eye = hidden ? "show" : "hide";
      const head = `<div class="account">${providerBadge(acc.provider)}<span class="acct-name">${acc.account}</span><button class="acct-toggle" data-key="${key}">${eye}</button></div>`;
      if (hidden) return head;
      const windows = acc.windows
        .map((w) => {
          const used = w.used_percent;
          const left = 100 - used;
          const shown = showUsed ? used : left;
          const fill = showUsed ? used : left;
          const reset = w.reset_at ? formatResetShort(w.reset_at) : "";
          const label = refineLabel(w.short, w.reset_at);
          const color =
            left > 40 ? "var(--good)" : left > 15 ? "var(--warn)" : "var(--bad)";
          return `
            <div class="qrow">
              <span class="qlabel">${label}</span>
              <span class="qbar"><span class="qfill" style="width:${fill}%;background:${color}"></span></span>
              <span class="qpct" style="color:${color}">${shown.toFixed(0)}%</span>
              <span class="qreset">${reset}</span>
            </div>`;
        })
        .join("");
      return head + windows;
    })
    .join("");
  section.innerHTML = `<div class="section-head"><h3>Usage</h3>${toggle}</div>${blocks}`;
  const btn = document.getElementById("mode-toggle");
  if (btn) {
    btn.onclick = () => {
      showUsed = !showUsed;
      localStorage.setItem("quotaMode", showUsed ? "used" : "left");
      paintRateLimits();
    };
  }
  section.querySelectorAll(".acct-toggle").forEach((b) => {
    b.onclick = () => toggleHidden(b.dataset.key);
  });
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

function resetMinutes(value) {
  let then;
  if (/^\d+$/.test(String(value))) {
    const n = Number(value);
    then = n < 1e12 ? n * 1000 : n;
  } else {
    then = new Date(value).getTime();
  }
  const diff = then - Date.now();
  if (Number.isNaN(then) || diff <= 0) return null;
  return Math.floor(diff / 6e4);
}

function refineLabel(short, resetAt) {
  if (short !== "sess" || !resetAt) return short;
  const mins = resetMinutes(resetAt);
  if (mins == null) return short;
  const days = mins / 1440;
  if (days >= 25) return "mo";
  if (days >= 5) return "wk";
  if (days >= 0.8) return "1d";
  return short;
}

function formatResetShort(value) {
  const totalMin = resetMinutes(value);
  if (totalMin == null) return "";
  const d = Math.floor(totalMin / 1440);
  const h = Math.floor((totalMin % 1440) / 60);
  const m = totalMin % 60;
  return d > 0 ? `${d}d${h}h` : `${h}h${m}m`;
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

function money(n) {
  return `$${(n ?? 0).toFixed(2)}`;
}

function sparkline(days) {
  if (!days.length) return "";
  const max = Math.max(...days.map((d) => d.cost), 0.0001);
  const bars = days
    .map((d) => {
      const h = Math.max(2, Math.round((d.cost / max) * 24));
      return `<span class="spark-bar" style="height:${h}px" data-date="${d.date}" data-cost="${money(d.cost)}" data-tokens="${formatTokens(d.tokens)}"></span>`;
    })
    .join("");
  return `<div class="spark">${bars}</div><div class="spark-tip" id="spark-tip" hidden></div>`;
}

function wireSparkline() {
  const spark = document.querySelector(".spark");
  const tip = document.getElementById("spark-tip");
  if (!spark || !tip) return;
  spark.querySelectorAll(".spark-bar").forEach((bar) => {
    bar.onmouseenter = () => {
      tip.innerHTML = `<strong>${bar.dataset.date}</strong>${bar.dataset.cost} · ${bar.dataset.tokens}`;
      tip.hidden = false;
      const sr = spark.getBoundingClientRect();
      const br = bar.getBoundingClientRect();
      tip.style.left = `${br.left - sr.left + br.width / 2}px`;
    };
    bar.onmouseleave = () => {
      tip.hidden = true;
    };
  });
}

async function renderTrend() {
  const t = await invoke("get_usage_trend");
  const section = document.getElementById("trend");
  if (!t || !t.days.length) {
    section.innerHTML = "";
    return;
  }
  section.innerHTML = `
    <h3>Usage Trend</h3>
    <div class="cost-row"><span>Today</span><span>${money(t.today_cost)} · ${formatTokens(t.today_tokens)}</span></div>
    <div class="cost-row"><span>Yesterday</span><span>${money(t.yesterday_cost)} · ${formatTokens(t.yesterday_tokens)}</span></div>
    <div class="cost-row"><span>Last 30 Days</span><span>${money(t.total_cost)} · ${formatTokens(t.total_tokens)}</span></div>
    ${sparkline(t.days)}`;
  wireSparkline();
}

async function renderVersion() {
  const el = document.getElementById("app-version");
  if (el.textContent) return;
  try {
    el.textContent = `OmniRouteTray ${await invoke("get_app_version")}`;
  } catch {}
}

function clearSections() {
  document.getElementById("ratelimits").innerHTML = "";
  document.getElementById("cost").innerHTML = "";
  document.getElementById("trend").innerHTML = "";
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
  const app = document.getElementById("app");
  const height = Math.min(620, app.offsetHeight + 16);
  try {
    const { LogicalSize } = await import("@tauri-apps/api/dpi");
    await getCurrentWindow().setSize(new LogicalSize(332, height));
  } catch {}
}

getCurrentWindow().listen("run-doctor", runDoctor);

async function tick() {
  await refresh();
  await renderVersion();
  await fitWindow();
}

async function loop() {
  await tick();
  setTimeout(loop, 5000);
}

loop();
