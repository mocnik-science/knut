// knut web: Vue 3 application (Composition API, templates compiled in the browser).
import { createEditor } from "./editor.js";

const { createApp, ref, reactive, computed, watch, nextTick, onMounted, markRaw } = Vue;

const COLORS = ["#2f6fdd", "#e8590c", "#2b8a3e", "#c2255c", "#7048e8", "#0c8599", "#e67700", "#5c940d"];
const POLL_MS = 150;
const CHECK_DELAY_MS = 400;

function store(key, value) {
  try { localStorage.setItem("knut-web:" + key, JSON.stringify(value)); } catch (_) { /* private mode */ }
}
function load(key, fallback) {
  try { const v = localStorage.getItem("knut-web:" + key); return v === null ? fallback : JSON.parse(v); } catch (_) { return fallback; }
}
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function fmtSeconds(s) {
  if (s < 1) return Math.round(s * 1000) + " ms";
  if (s < 60) return s.toFixed(1) + " s";
  return Math.floor(s / 60) + " min " + Math.round(s % 60) + " s";
}
function download(name, text, type) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement("a");
  a.href = url; a.download = name; a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// ----- icons (inline SVG, drawn with the text colour) ------------------------------------

const svg = path => `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${path}</svg>`;
const ICONS = {
  // the usual full-screen symbols: corners pointing out, corners pointing in
  enlarge: svg('<path d="M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5"/>'),
  shrink: svg('<path d="M9 4v5H4M20 9h-5V4M15 20v-5h5M4 15h5v5"/>'),
  left: svg('<path d="M15 5l-7 7 7 7"/>'),
  sun: svg('<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>'),
  moon: svg('<path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z"/>'),
  auto: svg('<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>'),
  right: svg('<path d="M9 5l7 7-7 7"/>'),
};

// ----- theme: light, dark or as the system ----------------------------------------------

const theme = ref(load("theme", "auto"));
function applyTheme() {
  if (theme.value === "auto") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", theme.value);
}
applyTheme();
watch(theme, v => { store("theme", v); applyTheme(); });

// ----- panel: slides out to a small strip, or fills the window --------------------------

const Panel = {
  props: { side: String, label: String },
  setup(props) {
    const el = ref(null);
    const collapsed = ref(load(props.side + ":collapsed", false));
    const max = ref(false);
    watch(collapsed, v => store(props.side + ":collapsed", v));
    // how much of the map the panel covers at its side: the map controls sit next to it
    function publishInset() {
      if (!el.value) return;
      const r = el.value.getBoundingClientRect();
      const w = window.innerWidth;
      const inset = max.value ? 0 : props.side === "left" ? Math.max(0, r.right) : Math.max(0, w - r.left);
      document.documentElement.style.setProperty("--inset-" + props.side, inset + "px");
    }
    onMounted(() => {
      publishInset();
      new ResizeObserver(publishInset).observe(el.value);
      window.addEventListener("resize", publishInset);
      // after the slide animation
      el.value.addEventListener("transitionend", publishInset);
    });
    watch([collapsed, max], () => nextTick(publishInset));
    const toggleMax = () => { max.value = !max.value; if (max.value) collapsed.value = false; };
    const toggleCollapse = () => { collapsed.value = !collapsed.value; if (collapsed.value) max.value = false; };
    const outward = props.side === "left" ? ICONS.left : ICONS.right;
    const inward = props.side === "left" ? ICONS.right : ICONS.left;
    return { el, collapsed, max, toggleMax, toggleCollapse, outward, inward, ICONS };
  },
  template: `
    <aside ref="el" :class="['panel', side, { collapsed, max }]" :aria-label="label">
      <header>
        <slot name="header"></slot>
        <div class="tools">
          <button class="icon" :title="max ? 'Back to normal size' : 'Fill the whole window'" :aria-label="max ? 'Back to normal size' : 'Fill the whole window'"
            @click="toggleMax" v-html="max ? ICONS.shrink : ICONS.enlarge"></button>
          <button class="icon" title="Slide out of the way" aria-label="Slide out of the way" @click="toggleCollapse" v-html="outward"></button>
        </div>
      </header>
      <button class="handle" :title="'Show ' + label" :aria-label="'Show ' + label" @click="toggleCollapse" v-html="inward"></button>
      <div class="body"><slot></slot></div>
    </aside>`,
};

// ----- map ------------------------------------------------------------------------------

