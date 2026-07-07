import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import {
  PROVIDER_ICONS,
  GITHUB_ICON,
  GEAR_ICON,
  REFRESH_ICON,
} from "./icons.js";

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
let showInactiveProviders =
  localStorage.getItem("showInactiveProviders") === "true";
let costShowTokens = localStorage.getItem("costMode") === "tokens";
let costCache = null;

const SECTION_LABELS = [
  ["health", "Provider health"],
  ["usage", "Usage"],
  ["cost", "Cost"],
  ["trend", "Usage trend"],
];
let hiddenSections = new Set(
  JSON.parse(localStorage.getItem("hiddenSections") || "[]")
);

function sectionVisible(key) {
  return !hiddenSections.has(key);
}

function setSectionHidden(key, hidden) {
  if (hidden) hiddenSections.add(key);
  else hiddenSections.delete(key);
  localStorage.setItem("hiddenSections", JSON.stringify([...hiddenSections]));
}

async function refresh() {
  try {
    const status = await invoke("get_status");
    lastStatus = status;
    renderHeader(status);
    if (inSettings) {
      return;
    }
    renderUpdate(status);
    if (status.state === "running" || status.state === "update-available") {
      const jobs = [];
      jobs.push(sectionVisible("health") ? renderStatusBand() : hideSection("statusband"));
      jobs.push(sectionVisible("usage") ? renderRateLimits() : hideSection("ratelimits"));
      jobs.push(sectionVisible("cost") ? renderCost() : hideSection("cost"));
      jobs.push(sectionVisible("trend") ? renderTrend() : hideSection("trend"));
      await Promise.all(jobs);
    } else {
      clearSections();
    }
  } catch (err) {
    renderHeader({ state: "error", reason: String(err) });
  }
}

async function toggleSettings() {
  inSettings = !inSettings;
  document.getElementById("gear-btn")?.classList.toggle("active", inSettings);
  if (inSettings) {
    if (!rateLimitCache.length) {
      try {
        rateLimitCache = await invoke("get_rate_limits");
      } catch {}
    }
    document.getElementById("update").innerHTML = "";
    clearSections();
    await renderSettings();
  } else {
    document.getElementById("content").innerHTML = mainContentHTML();
    paintRateLimits();
    fitWindow();
    await refresh();
  }
  fitWindow();
}

function mainContentHTML() {
  return `
    <div id="error" class="section"></div>
    <div id="statusband" class="section"></div>
    <div id="ratelimits" class="section"></div>
    <div id="cost" class="section"></div>
    <div id="trend" class="section"></div>
    <div id="update" class="section"></div>`;
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
    const reason = status.reason.replace(
      /View Logs/g,
      '<a href="#" id="view-logs-link">View Logs</a>'
    );
    errEl.innerHTML = `
      <p class="error">${reason}</p>
      <div class="error-actions">
        <button id="restart-btn" class="err-btn">Restart Server</button>
        <button id="logs-link-btn" class="err-btn ghost">View Logs</button>
      </div>`;
    document.getElementById("restart-btn").onclick = async (e) => {
      e.target.disabled = true;
      e.target.textContent = "Restarting…";
      try {
        await invoke("restart_server");
      } catch {}
    };
    document.getElementById("logs-link-btn").onclick = openLogs;
    document.getElementById("view-logs-link")?.addEventListener("click", (e) => {
      e.preventDefault();
      openLogs();
    });
  } else {
    errEl.innerHTML = "";
  }
}

async function openLogs() {
  try {
    await invoke("open_logs");
  } catch {}
}

let hiddenAccounts = new Set(
  JSON.parse(localStorage.getItem("hiddenAccounts") || "[]")
);
let accountOrder = JSON.parse(localStorage.getItem("accountOrder") || "[]");
let inSettings = false;

function accountKey(acc) {
  return `${acc.provider}/${acc.account}`;
}

function saveOrder() {
  localStorage.setItem("accountOrder", JSON.stringify(accountOrder));
}

let providerOrder = JSON.parse(localStorage.getItem("providerOrder") || "[]");

function saveProviderOrder() {
  localStorage.setItem("providerOrder", JSON.stringify(providerOrder));
}

