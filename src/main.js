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
let costRange = localStorage.getItem("costRange") || "30d";
const COST_RANGES = [
  ["1d", "1D"],
  ["7d", "7D"],
  ["30d", "30D"],
  ["yesterday", "Yesterday"],
  ["today", "Today"],
];

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
    noteServerVersion(status.version);
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
      // One section failing (e.g. a 403 from an auth-gated endpoint) must not
      // take the others down with it, nor masquerade as a server error in the
      // header — that is reserved for the server itself being unreachable.
      const results = await Promise.allSettled(jobs);
      for (const r of results) {
        if (r.status === "rejected") console.warn("section failed:", r.reason);
      }
    } else {
      clearSections();
    }
  } catch (err) {
    renderHeader({ state: "error", reason: String(err) });
  }
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

async function toggleSettings() {
  inSettings = !inSettings;
  document.getElementById("gear-btn")?.classList.toggle("active", inSettings);
  if (inSettings) {
    if (!rateLimitCache.length) {
      try {
        rateLimitCache = await invoke("get_rate_limits");
        rateLimitsLoaded = true;
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

// Inverse of accountKey. Provider ids never contain "/", account names might.
function parseAccountKey(key) {
  const i = key.indexOf("/");
  return i < 0
    ? { provider: key, account: "" }
    : { provider: key.slice(0, i), account: key.slice(i + 1) };
}

// "qwen-cloud-token-plan" → "Qwen Cloud Token Plan".
function providerLabel(provider) {
  return provider
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((w) => w[0].toUpperCase() + w.slice(1))
    .join(" ");
}

// Several providers report the account as just "main", which says nothing once two
// of them do it (#52). Name the provider instead so each Usage row is self-describing.
const DEFAULT_ACCOUNT_NAMES = new Set(["main", "default"]);

function accountLabel(acc) {
  const name = String(acc.account ?? "").trim();
  const generic =
    !name ||
    DEFAULT_ACCOUNT_NAMES.has(name.toLowerCase()) ||
    name.toLowerCase() === String(acc.provider).toLowerCase();
  return generic ? providerLabel(acc.provider) : name;
}

// Accounts the settings page lists: every connection OmniRoute knows about — idle
// and inactive ones included, which is why the backend stopped dropping them (#57) —
// plus hidden keys it no longer reports at all (deleted upstream). Without both, a
// hidden account that stops reporting usage could never be re-enabled (#52).
function settingsAccounts() {
  const seen = new Set(rateLimitCache.map(accountKey));
  const orphans = [...hiddenAccounts]
    .filter((key) => !seen.has(key))
    .map((key) => ({
      ...parseAccountKey(key),
      windows: [],
      active: false,
      missing: true,
    }));
  return [...rateLimitCache, ...orphans];
}

// Why a settings row is not in the Usage list. Every case stays listed and tickable:
// the hidden state must be reversible even when the account reports nothing (#57).
function accountStatus(acc) {
  if (acc.missing) {
    return {
      text: "not reported",
      tip: "OmniRoute no longer lists this account. Tick it to drop the hidden state.",
    };
  }
  if (acc.active === false) {
    return {
      text: "inactive",
      tip: "Disabled in OmniRoute, so it reports no usage.",
    };
  }
  if (!acc.windows || !acc.windows.length) {
    return {
      text: "no usage",
      tip: "OmniRoute reports no usage window for this account yet.",
    };
  }
  return null;
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
// The skeleton stands for "not fetched yet". Once a fetch has landed, an empty list
// is an answer — OmniRoute has no connections — and must not keep faking a load.
let rateLimitsLoaded = false;

// ---- provider marks ----
// Beyond the few marks bundled in icons.js, the local OmniRoute dashboard serves
// one per provider at /providers/<id>.svg. The webview CSP forbids loading them
// directly, so the Rust side fetches them and we inline the (normalised) SVG.
// `null` records that the server has none — the lettered badge stays and we stop
// asking for this session. A failed request (server not up yet) is not recorded,
// so the next paint retries.
const remoteIcons = new Map();
const pendingIcons = new Set();

// Marks belong to the OmniRoute build that served them, and `remoteIcons` lives for
// the whole session — so an update would keep painting the old build's marks until
// the tray restarted. Every version change drops them and bumps a generation, which
// also discards answers from a request that was in flight across the swap.
let servedVersion;
let iconGeneration = 0;

function noteServerVersion(version) {
  // Ignore "unknown" (server down / restarting): only a real version-to-version
  // move means different assets. A restart on the same build serves the same files.
  if (!version) return;
  if (servedVersion && servedVersion !== version) {
    remoteIcons.clear();
    pendingIcons.clear();
    iconGeneration += 1;
  }
  servedVersion = version;
}

function providerBadge(provider) {
  const icon = PROVIDER_ICONS[provider] ?? remoteIcons.get(provider);
  const attrs = `class="prov-badge" data-provider="${escapeHtml(provider)}" title="${escapeHtml(provider)}"`;
  if (icon) return `<span ${attrs}>${icon}</span>`;
  if (!remoteIcons.has(provider)) requestProviderIcon(provider);
  return letterBadge(provider, attrs);
}

// Distinguishable stand-in for providers without a mark: first letter on a hue
// derived from the id, so two unknown providers no longer look identical.
function letterBadge(provider, attrs) {
  const letter = (provider.match(/[a-z0-9]/i) || ["?"])[0].toUpperCase();
  let hash = 0;
  for (const ch of provider) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return `<span ${attrs.replace('class="prov-badge"', 'class="prov-badge prov-letter"')} style="--hue:${hash % 360}">${letter}</span>`;
}

function requestProviderIcon(provider) {
  if (pendingIcons.has(provider)) return;
  pendingIcons.add(provider);
  const generation = iconGeneration;
  invoke("get_provider_icon", { provider })
    .then((svg) => {
      if (generation !== iconGeneration) return; // answer from the previous server
      const clean = svg ? normalizeSvg(svg, provider) : null;
      remoteIcons.set(provider, clean);
      if (clean) {
        const html = providerBadge(provider);
        document
          .querySelectorAll(`.prov-badge[data-provider="${CSS.escape(provider)}"]`)
          .forEach((el) => (el.outerHTML = html));
      }
    })
    .catch(() => {})
    .finally(() => {
      // A newer generation owns the entry now; deleting it would let a duplicate
      // request start for a provider already being fetched against the new server.
      if (generation === iconGeneration) pendingIcons.delete(provider);
    });
}

const GRAY_TOLERANCE = 24;
const MAX_MARK_ASPECT = 2;
const NAMED_GRAYS = new Set(["white", "black", "gray", "grey", "silver", "gainsboro", "whitesmoke"]);

// Parse a CSS colour into [r,g,b], or null for anything we don't recognise.
function parseColor(raw) {
  const v = raw.trim().toLowerCase();
  if (NAMED_GRAYS.has(v)) return [0, 0, 0];
  let m = v.match(/^#([0-9a-f]{3,4})$/);
  if (m) return [...m[1].slice(0, 3)].map((c) => parseInt(c + c, 16));
  m = v.match(/^#([0-9a-f]{6})(?:[0-9a-f]{2})?$/);
  if (m) return [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16));
  m = v.match(/^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/);
  if (m) return [m[1], m[2], m[3]].map(Number);
  return null;
}

function isGrayish(raw) {
  const rgb = parseColor(raw);
  return !!rgb && Math.max(...rgb) - Math.min(...rgb) <= GRAY_TOLERANCE;
}

const PAINT_PROPS = ["fill", "stroke", "stop-color"];
const NON_COLORS = /^(none|currentcolor|inherit|transparent|url\(.*)$/i;

function paintColors(root) {
  const colors = [];
  const consider = (v) => {
    if (v && !NON_COLORS.test(v.trim())) colors.push(v.trim());
  };
  for (const el of [root, ...root.querySelectorAll("*")]) {
    for (const p of PAINT_PROPS) consider(el.getAttribute(p));
    const style = el.getAttribute("style") || "";
    for (const m of style.matchAll(/(?:fill|stroke|stop-color)\s*:\s*([^;]+)/gi)) consider(m[1]);
  }
  for (const st of root.querySelectorAll("style")) {
    for (const m of (st.textContent || "").matchAll(/(?:fill|stroke|stop-color)\s*:\s*([^;}]+)/gi)) consider(m[1]);
  }
  return colors;
}

const escapeRegExp = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

// Inlined SVGs share the page's id space and their <style> rules apply document-wide.
// Marks ship generic ids/classes (".st0", ".cls-1", gradient ids), so prefix every id
// and pin every style rule under the root, or two providers' marks recolour each other.
function scopeSvg(root, prefix) {
  root.setAttribute("id", prefix);
  const all = [root, ...root.querySelectorAll("*")];
  const renames = all
    .filter((el) => el !== root && el.getAttribute("id"))
    .map((el) => [el.getAttribute("id"), `${prefix}-${el.getAttribute("id")}`])
    .sort((a, b) => b[0].length - a[0].length);
  const rewriteRefs = (text) =>
    renames.reduce(
      (acc, [from, to]) => acc.replace(new RegExp(`#${escapeRegExp(from)}(?![\\w-])`, "g"), `#${to}`),
      text
    );
  for (const el of all) {
    for (const attr of [...el.attributes]) {
      if (attr.name !== "id" && attr.value.includes("#")) el.setAttribute(attr.name, rewriteRefs(attr.value));
    }
  }
  for (const [from, to] of renames) root.querySelector(`[id="${from}"]`)?.setAttribute("id", to);
  for (const st of root.querySelectorAll("style")) {
    const css = rewriteRefs((st.textContent || "").replace(/\/\*[\s\S]*?\*\//g, ""));
    st.textContent = css.replace(/([^{}]+)\{/g, (_, sel) =>
      sel
        .split(",")
        .map((s) => `#${prefix} ${s.trim()}`)
        .join(", ") + "{"
    );
  }
}

const PRESENTATION_PROPS = new Set([
  "fill",
  "fill-rule",
  "fill-opacity",
  "stroke",
  "stroke-width",
  "stroke-linecap",
  "stroke-linejoin",
  "stroke-opacity",
  "opacity",
  "color",
]);

// Keep only the declarations that affect how the mark is painted.
function presentationStyle(style) {
  if (!style) return "";
  return style
    .split(";")
    .map((d) => d.trim())
    .filter((d) => PRESENTATION_PROPS.has(d.split(":")[0].trim().toLowerCase()))
    .join("; ");
}

// The server on :20128 is adopted, not necessarily ours, so a fetched mark is
// untrusted input. Only inert drawing primitives survive, with an attribute
// allowlist; anything that navigates, loads, animates or scripts is dropped.
const SVG_ELEMENTS = new Set([
  "svg",
  "g",
  "path",
  "circle",
  "ellipse",
  "rect",
  "line",
  "polyline",
  "polygon",
  "defs",
  "lineargradient",
  "radialgradient",
  "stop",
  "clippath",
  "mask",
  "use",
  "symbol",
  "style",
  // Inert filter primitives (blur/glow, as in Gemini's mark). feImage is NOT here:
  // it loads a URL.
  "filter",
  "fegaussianblur",
  "feflood",
  "feblend",
  "fecolormatrix",
  "feoffset",
  "fecomposite",
  "femerge",
  "femergenode",
]);
const SVG_ATTRS = new Set([
  "id",
  "class",
  "style",
  "d",
  "x",
  "y",
  "x1",
  "y1",
  "x2",
  "y2",
  "cx",
  "cy",
  "r",
  "rx",
  "ry",
  "fx",
  "fy",
  "fr",
  "width",
  "height",
  "viewbox",
  "points",
  "transform",
  "fill",
  "fill-rule",
  "fill-opacity",
  "stroke",
  "stroke-width",
  "stroke-linecap",
  "stroke-linejoin",
  "stroke-miterlimit",
  "stroke-dasharray",
  "stroke-dashoffset",
  "stroke-opacity",
  "opacity",
  "color",
  "clip-path",
  "clip-rule",
  "mask",
  "gradientunits",
  "gradienttransform",
  "spreadmethod",
  "offset",
  "stop-color",
  "stop-opacity",
  "clippathunits",
  "maskunits",
  "maskcontentunits",
  "preserveaspectratio",
  "xmlns",
  "xmlns:xlink",
  "version",
  "filter",
  "filterunits",
  "primitiveunits",
  "color-interpolation-filters",
  "stddeviation",
  "flood-color",
  "flood-opacity",
  "in",
  "in2",
  "mode",
  "operator",
  "result",
  "type",
  "values",
  "dx",
  "dy",
]);
const STYLE_PROPS = new Set([
  ...PRESENTATION_PROPS,
  "stop-color",
  "stop-opacity",
  "clip-rule",
  "mask-type",
]);

// Keep only allowlisted declarations, dropping anything that could load or lay out.
function sanitizeDeclarations(css) {
  return css
    .split(";")
    .map((d) => d.trim())
    .filter((d) => {
      const [prop, value] = d.split(/:(.*)/s);
      return (
        d && STYLE_PROPS.has(prop.trim().toLowerCase()) && !/url\((?!\s*['"]?#)/i.test(value || "")
      );
    })
    .join("; ");
}

// Rebuild a <style> sheet from its rules, keeping only allowlisted declarations.
function sanitizeStyleSheet(css) {
  const rules = [];
  for (const m of css.replace(/\/\*[\s\S]*?\*\//g, "").matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const decls = sanitizeDeclarations(m[2]);
    if (decls) rules.push(`${m[1].trim()}{${decls}}`);
  }
  return rules.join("\n");
}

function sanitizeSvg(root) {
  for (const el of [...root.querySelectorAll("*")]) {
    if (!SVG_ELEMENTS.has(el.localName.toLowerCase())) {
      // Unwrap containers we don't know (e.g. <a>, <switch>) so the drawing inside
      // survives; anything else (script, image, animate…, foreignObject) goes.
      if (["a", "switch"].includes(el.localName.toLowerCase())) el.replaceWith(...el.childNodes);
      else el.remove();
    }
  }
  for (const el of [root, ...root.querySelectorAll("*")]) {
    for (const attr of [...el.attributes]) {
      const n = attr.name.toLowerCase();
      if (n === "href" || n === "xlink:href") {
        // Only local fragment references (gradients, <use> of a <symbol>).
        if (!/^\s*#[\w-]+\s*$/.test(attr.value)) el.removeAttribute(attr.name);
        continue;
      }
      if (!SVG_ATTRS.has(n)) {
        el.removeAttribute(attr.name);
        continue;
      }
      if (n === "style") {
        const clean = sanitizeDeclarations(attr.value);
        if (clean) el.setAttribute("style", clean);
        else el.removeAttribute("style");
      } else if (/url\((?!\s*['"]?#)/i.test(attr.value)) {
        // fill="url(https://…)" and friends: external paint servers are not allowed.
        el.removeAttribute(attr.name);
      }
    }
  }
  for (const st of root.querySelectorAll("style")) {
    const clean = sanitizeStyleSheet(st.textContent || "");
    if (clean) st.textContent = clean;
    else st.remove();
  }
}

// Make a server-provided mark safe to inline and legible in both themes: reduce it
// to allowlisted inert drawing elements, fit it to the 16px badge, and turn
// monochrome marks (drawn for one particular background — white for dark UIs,
// black for light) into currentColor so they follow the text colour. Multi-colour
// brand marks are kept.
function normalizeSvg(text, provider) {
  let doc;
  try {
    doc = new DOMParser().parseFromString(text, "image/svg+xml");
  } catch {
    return null;
  }
  const root = doc.documentElement;
  if (!root || root.localName !== "svg" || doc.querySelector("parsererror")) return null;

  sanitizeSvg(root);
  scopeSvg(root, `pi-${String(provider).toLowerCase().replace(/[^a-z0-9]+/g, "-")}`);

  const colors = paintColors(root);
  if (colors.length === 0) {
    // No explicit paint anywhere: SVG's default is black, invisible in dark mode.
    root.setAttribute("fill", "currentColor");
  } else if (colors.every(isGrayish)) {
    const recolor = (css) => css.replace(/(fill|stroke|stop-color)\s*:\s*(?!none)[^;}]+/gi, "$1:currentColor");
    for (const el of [root, ...root.querySelectorAll("*")]) {
      for (const p of PAINT_PROPS) {
        const v = el.getAttribute(p);
        if (v && !NON_COLORS.test(v.trim())) el.setAttribute(p, "currentColor");
      }
      if (el.hasAttribute("style")) el.setAttribute("style", recolor(el.getAttribute("style")));
    }
    root.querySelectorAll("style").forEach((st) => (st.textContent = recolor(st.textContent || "")));
    if (!root.getAttribute("fill")) root.setAttribute("fill", "currentColor");
  }

  if (!root.getAttribute("viewBox")) {
    const w = parseFloat(root.getAttribute("width"));
    const h = parseFloat(root.getAttribute("height"));
    if (w > 0 && h > 0) root.setAttribute("viewBox", `0 0 ${w} ${h}`);
  }
  // A wordmark (e.g. 234×42) squeezed into a 16px square is an unreadable smear;
  // the letter badge says more. Only roughly square marks are worth inlining.
  const box = (root.getAttribute("viewBox") || "").trim().split(/[\s,]+/).map(Number);
  if (box.length === 4 && box[2] > 0 && box[3] > 0) {
    const ratio = box[2] / box[3];
    if (ratio > MAX_MARK_ASPECT || ratio < 1 / MAX_MARK_ASPECT) return null;
  }
  root.setAttribute("width", "16");
  root.setAttribute("height", "16");
  // lobehub marks carry layout styles (flex:none; line-height:1) that fight the
  // badge; paint properties on the root are part of the drawing and must stay.
  const rootStyle = presentationStyle(root.getAttribute("style"));
  if (rootStyle) root.setAttribute("style", rootStyle);
  else root.removeAttribute("style");
  root.setAttribute("class", "prov-icon");
  root.setAttribute("aria-hidden", "true");
  return new XMLSerializer().serializeToString(root);
}

function setAccountHidden(key, hidden) {
  if (hidden) hiddenAccounts.add(key);
  else hiddenAccounts.delete(key);
  saveHiddenAccounts();
}

function clearHiddenAccounts() {
  hiddenAccounts.clear();
  saveHiddenAccounts();
}

function saveHiddenAccounts() {
  localStorage.setItem("hiddenAccounts", JSON.stringify([...hiddenAccounts]));
}

async function renderRateLimits() {
  const section = document.getElementById("ratelimits");
  if (!rateLimitCache.length) {
    section.innerHTML = usageSkeleton();
  }
  try {
    const data = await invoke("get_rate_limits");
    if (Array.isArray(data)) {
      rateLimitCache = data;
      rateLimitsLoaded = true;
    }
  } catch (err) {
    if (!rateLimitCache.length) {
      // Nothing to fall back on: say why instead of spinning forever (#42).
      section.innerHTML = `<div class="section-head"><h3>Usage</h3></div>
        <p class="section-note">Usage unavailable: ${escapeHtml(err)}</p>`;
      return;
    }
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
    section.innerHTML = rateLimitsLoaded
      ? `<div class="section-head"><h3>Usage</h3></div><p class="placeholder">No accounts connected to OmniRoute.</p>`
      : usageSkeleton();
    return;
  }
  const toggle = `<button id="mode-toggle" class="mode-toggle">${
    showUsed ? "% used" : "% left"
  }</button>`;
  // The cache now carries every connection, idle ones included (#57); only those
  // with a usage window have anything to draw here.
  const reporting = rateLimitCache.filter((a) => a.windows && a.windows.length);
  const groups = groupByProvider(reporting)
    .map(([provider, accts]) => {
      const visible = accts.filter((a) => !hiddenAccounts.has(accountKey(a)));
      return [provider, visible];
    })
    .filter(([, visible]) => visible.length);

  if (!groups.length) {
    const note = reporting.length
      ? "All accounts hidden. Enable in settings."
      : "No account is reporting usage yet.";
    section.innerHTML = `<div class="section-head"><h3>Usage</h3>${toggle}</div><p class="placeholder">${note}</p>`;
    wireModeToggle();
    return;
  }

  const blocks = groups
    .map(([, accts]) => {
      const rows = accts
        .map((acc) => {
          const head = `<div class="account">${providerBadge(acc.provider)}<span class="acct-name" title="${escapeHtml(acc.account)}">${escapeHtml(accountLabel(acc))}</span></div>`;
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
  const requestedRange = costRange;
  const result = await invoke("get_cost", { range: requestedRange });
  if (requestedRange !== costRange) return; // a newer range was picked meanwhile
  costCache = result;
  paintCost();
}

function costSkeleton() {
  const row = `<div class="cost-row"><span class="skel skel-cost-label"></span><span class="skel skel-cost-value"></span></div>`;
  return `<div class="skel skel-cost-total"></div>${row}${row}${row}${row}`;
}

function paintCost() {
  const section = document.getElementById("cost");
  const result = costCache;
  if (!section || (result && result.status === "unavailable")) {
    if (section) section.innerHTML = "";
    return;
  }

  const rangeSelect = `<select id="cost-range" class="mode-select">${COST_RANGES.map(
    ([value, label]) =>
      `<option value="${value}"${value === costRange ? " selected" : ""}>${label}</option>`
  ).join("")}</select>`;

  // Always rendered (not gated on rows) so the range select doesn't shift
  // position as this button appears/disappears across loading/empty states.
  const toggle = `<button id="cost-toggle" class="mode-toggle">${
    costShowTokens ? "in/out" : "%"
  }</button>`;

  let body;
  if (!result) {
    body = costSkeleton();
  } else if (result.status === "needs-api-key") {
    body = `<a class="connect" href="http://127.0.0.1:20128" target="_blank">Connect API key →</a>`;
  } else if (!result.rows?.length) {
    body = `<div class="cost-empty">No spend in this range.</div>`;
  } else {
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
        const value = costShowTokens
          ? `${compactTokens(r.tokens_in ?? 0)} in · ${compactTokens(
              r.tokens_out ?? 0
            )} out`
          : `${(total > 0 ? ((r.cost ?? 0) / total) * 100 : 0).toFixed(1)}%`;
        return `<div class="cost-row"><span class="cost-model" data-tip="${r.model}">${r.model}</span><span class="cost-value">${value}</span></div>`;
      })
      .join("");
    body = `<div class="cost-total">$${total.toFixed(2)} · ${formatTokens(totalTokens)}</div>${top}`;
  }

  section.innerHTML = `
    <div class="section-head"><h3>Cost</h3><div class="cost-controls">${rangeSelect}${toggle}</div></div>
    ${body}`;

  const rangeSel = document.getElementById("cost-range");
  if (rangeSel) {
    rangeSel.onchange = () => {
      costRange = rangeSel.value;
      localStorage.setItem("costRange", costRange);
      costCache = null;
      paintCost();
      renderCost();
    };
  }

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
  const section = document.getElementById("trend");
  let t;
  try {
    t = await invoke("get_usage_trend");
  } catch (err) {
    // The trend is a nice-to-have; a rejected fetch just hides it.
    console.warn("usage trend unavailable:", err);
    section.innerHTML = "";
    return;
  }
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

  const groups = groupByProvider(settingsAccounts());
  const providers = groups.map(([p]) => p);
  const accountRows = groups
    .map(([provider, accts], i) => {
      const p = escapeHtml(provider);
      const acctList = accts
        .map((acc) => {
          const key = accountKey(acc);
          const checked = hiddenAccounts.has(key) ? "" : "checked";
          const status = accountStatus(acc);
          const hint = status
            ? `<span class="set-hint" title="${escapeHtml(status.tip)}">${escapeHtml(status.text)}</span>`
            : "";
          return `
            <label class="set-acct${status ? " set-quiet" : ""}">
              <input type="checkbox" class="set-check" data-key="${escapeHtml(key)}" ${checked} />
              <span class="acct-name">${escapeHtml(acc.account)}</span>${hint}
            </label>`;
        })
        .join("");
      return `
        <li class="set-row" data-provider="${p}">
          <span class="set-reorder">
            <button class="move-btn" data-provider="${p}" data-dir="up" ${i === 0 ? "disabled" : ""}>▲</button>
            <button class="move-btn" data-provider="${p}" data-dir="down" ${i === providers.length - 1 ? "disabled" : ""}>▼</button>
          </span>
          ${providerBadge(provider)}
          <span class="set-provider-name" title="${p}">${escapeHtml(providerLabel(provider))}</span>
          <span class="set-accts">${acctList}</span>
        </li>`;
    })
    .join("");
  const showAll = `<button id="show-all-btn" class="mode-toggle" ${hiddenAccounts.size ? "" : "hidden"}>Show all</button>`;

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
      <div class="section-head"><h3>Accounts</h3>${showAll}</div>
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
  const showAllBtn = document.getElementById("show-all-btn");
  if (showAllBtn) {
    showAllBtn.onclick = () => {
      clearHiddenAccounts();
      renderSettings();
    };
  }
  content.querySelectorAll(".set-check").forEach((c) => {
    c.onchange = () => {
      setAccountHidden(c.dataset.key, !c.checked);
      // No re-render: a row must not vanish under the pointer that just ticked it.
      // Re-enabling an account OmniRoute no longer reports has to look like it took
      // effect, even though nothing can come back in the Usage list (#57).
      if (showAllBtn) showAllBtn.hidden = hiddenAccounts.size === 0;
    };
  });
  content.querySelectorAll(".move-btn").forEach((b) => {
    b.onclick = () => moveProvider(b.dataset.provider, b.dataset.dir);
  });
}

function moveProvider(provider, dir) {
  // Same provider set the settings list was rendered from, orphans included.
  const providers = groupByProvider(settingsAccounts()).map(([p]) => p);
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
  // #app is capped at the viewport so long lists scroll instead of being clipped,
  // which also means its measured height can never ask for a taller window. The
  // height we want is the fixed chrome plus everything inside the scroller.
  const content = document.getElementById("content");
  const natural = content
    ? app.offsetHeight - content.clientHeight + content.scrollHeight
    : app.offsetHeight;
  const height = Math.min(620, Math.ceil(natural) + 16);
  if (height === lastHeight) return;
  lastHeight = height;
  getCurrentWindow()
    .setSize(new LogicalSize(332, height))
    .catch(() => {});
}

getCurrentWindow().listen("run-doctor", runDoctor);

getCurrentWindow().listen("quota-refreshed", (event) => {
  if (Array.isArray(event.payload)) rateLimitCache = event.payload;
  if (!inSettings) paintRateLimits();
});

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