const OSM_STYLE = {
  version: 8,
  sources: {
    osm: {
      type: "raster",
      tiles: ["https://tile.openstreetmap.org/{z}/{x}/{y}.png"],
      tileSize: 256,
      maxzoom: 19,
      attribution: '© <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a> contributors',
    },
  },
  layers: [{ id: "osm", type: "raster", source: "osm" }],
};

function extend(b, c) {
  if (typeof c[0] === "number") {
    b[0] = Math.min(b[0], c[0]); b[1] = Math.min(b[1], c[1]); b[2] = Math.max(b[2], c[0]); b[3] = Math.max(b[3], c[1]);
  } else {
    for (const x of c) extend(b, x);
  }
}
function featureBox(f) {
  const b = [Infinity, Infinity, -Infinity, -Infinity];
  const g = f.geometry;
  if (!g) return null;
  if (g.type === "GeometryCollection") g.geometries.forEach(x => extend(b, x.coordinates));
  else extend(b, g.coordinates);
  return b[0] <= b[2] ? b : null;
}
// The box to show: all features, but for many features only where most of them lie
// (history extracts contain objects that once lay far away).
function bboxOf(fc) {
  const boxes = fc.features.map(featureBox).filter(Boolean);
  if (!boxes.length) return null;
  if (boxes.length < 20) {
    const b = [Infinity, Infinity, -Infinity, -Infinity];
    boxes.forEach(x => extend(b, [[x[0], x[1]], [x[2], x[3]]]));
    return b;
  }
  const q = (v, p) => v[Math.round((v.length - 1) * p)];
  const xs = boxes.map(x => (x[0] + x[2]) / 2).sort((a, b) => a - b);
  const ys = boxes.map(x => (x[1] + x[3]) / 2).sort((a, b) => a - b);
  const b = [q(xs, 0.02), q(ys, 0.02), q(xs, 0.98), q(ys, 0.98)];
  // keep the extent of the features inside that box (polygons reach beyond their centres)
  for (const x of boxes) {
    const cx = (x[0] + x[2]) / 2, cy = (x[1] + x[3]) / 2;
    if (cx >= b[0] && cx <= b[2] && cy >= b[1] && cy <= b[3]) extend(b, [[x[0], x[1]], [x[2], x[3]]]);
  }
  return b;
}
function popupHtml(p) {
  const rows = Object.entries(p).filter(([k]) => !["@label", "@kind"].includes(k))
    .map(([k, v]) => `<tr><td>${escapeHtml(k)}</td><td>${escapeHtml(typeof v === "object" ? JSON.stringify(v) : v)}</td></tr>`).join("");
  return `<div class="popup"><h4>${escapeHtml(p["@label"] || "")}</h4><table>${rows}</table></div>`;
}