function groupByProvider(accounts) {
  const groups = new Map();
  for (const a of accounts) {
    if (!groups.has(a.provider)) groups.set(a.provider, []);
    groups.get(a.provider).push(a);
  }
  const known = new Set(providerOrder);
  for (const p of groups.keys()) {
    if (!known.has(p)) {
      providerOrder.push(p);
      known.add(p);
    }
  }
  const rank = new Map(providerOrder.map((p, i) => [p, i]));
  return [...groups.entries()].sort(
    (a, b) => (rank.get(a[0]) ?? 999) - (rank.get(b[0]) ?? 999)
  );
}

let rateLimitCache = [];

function providerBadge(provider) {
  const icon = PROVIDER_ICONS[provider];
  if (icon) {
    return `<span class="prov-badge" title="${provider}">${icon}</span>`;
  }
  return `<span class="prov-badge prov-fallback" title="${provider}">●</span>`;
}

function setAccountHidden(key, hidden) {
  if (hidden) hiddenAccounts.add(key);
  else hiddenAccounts.delete(key);
  localStorage.setItem("hiddenAccounts", JSON.stringify([...hiddenAccounts]));
}

async function renderRateLimits() {
  if (!rateLimitCache.length) {
    document.getElementById("ratelimits").innerHTML = usageSkeleton();
  }
  try {
    const data = await invoke("get_rate_limits");
    if (Array.isArray(data)) rateLimitCache = data;
  } catch {
    // keep last-known cache on transient failure
  }
  paintRateLimits();
}

function usageSkeleton() {
  const row = `
    <div class="account"><span class="skel skel-icon"></span><span class="skel skel-name"></span></div>
    <div class="qrow"><span class="skel skel-label"></span><span class="skel skel-bar"></span></div>
    <div class="qrow"><span class="skel skel-label"></span><span class="skel skel-bar"></span></div>`;
  return `<div class="section-head"><h3>Usage</h3></div>${row}${row}`;
}

function paintRateLimits() {
  const section = document.getElementById("ratelimits");
  if (!rateLimitCache.length) {
    section.innerHTML = usageSkeleton();
    return;
  }
  const toggle = `<button id="mode-toggle" class="mode-toggle">${
    showUsed ? "% used" : "% left"
  }</button>`;
  const groups = groupByProvider(rateLimitCache)
    .map(([provider, accts]) => {
      const visible = accts.filter((a) => !hiddenAccounts.has(accountKey(a)));
      return [provider, visible];
    })
    .filter(([, visible]) => visible.length);

  if (!groups.length) {
    section.innerHTML = `<div class="section-head"><h3>Usage</h3>${toggle}</div><p class="placeholder">All accounts hidden. Enable in settings.</p>`;
    wireModeToggle();
    return;
  }

  const blocks = groups
    .map(([, accts]) => {
      const rows = accts
        .map((acc) => {
          const head = `<div class="account">${providerBadge(acc.provider)}<span class="acct-name">${acc.account}</span></div>`;
          const windows = acc.windows
            .map((w) => {
              const used = w.used_percent;
              const left = 100 - used;
              const shown = showUsed ? used : left;
              const fill = showUsed ? used : left;
              const reset = w.reset_at ? formatResetShort(w.reset_at) : "";
              const absolute = w.reset_at ? formatResetAbsolute(w.reset_at) : "";
              const label = refineLabel(w.short, w.reset_at);
              const color =
                left > 40 ? "var(--good)" : left > 15 ? "var(--warn)" : "var(--bad)";
              const resetCell = reset
                ? `<span class="qreset" data-tip="${absolute}">${reset}</span>`
                : `<span class="qreset"></span>`;
              return `
                <div class="qrow">
                  <span class="qlabel">${label}</span>
                  <span class="qbar"><span class="qfill" style="width:${fill}%;background:${color}"></span></span>
                  <span class="qpct" style="color:${color}">${shown.toFixed(0)}%</span>
                  ${resetCell}
                </div>`;
            })
            .join("");
          return head + windows;
        })
        .join("");
      return rows;
    })
    .join("");
  section.innerHTML = `<div class="section-head"><h3>Usage</h3>${toggle}</div>${blocks}`;
  wireModeToggle();
}

function wireModeToggle() {
  const btn = document.getElementById("mode-toggle");
  if (btn) {
    btn.onclick = () => {
      showUsed = !showUsed;
      localStorage.setItem("quotaMode", showUsed ? "used" : "left");
      paintRateLimits();
    };
  }
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

function formatResetAbsolute(value) {
  let then;
  if (/^\d+$/.test(String(value))) {
    const n = Number(value);
    then = n < 1e12 ? n * 1000 : n;
  } else {
    then = new Date(value).getTime();
  }
  if (Number.isNaN(then)) return "";
  const dt = new Date(then);
  const time = dt.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  const today = new Date();
  const sameDay = dt.toDateString() === today.toDateString();
  const tomorrow = new Date(today);
  tomorrow.setDate(today.getDate() + 1);
  const isTomorrow = dt.toDateString() === tomorrow.toDateString();
  if (sameDay) return `Resets today at ${time}`;
  if (isTomorrow) return `Resets tomorrow at ${time}`;
  const day = dt.toLocaleDateString([], { month: "short", day: "numeric" });
  return `Resets ${day} at ${time}`;
}



async function renderCost() {
  const result = await invoke("get_cost");
  costCache = result;
  paintCost();
}

function paintCost() {
  const section = document.getElementById("cost");
  if (!section || !costCache) return;
  const result = costCache;
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
  const toggle = `<button id="cost-toggle" class="mode-toggle">${
    costShowTokens ? "in/out" : "%"
  }</button>`;
  const top = rows
    .slice()
    .sort((a, b) => (b.cost ?? 0) - (a.cost ?? 0))
    .slice(0, 4)
    .map((r) => {
      const value = costShowTokens
        ? `${compactTokens(r.tokens_in ?? 0)} in · ${compactTokens(
            r.tokens_out ?? 0
          )} out`
        : `${(total > 0 ? ((r.cost ?? 0) / total) * 100 : 0).toFixed(1)}%`;
      return `<div class="cost-row"><span class="cost-model">${r.model}</span><span class="cost-value">${value}</span></div>`;
    })
    .join("");
  section.innerHTML = `
    <div class="section-head"><h3>Cost (30d)</h3>${toggle}</div>
    <div class="cost-total">$${total.toFixed(2)} · ${formatTokens(totalTokens)}</div>
    ${top}`;

  const btn = document.getElementById("cost-toggle");
  if (btn) {
    btn.onclick = () => {
      costShowTokens = !costShowTokens;
      localStorage.setItem("costMode", costShowTokens ? "tokens" : "pct");
      paintCost();
    };
  }
}

function formatTokens(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)}M tokens`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)}K tokens`;
  return `${n} tokens`;
}