const MapView = {
  props: { info: Object },
  setup(props, { expose }) {
    const el = ref(null);
    let map = null;
    let ready = null;
    let layers = [];

    onMounted(() => {
      map = markRaw(new maplibregl.Map({
        container: el.value,
        style: (props.info && props.info.style) || OSM_STYLE,
        center: [10, 51], zoom: 4, attributionControl: { compact: true },
      }));
      map.addControl(new maplibregl.NavigationControl(), "bottom-right");
      map.addControl(new maplibregl.ScaleControl(), "bottom-left");
      ready = new Promise(r => map.on("load", r));
    });

    // room for the panels that cover the map
    function padding() {
      const w = el.value.clientWidth, h = el.value.clientHeight;
      const p = { top: 40, bottom: 40, left: 40, right: 40 };
      const l = document.querySelector(".panel.left:not(.collapsed):not(.max)");
      const r = document.querySelector(".panel.right:not(.collapsed):not(.max)");
      if (l) p.left = l.getBoundingClientRect().right + 30;
      if (r) p.right = w - r.getBoundingClientRect().left + 30;
      // MapLibre does not move at all if the padding leaves no room: keep at least
      // 160 × 120 px of map, shrinking the padding in proportion
      const fit = (a, b, total, min) => {
        const room = total - min;
        if (a + b <= room) return [a, b];
        const k = Math.max(room, 0) / (a + b);
        return [a * k, b * k];
      };
      [p.left, p.right] = fit(p.left, p.right, w, 160);
      [p.top, p.bottom] = fit(p.top, p.bottom, h, 120);
      return p;
    }

    async function fitBox(b, animate) {
      if (!b || !map) return;
      await ready;
      map.fitBounds([[b[0], b[1]], [b[2], b[3]]], { padding: padding(), maxZoom: 17, animate });
    }

    async function clear() {
      if (!map) return;
      await ready;
      for (const id of layers) if (map.getLayer(id)) map.removeLayer(id);
      for (const id of new Set(layers.map(l => l.split(":")[0]))) if (map.getSource(id)) map.removeSource(id);
      layers = [];
    }

    async function show(results) {
      await clear();
      const all = [Infinity, Infinity, -Infinity, -Infinity];
      results.forEach((r, i) => {
        if (!r.geojson) return;
        const src = "out" + i;
        const color = COLORS[i % COLORS.length];
        map.addSource(src, { type: "geojson", data: r.geojson });
        // groups with values: shade by value
        const vals = r.geojson.features.map(f => f.properties["@value"]).filter(v => typeof v === "number");
        const lo = Math.min(...vals), hi = Math.max(...vals);
        const fillColor = vals.length > 1 && hi > lo
          ? ["interpolate", ["linear"], ["to-number", ["get", "@value"], lo], lo, "#fff5eb", hi, color]
          : color;
        const fillOpacity = vals.length > 1 ? 0.65 : 0.25;
        const poly = ["match", ["geometry-type"], ["Polygon", "MultiPolygon"], true, false];
        const line = ["match", ["geometry-type"], ["LineString", "MultiLineString"], true, false];
        const point = ["match", ["geometry-type"], ["Point", "MultiPoint"], true, false];
        map.addLayer({ id: src + ":fill", type: "fill", source: src, filter: poly, paint: { "fill-color": fillColor, "fill-opacity": fillOpacity } });
        map.addLayer({ id: src + ":outline", type: "line", source: src, filter: poly, paint: { "line-color": color, "line-width": 1 } });
        map.addLayer({ id: src + ":line", type: "line", source: src, filter: line, paint: { "line-color": color, "line-width": 3 } });
        map.addLayer({
          id: src + ":point", type: "circle", source: src, filter: point,
          paint: { "circle-color": color, "circle-radius": ["interpolate", ["linear"], ["zoom"], 8, 2.5, 16, 6], "circle-stroke-color": "#fff", "circle-stroke-width": 1 },
        });
        for (const kind of ["fill", "line", "point"]) {
          const id = src + ":" + kind;
          map.on("click", id, e => new maplibregl.Popup({ maxWidth: "360px" }).setLngLat(e.lngLat).setHTML(popupHtml(e.features[0].properties)).addTo(map));
          map.on("mouseenter", id, () => { map.getCanvas().style.cursor = "pointer"; });
          map.on("mouseleave", id, () => { map.getCanvas().style.cursor = ""; });
        }
        layers.push(src + ":fill", src + ":outline", src + ":line", src + ":point");
        const b = bboxOf(r.geojson);
        if (b) extend(all, [[b[0], b[1]], [b[2], b[3]]]);
      });
      if (all[0] <= all[2]) fitBox(all, true);
    }

    function setVisible(i, on) {
      for (const id of layers) if (id.startsWith("out" + i + ":")) map.setLayoutProperty(id, "visibility", on ? "visible" : "none");
    }

    expose({ show, clear, fitBox, setVisible });
    return { el };
  },
  template: `<div id="map" ref="el"></div>`,
};

// ----- application ----------------------------------------------------------------------