function compactTokens(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)}K`;
  return `${n}`;
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

async function renderSettings() {
  const content = document.getElementById("content");
  let autostart = false;
  try {
    autostart = await invoke("get_autostart");
  } catch {}

  const groups = groupByProvider(rateLimitCache);
  const providers = groups.map(([p]) => p);
  const providerRows = groups
    .map(([provider, accts], i) => {
      const acctList = accts
        .map((acc) => {
          const key = accountKey(acc);
          const checked = hiddenAccounts.has(key) ? "" : "checked";
          return `
            <label class="set-acct">
              <input type="checkbox" class="set-check" data-key="${key}" ${checked} />
              <span class="acct-name">${acc.account}</span>
            </label>`;
        })
        .join("");
      return `
        <li class="set-row" data-provider="${provider}">
          <span class="set-reorder">
            <button class="move-btn" data-provider="${provider}" data-dir="up" ${i === 0 ? "disabled" : ""}>▲</button>
            <button class="move-btn" data-provider="${provider}" data-dir="down" ${i === providers.length - 1 ? "disabled" : ""}>▼</button>
          </span>
          ${providerBadge(provider)}
          <span class="set-provider-name">${provider}</span>
          <span class="set-accts">${acctList}</span>
        </li>`;
    })
    .join("");
  const accountRows = providerRows;

  content.innerHTML = `
    <div class="settings-head">
      <button id="back-btn" class="back-btn">← Back</button>
      <span class="settings-title">Settings</span>
    </div>
    <div class="section">
      <h3>Start on Login</h3>
      <label class="set-toggle">
        <input type="checkbox" id="autostart-check" ${autostart ? "checked" : ""} />
        <span>Launch OmniRouteTray when you sign in</span>
      </label>
    </div>
    <div class="section">
      <h3>Status Bar</h3>
      <label class="set-toggle">
        <input type="checkbox" id="inactive-providers-check" ${
          showInactiveProviders ? "checked" : ""
        } />
        <span>Show inactive providers in the health strip</span>
      </label>
    </div>
    <div class="section">
      <h3>Sections</h3>
      ${SECTION_LABELS.map(
        ([key, label]) => `
      <label class="set-toggle">
        <input type="checkbox" class="section-check" data-section="${key}" ${
          sectionVisible(key) ? "checked" : ""
        } />
        <span>${label}</span>
      </label>`
      ).join("")}
    </div>
    <div class="section">
      <h3>Accounts</h3>
      <ul class="set-list" id="set-list">${accountRows}</ul>
    </div>`;

  document.getElementById("back-btn").onclick = toggleSettings;

  const auto = document.getElementById("autostart-check");
  if (auto) {
    auto.onchange = async () => {
      try {
        await invoke("set_autostart", { enabled: auto.checked });
      } catch {}
    };
  }

  const inactive = document.getElementById("inactive-providers-check");
  if (inactive) {
    inactive.onchange = () => {
      showInactiveProviders = inactive.checked;
      localStorage.setItem(
        "showInactiveProviders",
        showInactiveProviders ? "true" : "false"
      );
    };
  }
  content.querySelectorAll(".section-check").forEach((c) => {
    c.onchange = () => setSectionHidden(c.dataset.section, !c.checked);
  });
  content.querySelectorAll(".set-check").forEach((c) => {
    c.onchange = () => setAccountHidden(c.dataset.key, !c.checked);
  });
  content.querySelectorAll(".move-btn").forEach((b) => {
    b.onclick = () => moveProvider(b.dataset.provider, b.dataset.dir);
  });
}

function moveProvider(provider, dir) {
  const providers = groupByProvider(rateLimitCache).map(([p]) => p);
  const i = providers.indexOf(provider);
  const j = dir === "up" ? i - 1 : i + 1;
  if (i < 0 || j < 0 || j >= providers.length) return;
  [providers[i], providers[j]] = [providers[j], providers[i]];
  providerOrder = providers;
  saveProviderOrder();
  renderSettings();
}

async function renderVersion() {
  const el = document.getElementById("app-version");
  if (el.dataset.ready) return;
  try {
    const [version, port] = await Promise.all([
      invoke("get_app_version"),
      invoke("get_port"),
    ]);
    el.innerHTML = `<span class="app-name">OmniRouteTray ${version}</span><button class="port-link" id="port-link" title="Open OmniRoute dashboard">:${port}</button>`;
    el.dataset.ready = "1";
    document.getElementById("port-link")?.addEventListener("click", () => {
      invoke("open_url", { url: `http://127.0.0.1:${port}` }).catch(() => {});
    });
    renderTrayUpdate();
  } catch {}
}

async function renderTrayUpdate() {
  const help = document.getElementById("help-btn");
  if (!help) return;
  try {
    const u = await invoke("get_tray_update");
    if (u && u.available) {
      help.classList.add("has-update");
      help.title = `Update available: v${u.latest}. View on GitHub`;
    }
  } catch {}
}

function hideSection(id) {
  const el = document.getElementById(id);
  if (el) el.innerHTML = "";
}

function clearSections() {
  const band = document.getElementById("statusband");
  if (band) band.innerHTML = "";
  document.getElementById("ratelimits").innerHTML = "";
  document.getElementById("cost").innerHTML = "";
  document.getElementById("trend").innerHTML = "";
}

function fmtLatency(ms) {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
}

function providerPanelRows(providers, showInactive) {
  return providers
    .filter((p) => showInactive || p.active)
    .map((p) => {
      const state = p.breaker_open ? "bad" : p.active ? "good" : "off";
      const tag = p.breaker_open ? "breaker open" : p.active ? "" : "off";
      return `
        <div class="prov-row">
          <span class="prov-dot" data-state="${state}"></span>
          <span class="prov-name">${p.name}</span>
          ${tag ? `<span class="prov-tag prov-tag-${state}">${tag}</span>` : ""}
        </div>`;
    })
    .join("");
}

async function renderStatusBand() {
  const el = document.getElementById("statusband");
  if (!el) return;
  let h;
  try {
    h = await invoke("get_health");
  } catch {
    el.innerHTML = "";
    return;
  }

  const providers = Array.isArray(h.providers) ? h.providers : [];
  const denom = showInactiveProviders
    ? h.configured_providers
    : h.active_providers;

  const segments = [];
  if (h.configured_providers > 0) {
    segments.push(
      `<span class="stat-providers" tabindex="0">${h.active_providers}/${denom} providers</span>`
    );
  }
  if (h.latency_sampled) {
    segments.push(
      `<span class="stat-tip" data-tip="95th percentile response time: 95% of recent requests finished faster than this.">p95 ${fmtLatency(
        h.p95_ms
      )}</span>`
    );
  }
  if (h.cache_active) {
    const saved =
      h.cache_cost_saved > 0
        ? `, saved $${Math.round(h.cache_cost_saved).toLocaleString()}`
        : "";
    segments.push(
      `<span class="stat-tip" data-tip="Prompt cache rate: share of requests served with cache control${saved}.">cache ${Math.round(
        h.cache_hit_rate * 100
      )}%</span>`
    );
  }

  if (!segments.length && h.breakers_open === 0) {
    el.innerHTML = "";
    return;
  }

  // Only real failures (open circuit breakers) tint the line. Providers that are
  // simply toggled off are a deliberate config choice, not degradation.
  const degraded = h.breakers_open > 0;

  const rows = providerPanelRows(providers, showInactiveProviders);
  const panel = rows
    ? `<div class="prov-panel" role="tooltip">${rows}</div>`
    : "";

  const line = `<div class="statusband-line${
    degraded ? " degraded" : ""
  }">${segments.join(" · ")}${panel}</div>`;
  const warn =
    h.breakers_open > 0
      ? `<div class="statusband-warn">⚠ ${h.breakers_open} breaker${
          h.breakers_open === 1 ? "" : "s"
        } open</div>`
      : "";
  el.innerHTML = line + warn;
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

let lastHeight = 0;

function fitWindow() {
  const app = document.getElementById("app");
  if (!app) return;
  const height = Math.min(620, app.offsetHeight + 16);
  if (height === lastHeight) return;
  lastHeight = height;
  getCurrentWindow()
    .setSize(new LogicalSize(332, height))
    .catch(() => {});
}

getCurrentWindow().listen("run-doctor", runDoctor);

const gearBtn = document.getElementById("gear-btn");
if (gearBtn) {
  gearBtn.innerHTML = GEAR_ICON;
  gearBtn.addEventListener("click", toggleSettings);
}

const refreshBtn = document.getElementById("refresh-btn");
if (refreshBtn) {
  refreshBtn.innerHTML = REFRESH_ICON;
  refreshBtn.addEventListener("click", async () => {
    if (refreshBtn.classList.contains("spinning")) return;
    refreshBtn.classList.add("spinning");
    refreshBtn.disabled = true;
    try {
      await refresh();
    } finally {
      refreshBtn.classList.remove("spinning");
      refreshBtn.disabled = false;
    }
  });
}

const helpBtn = document.getElementById("help-btn");
if (helpBtn) {
  helpBtn.innerHTML = GITHUB_ICON;
  helpBtn.addEventListener("click", () => {
    invoke("open_url", {
      url: "https://github.com/zoispag/omniroute-tray",
    }).catch(() => {});
  });
}

async function tick() {
  await refresh();
  await renderVersion();
  fitWindow();
}

async function loop() {
  await tick();
  setTimeout(loop, 5000);
}

const appEl = document.getElementById("app");
if (appEl && "ResizeObserver" in window) {
  new ResizeObserver(() => fitWindow()).observe(appEl);
}

loop();