const App = {
  components: { Panel, MapView },
  setup() {
    const map = ref(null);
    const editorHost = ref(null);
    let editor = null;
    const state = reactive({
      tab: "query",
      info: null,
      query: load("query", ""),
      db: load("db", ""),
      running: false,
      cancelling: false,
      job: null,
      progress: null,
      elapsed: 0,
      messages: "",
      problems: [],
      status: "",
      statusKind: "",
      results: [],
      visible: [],
      examples: [],
      filter: "",
      docs: "",
    });
    watch(() => state.query, v => store("query", v));
    watch(() => state.db, v => store("db", v));

    onMounted(async () => {
      const [info, examples, docs] = await Promise.all([
        fetch("api/info").then(r => r.json()).catch(() => ({})),
        fetch("examples.json").then(r => r.json()).catch(() => []),
        fetch("docs.html").then(r => r.text()).catch(() => ""),
      ]);
      state.info = info;
      state.examples = examples;
      state.docs = docs;
      if (info.bbox) map.value.fitBox(info.bbox, false);
    });

    // the editor stays alive while other tabs are shown (v-show), keeping its history
    function mountEditor() {
      if (!editorHost.value) return;
      editor = createEditor(editorHost.value, {
        doc: state.query,
        hint: "count of nodes n with n:amenity=bench",
        onChange: v => { state.query = v; },
        onRun: () => run(),
        onEscape: () => { if (state.running) { cancel(); return true; } return false; },
      });
      if (state.problems.length) editor.setProblems(state.problems);
    }
    onMounted(() => nextTick(mountEditor));

    // check while typing (parse and check only, no run)
    let checkTimer = null;
    let checkSeq = 0;
    async function check() {
      const seq = ++checkSeq;
      try {
        const r = await fetch("api/check", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ script: state.query, db: state.db }) });
        const { problems } = await r.json();
        if (seq !== checkSeq || state.running) return;
        state.problems = problems;
        if (editor) editor.setProblems(problems);
      } catch (_) { /* the server may be busy; the run reports everything anyway */ }
    }
    watch(() => [state.query, state.db], () => {
      clearTimeout(checkTimer);
      checkTimer = setTimeout(check, CHECK_DELAY_MS);
    });
    onMounted(() => { if (state.query.trim()) check(); });

    function goToLine(line) {
      state.tab = "query";
      nextTick(() => editor && editor.goTo(line));
    }

    // messages as lines with highlighted headers and links to the lines ----------------
    const messageLines = computed(() => state.messages.replace(/\s+$/, "").split("\n").map(text => {
      const link = text.match(/-->\s+[^:]*:(\d+):(\d+)/) || text.match(/^\s*\[\d+\] (?:line|Zeile) (\d+):/);
      let cls = "";
      if (/^(Error|Fehler) \[/.test(text)) cls = "error";
      else if (/^(Warning|Warnung) \[/.test(text)) cls = "warning";
      return { text, cls, line: link ? +link[1] : null };
    }));

    // running -------------------------------------------------------------------------
    let pollTimer = null;
    async function run() {
      if (state.running || !state.query.trim()) return;
      state.running = true;
      state.cancelling = false;
      state.progress = null;
      state.elapsed = 0;
      state.messages = "";
      state.status = "";
      try {
        const r = await fetch("api/run", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ script: state.query, db: state.db }) });
        const { job, error } = await r.json();
        if (!job) throw new Error(error || "the server did not start the run");
        state.job = job;
        poll();
      } catch (e) {
        finish();
        state.status = "Could not run: " + e.message;
        state.statusKind = "bad";
      }
    }
    async function poll() {
      try {
        const r = await fetch("api/job/" + state.job);
        const j = await r.json();
        state.elapsed = j.seconds || 0;
        if (j.state === "running") {
          state.progress = j.progress;
          state.cancelling = j.cancelling;
          pollTimer = setTimeout(poll, POLL_MS);
          return;
        }
        const res = j.result;
        state.messages = res.messages || "";
        state.problems = res.problems || [];
        if (editor) editor.setProblems(state.problems);
        if (j.state === "cancelled") {
          state.status = "Cancelled after " + fmtSeconds(j.seconds);
          state.statusKind = "bad";
        } else if (res.code === 0) {
          state.status = res.outputs.length + (res.outputs.length === 1 ? " result" : " results") + " in " + fmtSeconds(j.seconds);
          state.statusKind = "ok";
        } else {
          state.status = res.code === 1 ? "Problems found, nothing was run" : "Stopped by an error";
          state.statusKind = "bad";
        }
        state.results = res.outputs;
        state.visible = res.outputs.map(() => true);
        map.value.show(res.outputs);
        finish();
      } catch (e) {
        finish();
        state.status = "Lost the connection to knut web: " + e.message;
        state.statusKind = "bad";
      }
    }
    function finish() {
      clearTimeout(pollTimer);
      state.running = false;
      state.job = null;
    }
    async function cancel() {
      if (!state.job) return;
      state.cancelling = true;
      await fetch("api/job/" + state.job + "/cancel", { method: "POST" }).catch(() => {});
    }
    function reset() {
      if (state.running) cancel();
      if (editor) editor.setValue(""); else state.query = "";
      state.messages = "";
      state.problems = [];
      state.status = "";
      state.results = [];
      state.visible = [];
      map.value.clear();
    }

    const progressPct = computed(() => state.progress && state.progress.fraction != null ? Math.round(state.progress.fraction * 100) : null);
    const progressLabel = computed(() => {
      const p = state.progress;
      if (state.cancelling) return "Cancelling…";
      if (!p || !p.steps) return "Starting…";
      let s = p.steps > 1 ? `Output ${p.step} of ${p.steps} · ` : "";
      s += p.label || "";
      return s;
    });
    const progressRight = computed(() => {
      const p = state.progress;
      const blocks = p && p.blocks_total ? `${p.blocks_done}/${p.blocks_total} blocks · ` : "";
      return blocks + fmtSeconds(state.elapsed);
    });

    // examples ------------------------------------------------------------------------
    const filtered = computed(() => {
      const q = state.filter.trim().toLowerCase();
      if (!q) return state.examples;
      return state.examples.filter(e => (e.title + " " + e.description + " " + e.code + " " + e.category).toLowerCase().includes(q));
    });
    function useExample(e, andRun) {
      state.tab = "query";
      nextTick(() => {
        if (editor) editor.setValue(e.code); else state.query = e.code;
        state.query = e.code;
        if (andRun) run();
      });
    }

    // results -------------------------------------------------------------------------
    const summary = computed(() => {
      const n = state.results.length;
      if (!n) return "";
      const f = state.results.reduce((a, r) => a + (r.features || 0), 0);
      return `${n} · ${f.toLocaleString()} on the map`;
    });
    function toggleLayer(i) {
      state.visible[i] = !state.visible[i];
      map.value.setVisible(i, state.visible[i]);
    }
    function zoomTo(r) { map.value.fitBox(bboxOf(r.geojson), true); }
    function saveJson(r, i) { download(`result-${i + 1}.json`, JSON.stringify(r.value, null, 2), "application/json"); }
    function saveGeojson(r, i) { download(`result-${i + 1}.geojson`, JSON.stringify(r.geojson), "application/geo+json"); }
    function copyText(r) { navigator.clipboard && navigator.clipboard.writeText(r.text); }

    return {
      state, map, editorHost, goToLine, messageLines,
      run, cancel, reset, progressPct, progressLabel, progressRight, filtered, useExample,
      summary, toggleLayer, zoomTo, saveJson, saveGeojson, copyText, COLORS, fmtSeconds, theme,
      themes: [
        { id: "light", label: "Light", title: "Light colours", icon: ICONS.sun },
        { id: "dark", label: "Dark", title: "Dark colours", icon: ICONS.moon },
        { id: "auto", label: "Automatic", title: "As the system", icon: ICONS.auto },
      ],
    };
  },
  template: `
    <MapView ref="map" :info="state.info" />

    <Panel side="left" label="the query panel">
      <template #header>
        <nav class="tabs" role="tablist">
          <button v-for="t in ['query', 'examples', 'docs', 'settings', 'about']" :key="t" role="tab"
            :class="{ active: state.tab === t }" @click="state.tab = t">{{ t[0].toUpperCase() + t.slice(1) }}</button>
        </nav>
      </template>

      <div v-show="state.tab === 'query'" class="tabpane">
        <div class="cm-host" ref="editorHost"></div>
        <div class="actions">
          <button v-if="!state.running" class="primary" :disabled="!state.query.trim()" title="Run (⌘/Ctrl + Enter)" @click="run">Run</button>
          <button v-else class="danger" :disabled="state.cancelling" title="Cancel (Esc)" @click="cancel">Cancel</button>
          <button title="Clear the query, the results and the map" @click="reset">Reset</button>
          <span v-if="state.status && !state.running" :class="['status', state.statusKind]" :title="state.status">{{ state.status }}</span>
        </div>
        <div v-if="state.running" class="progress">
          <div :class="['bar', { busy: progressPct === null }]"><div class="fill" :style="{ width: (progressPct ?? 30) + '%' }"></div></div>
          <div class="label"><span>{{ progressLabel }}</span><span>{{ progressRight }}</span></div>
        </div>
        <div v-if="state.messages" class="messages">
          <pre><template v-for="(l, i) in messageLines" :key="i"><a v-if="l.line" :class="l.cls" @click="goToLine(l.line)">{{ l.text }}</a><span v-else :class="l.cls">{{ l.text }}</span>
</template></pre>
        </div>
      </div>

      <template v-if="state.tab === 'examples'">
        <input type="search" v-model="state.filter" placeholder="Filter the examples…" autocomplete="off">
        <div class="count">{{ filtered.length }} of {{ state.examples.length }} examples</div>
        <div v-for="(e, i) in filtered" :key="i" class="example">
          <h3>{{ e.title }}<span class="cat">{{ e.category }}</span></h3>
          <p>{{ e.description }}</p>
          <pre>{{ e.code }}</pre>
          <div class="buttons">
            <button class="small" @click="useExample(e, false)">Use</button>
            <button class="small primary" :disabled="state.running" @click="useExample(e, true)">Run</button>
          </div>
        </div>
      </template>

      <div v-if="state.tab === 'docs'" class="docs" v-html="state.docs"></div>

      <div v-if="state.tab === 'settings'" class="settings">
        <section>
          <h3>Database</h3>
          <input class="text" v-model="state.db" spellcheck="false" autocomplete="off"
            :placeholder="state.info && state.info.db ? state.info.db : 'path/to/file.knutdb'">
          <p class="help">Used for every query. If this is empty, a query uses its own <code>knut_db = '…'</code>,
            <template v-if="state.info && state.info.db">and otherwise <code>{{ state.info.db }}</code>, the database knut web was started with<template v-if="state.info.time"> (data from {{ state.info.time[0].slice(0, 10) }} to {{ state.info.time[1].slice(0, 10) }})</template>.</template>
            <template v-else>which it must then contain.</template>
            Relative paths start at the folder knut web was started in.</p>
        </section>
        <section>
          <h3>Appearance</h3>
          <div class="themes" role="radiogroup" aria-label="Appearance">
            <button v-for="t in themes" :key="t.id" role="radio" :aria-checked="theme === t.id"
              :class="{ active: theme === t.id }" :title="t.title" @click="theme = t.id">
              <span v-html="t.icon"></span>{{ t.label }}
            </button>
          </div>
          <p class="help">Automatic follows the setting of your system. The map always keeps its normal colours.</p>
        </section>
      </div>

      <div v-if="state.tab === 'about'" class="docs about">
        <h2>knut</h2>
        <p><em>knut</em> is short for <em>knutpunkt</em>, the Swedish word for ‘junction’ or ‘node’, one of the building blocks of OpenStreetMap data.</p>
        <p>It reads OpenStreetMap PBF files (with history) and converts them into its own compact, high-performance data format, which enables fast queries of OpenStreetMap data in Rust, in Python, on the command line, and in this web interface, using the <em>knut</em> query language.</p>
        <p><em>knut</em> has been created by Franz-Benjamin Mocnik, University of Salzburg, and is released open source.</p>
        <p>Copyright by Franz-Benjamin Mocnik, 2023–2026.</p>
        <p>Parts of this software have been supported by the <em>EXDIGIT&nbsp;(Excellence in Digital Sciences and Interdisciplinary Technologies)</em> project, funded by Land Salzburg under grant number 20204-WISS/263/6-6022.</p>
        <p class="version" v-if="state.info && state.info.version">Version {{ state.info.version }}</p>
      </div>
    </Panel>

    <Panel side="right" label="the results">
      <template #header>
        <h2>Results <span class="meta">{{ summary }}</span></h2>
      </template>
      <div v-if="!state.results.length" class="empty-note">Run a query to see its results here and on the map.</div>
      <details v-for="(r, i) in state.results" :key="i" class="result" open>
        <summary>
          <span class="swatch" :style="{ background: r.geojson ? COLORS[i % COLORS.length] : 'transparent' }"></span>
          <span class="expr" :title="r.expr">{{ r.expr }}</span>
          <span class="meta">line {{ r.line }} · {{ fmtSeconds(r.seconds) }}</span>
        </summary>
        <div class="rtools">
          <template v-if="r.geojson">
            <label><input type="checkbox" :checked="state.visible[i]" @change="toggleLayer(i)"> map</label>
            <span>{{ r.features.toLocaleString() }} features<template v-if="r.truncated"> (first ones only)</template></span>
            <button class="small" @click="zoomTo(r)">Zoom</button>
            <button class="small" @click="saveGeojson(r, i)">GeoJSON</button>
          </template>
          <button class="small" @click="saveJson(r, i)">JSON</button>
          <button class="small" @click="copyText(r)">Copy</button>
        </div>
        <pre>{{ r.text }}</pre>
      </details>
    </Panel>`,
};

createApp(App).mount("#app");
