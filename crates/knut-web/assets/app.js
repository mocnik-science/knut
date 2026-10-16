// knut web: Vue 3 application (Composition API, templates compiled in the browser).
import { createEditor } from "./editor.js";

const { createApp, ref, reactive, computed, watch, nextTick, onMounted, onUnmounted, markRaw } = Vue;

// the animated logo, inlined so that it can be started: its animations run once, and only while
// the element around it has the class "play"; their names don't clash with the page's
function loaderMarkup(svg) {
  return svg
    .replace(/<metadata>[\s\S]*?<\/metadata>/, "")
    .replace(/ role="img" aria-label="[^"]*"/, ' aria-hidden="true"')
    .replace(/<style>([\s\S]*?)<\/style>/, (_, css) => "<style>" + css
      .replace(/\.(hop|dot|paint|swoosh)\b/g, ".knut-loader.play .$1")
      .replace(/@keyframes (\w+)/g, "@keyframes knut-$1")
      .replace(/animation: (\w+) ([^;]*?)infinite/g, "animation: knut-$1 $2 1") + "</style>");
}

// The colours of knut: magenta, cyan and orange, then colours that go with them. Maps (and the
// result list) use them in this order whatever the accent; the charts of the text interface lead
// with the accent.
const COLORS = ["#e64fa5", "#24c1e4", "#feaf3a", "#7dc07a", "#8d7bd8", "#f06b5b", "#2e7fa8", "#c9b43c"];
const POLL_MS = 150;
const CHECK_DELAY_MS = 400;

function store(key, value) {
  try { localStorage.setItem("knut-web:" + key, JSON.stringify(value)); } catch (_) { /* private mode */ }
}
function load(key, fallback) {
  try { const v = localStorage.getItem("knut-web:" + key); return v === null ? fallback : JSON.parse(v); } catch (_) { return fallback; }
}
/** Shakes an element briefly, e.g. the results panel when new results arrive. */
function nudge(selector) {
  const el = document.querySelector(selector);
  if (!el) return;
  el.classList.remove("nudge");
  void el.offsetWidth; // restarts the animation
  el.classList.add("nudge");
  el.addEventListener("animationend", () => el.classList.remove("nudge"), { once: true });
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

// ----- icons: Lucide (lucide.dev, ISC license, lucide-static 0.460.0), inline, drawn with the text colour

const svg = body => `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
const ICONS = {
  // the panel fills the window, and back
  enlarge: svg('<path d="M8 3H5a2 2 0 0 0-2 2v3"/><path d="M21 8V5a2 2 0 0 0-2-2h-3"/><path d="M3 16v3a2 2 0 0 0 2 2h3"/><path d="M16 21h3a2 2 0 0 0 2-2v-3"/>'),
  shrink: svg('<path d="M8 3v3a2 2 0 0 1-2 2H3"/><path d="M21 8h-3a2 2 0 0 1-2-2V3"/><path d="M3 16h3a2 2 0 0 1 2 2v3"/><path d="M16 21v-3a2 2 0 0 1 2-2h3"/>'),
  left: svg('<path d="m15 18-6-6 6-6"/>'),
  right: svg('<path d="m9 18 6-6-6-6"/>'),
  sun: svg('<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>'),
  moon: svg('<path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"/>'),
  auto: svg('<rect width="20" height="14" x="2" y="3" rx="2"/><line x1="8" x2="16" y1="21" y2="21"/><line x1="12" x2="12" y1="17" y2="21"/>'),
  settings: svg('<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>'),
  // a new conversation
  restart: svg('<path d="m16 22-1-4"/><path d="M19 13.99a1 1 0 0 0 1-1V12a2 2 0 0 0-2-2h-3a1 1 0 0 1-1-1V4a2 2 0 0 0-4 0v5a1 1 0 0 1-1 1H6a2 2 0 0 0-2 2v.99a1 1 0 0 0 1 1"/><path d="M5 14h14l1.973 6.767A1 1 0 0 1 20 22H4a1 1 0 0 1-.973-1.233z"/><path d="m8 22 1-4"/>'),
  // About
  bird: svg('<path d="M16 7h.01"/><path d="M3.4 18H12a8 8 0 0 0 8-8V7a4 4 0 0 0-7.28-2.3L2 20"/><path d="m20 7 2 .5-2 .5"/><path d="M10 18v3"/><path d="M14 17.75V21"/><path d="M7 18a6 6 0 0 0 3.84-10.61"/>'),
  // the text interface: questions in plain language
  chat: svg('<path d="M14 9a2 2 0 0 1-2 2H6l-4 4V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z"/><path d="M18 9h2a2 2 0 0 1 2 2v11l-4-4h-6a2 2 0 0 1-2-2v-1"/>'),
  // copy to the clipboard, and done
  copy: svg('<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>'),
  check: svg('<path d="M20 6 9 17l-5-5"/>'),
  // ask a question again
  retry: svg('<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>'),
  // edit a question
  pencil: svg('<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>'),
  // the advanced query editor
  rocket: svg('<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0"/><path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>'),
};

// ----- theme: light, dark or as the system ----------------------------------------------

const theme = ref(load("theme", "auto"));
function applyTheme() {
  if (theme.value === "auto") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", theme.value);
}
applyTheme();
watch(theme, v => { store("theme", v); applyTheme(); });
const THEMES = [
  { id: "light", label: "Light", title: "Light colours", icon: ICONS.sun },
  { id: "dark", label: "Dark", title: "Dark colours", icon: ICONS.moon },
  { id: "auto", label: "Automatic", title: "As the system", icon: ICONS.auto },
];

// ----- accent: the colour of the controls, chosen by name -------------------------------

const ACCENTS = [
  { id: "orange", label: "Orange", title: "Orange accent", color: "#feaf3a" },
  { id: "magenta", label: "Magenta", title: "Magenta accent", color: "#e64fa5" },
  { id: "cyan", label: "Cyan", title: "Cyan accent", color: "#24c1e4" },
];
const accent = ref(load("accent", "orange"));
const accentColor = () => (ACCENTS.find(a => a.id === accent.value) || ACCENTS[0]).color;
function applyAccent() {
  const c = accentColor(), style = document.documentElement.style;
  const [r, g, b] = [1, 3, 5].map(i => parseInt(c.slice(i, i + 2), 16));
  style.setProperty("--accent", c);
  style.setProperty("--hl-active", `rgba(${r}, ${g}, ${b}, 0.08)`);
  style.setProperty("--hl-selection", `rgba(${r}, ${g}, ${b}, 0.3)`);
}
applyAccent();
watch(accent, v => { store("accent", v); applyAccent(); });
// the charts: the accent first, then the other colours of knut
const chatColors = () => [accentColor(), ...COLORS.filter(c => c !== accentColor())];

// ----- a choice in the top bar: only the chosen one shows; under the mouse (on a touch screen,
// after a tap on it) the others fold out below it, in an order that stays until it folds up -----

const HOVER = window.matchMedia("(hover: hover)").matches;
const Pick = {
  props: { items: Array, modelValue: String, label: String },
  emits: ["update:modelValue"],
  setup(props, { emit }) {
    const el = ref(null);
    const open = ref(false);
    const order = ref([]);
    const chosenFirst = () => [...props.items.filter(t => t.id === props.modelValue), ...props.items.filter(t => t.id !== props.modelValue)];
    const shown = computed(() => open.value ? order.value : chosenFirst().slice(0, 1));
    function show() { if (!open.value) { order.value = chosenFirst(); open.value = true; } }
    const hide = () => { open.value = false; };
    function choose(t, i) {
      if (!open.value) return show();
      emit("update:modelValue", t.id);
      if (i === 0 && !HOVER) hide();
    }
    const outside = e => { if (open.value && el.value && !el.value.contains(e.target)) hide(); };
    onMounted(() => document.addEventListener("pointerdown", outside));
    onUnmounted(() => document.removeEventListener("pointerdown", outside));
    return { el, open, shown, choose, show, hide, HOVER };
  },
  template: `
    <div ref="el" :class="['pick', { open }]" role="group" :aria-label="label" @mouseenter="HOVER && show()" @mouseleave="HOVER && hide()">
      <div class="pick-list">
        <button v-for="(t, i) in shown" :key="t.id" :class="['icon', { active: t.id === modelValue }]" :title="t.title" :aria-label="t.title"
          :aria-pressed="t.id === modelValue" @click="choose(t, i)">
          <span v-if="t.color" class="swatch" :style="{ background: t.color }"></span>
          <span v-else class="glyph" v-html="t.icon"></span>
        </button>
      </div>
    </div>`,
};

// ----- about knut: in its tab of the editor, and in a box over the text interface ----------

const AboutText = {
  props: { info: Object },
  template: `
    <img class="logo" src="knut.svg" alt="knut">
    <p><em>knut</em> is short for <em>knutpunkt</em>, the Swedish word for ‘junction’ or ‘node’, one of the building blocks of OpenStreetMap data.</p>
    <p>It reads OpenStreetMap PBF files (with history) and converts them into its own compact, high-performance data format, which enables fast queries of OpenStreetMap data in Rust, in Python, on the command line, and in this web interface, using the <em>knut</em> query language.</p>
    <p>This application is written and maintained by Franz-Benjamin Mocnik, <a href="mailto:mail@mocnik-science.net">mail@mocnik-science.net</a>.</p>
    <p>(c) by Franz-Benjamin Mocnik, 2023–2026.</p>
    <p>Its parts written after September 2023 have been supported by the <em>EXDIGIT&nbsp;(Excellence in Digital Sciences and Interdisciplinary Technologies)</em> project, funded by Land Salzburg under grant number 20204-WISS/263/6-6022.</p>
    <p>The code is licensed under the <a href="https://github.com/mocnik-science/knut/blob/main/LICENSE" target="_blank" rel="noopener">MIT License</a>.</p>
    <p class="version" v-if="info && info.version">Version {{ info.version }}</p>`,
};
// the box over the whole page, the page dimmed and blurred behind it: gone with a click beside it, or with Esc
const AboutBox = {
  components: { AboutText },
  props: { info: Object, open: Boolean },
  emits: ["close"],
  setup(props, { emit }) {
    const esc = e => { if (e.key === "Escape" && props.open) emit("close"); };
    onMounted(() => window.addEventListener("keydown", esc));
    onUnmounted(() => window.removeEventListener("keydown", esc));
  },
  template: `
    <Transition name="fade">
      <div v-if="open" class="about-overlay" @click.self="$emit('close')">
        <div class="about-box docs about" role="dialog" aria-label="About knut"><AboutText :info="info" /></div>
      </div>
    </Transition>`,
};

// ----- panel: slides out to a small strip, or fills the window --------------------------

const Panel = {
  props: { side: String, label: String },
  setup(props) {
    const el = ref(null);
    const collapsed = ref(load(props.side + ":collapsed", false));
    const max = ref(false);
    watch(collapsed, v => store(props.side + ":collapsed", v));
    // width set by dragging the inner edge; null = the default from the stylesheet
    const width = ref(load(props.side + ":width", null));
    const dragging = ref(false);
    function startResize(e) {
      e.preventDefault();
      const handle = e.currentTarget;
      handle.setPointerCapture(e.pointerId);
      dragging.value = true;
      const r = el.value.getBoundingClientRect();
      const move = ev => {
        const w = props.side === "left" ? ev.clientX - r.left : r.right - ev.clientX;
        width.value = Math.round(Math.max(260, Math.min(w, window.innerWidth - 120)));
      };
      const up = () => {
        dragging.value = false;
        handle.removeEventListener("pointermove", move);
        handle.removeEventListener("pointerup", up);
        store(props.side + ":width", width.value);
      };
      handle.addEventListener("pointermove", move);
      handle.addEventListener("pointerup", up);
    }
    const resetWidth = () => { width.value = null; store(props.side + ":width", null); };
    // how much of the map the panel covers at its side: the map controls sit next to it
    function publishInset() {
      if (!el.value) return;
      const r = el.value.getBoundingClientRect();
      const w = window.innerWidth;
      // a collapsed panel always counts as its strip, also while it peeks out under the mouse
      const peek = parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--peek")) || 0;
      const inset = max.value ? 0 : collapsed.value ? peek : props.side === "left" ? Math.max(0, r.right) : Math.max(0, w - r.left);
      document.documentElement.style.setProperty("--inset-" + props.side, inset + "px");
      setTimeout(() => document.documentElement.classList.add("insets-ready"), 500);
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
    return { el, collapsed, max, toggleMax, toggleCollapse, outward, inward, ICONS, width, dragging, startResize, resetWidth };
  },
  template: `
    <aside ref="el" :class="['panel', side, { collapsed, max, dragging }]" :style="width ? { '--w': width + 'px' } : null" :aria-label="label">
      <div class="resizer" title="Drag to resize, double-click for the default width" @pointerdown="startResize" @dblclick="resetWidth"></div>
      <header>
        <slot name="header"></slot>
        <div class="tools">
          <slot name="tools"></slot>
          <button class="icon" :title="max ? 'Back to normal size' : 'Fill the whole window'" :aria-label="max ? 'Back to normal size' : 'Fill the whole window'"
            @click="toggleMax" v-html="max ? ICONS.shrink : ICONS.enlarge"></button>
          <button class="icon" title="Slide out of the way" aria-label="Slide out of the way" @click="toggleCollapse" v-html="outward"></button>
        </div>
      </header>
      <button class="handle" :title="'Show ' + label" :aria-label="'Show ' + label" @click="toggleCollapse" v-html="inward"></button>
      <!-- what stays at hand at the top of the strip while the panel is slid out of the way -->
      <div class="strip"><slot name="strip"></slot></div>
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

// The base map recedes behind the results: paler, with little colour.
const RECEDE = { "raster-saturation": -0.65, "raster-contrast": -0.2, "raster-brightness-min": 0.2 };

/// Draws results (with GeoJSON) on a map, in the colours of the result list; a click on a
/// feature shows its tags. Returns the layers and the box of all features.
function addResults(map, results, prefix, colors = COLORS) {
  const layers = [];
  const all = [Infinity, Infinity, -Infinity, -Infinity];
  results.forEach((r, i) => {
    if (!r.geojson) return;
    const src = prefix + i;
    const color = colors[i % colors.length];
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
  return { layers, box: all[0] <= all[2] ? all : null };
}

const MapView = {
  props: { info: Object },
  setup(props, { expose }) {
    const el = ref(null);
    let map = null;
    let ready = null;
    let layers = [];

    onMounted(() => {
      // the default map starts receded (no flash of full colour); other styles once loaded
      const osm = { ...OSM_STYLE, layers: [{ ...OSM_STYLE.layers[0], paint: RECEDE }] };
      map = markRaw(new maplibregl.Map({
        container: el.value,
        style: (props.info && props.info.style) || osm,
        center: [10, 51], zoom: 4, attributionControl: { compact: true },
      }));
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }), "bottom-right");
      // without a compass, the map stays north up
      map.dragRotate.disable();
      map.touchZoomRotate.disableRotation();
      map.keyboard.disableRotation();
      map.addControl(new maplibregl.ScaleControl(), "bottom-left");
      ready = new Promise(r => map.on("load", r));
      // the attribution starts as the (i) button; a click opens it
      const collapseAttribution = () => {
        const a = el.value.querySelector(".maplibregl-ctrl-attrib");
        if (a) { a.classList.remove("maplibregl-compact-show"); a.removeAttribute("open"); }
      };
      collapseAttribution();
      map.once("load", collapseAttribution);
      map.once("load", () => {
        for (const l of map.getStyle().layers) {
          if (l.type !== "raster" || layers.includes(l.id)) continue;
          for (const [k, v] of Object.entries(RECEDE)) map.setPaintProperty(l.id, k, v);
        }
      });
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
      const added = addResults(map, results, "out");
      layers = added.layers;
      if (added.box) extend(all, [[added.box[0], added.box[1]], [added.box[2], added.box[3]]]);
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

// ----- text interface: questions in plain language ------------------------------------
// A search field; a question goes to the language model, which writes a query (checked and
// corrected on the server), runs it and answers; or asks back. Each turn can be opened in
// the editor of the advanced page, with its results on the map.
// ----- answers of the text interface: a small map, a chart ------------------------------

/// The results with geometry on a small map in the answer. After a reload only the box is
/// known: the map shows it, darkened, and a click runs the query again.
const AnswerMap = {
  props: { results: Array, box: Array, stale: Boolean, loading: Boolean },
  emits: ["regenerate"],
  setup(props, { emit }) {
    const el = ref(null);
    let map = null;
    let ready = null;
    async function draw() {
      if (!map || !props.results || !props.results.length) return;
      await ready;
      const { box } = addResults(map, props.results, "a", COLORS);
      const b = box || props.box;
      if (b) map.fitBounds([[b[0], b[1]], [b[2], b[3]]], { padding: 24, maxZoom: 16, animate: false });
    }
    function create() {
      const osm = { ...OSM_STYLE, layers: [{ ...OSM_STYLE.layers[0], paint: RECEDE }] };
      const b = props.box;
      map = markRaw(new maplibregl.Map({
        container: el.value, style: osm,
        ...(b ? { bounds: [[b[0], b[1]], [b[2], b[3]]], fitBoundsOptions: { padding: 24, maxZoom: 16 } } : { center: [10, 51], zoom: 4 }),
        attributionControl: { compact: true },
        // the page scrolls past the map; zooming needs the modifier key
        cooperativeGestures: true,
      }));
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }), "bottom-right");
      map.dragRotate.disable();
      map.touchZoomRotate.disableRotation();
      ready = new Promise(r => map.on("load", r));
      // the map may be made before the page has its layout: it follows the size of its box
      new ResizeObserver(() => map.resize()).observe(el.value);
      map.on("load", () => map.resize());
      // the attribution starts as the (i) button
      const collapse = () => {
        const a = el.value && el.value.querySelector(".maplibregl-ctrl-attrib");
        if (a) { a.classList.remove("maplibregl-compact-show"); a.removeAttribute("open"); }
      };
      collapse();
      map.on("load", collapse);
      draw();
    }
    onMounted(create);
    watch(() => props.results, draw);
    return { el, emit };
  },
  template: `
    <div class="answer-map">
      <div class="map" ref="el"></div>
      <button v-if="stale" class="regenerate" :disabled="loading" @click="emit('regenerate')">{{ loading ? 'Re-generating …' : 'Click to re-generate' }}</button>
    </div>`,
};

/// The outputs with something to draw.
function geoOf(outputs) {
  return outputs.filter(o => o.geojson && o.geojson.features && o.geojson.features.length).map(o => ({ expr: o.expr, geojson: o.geojson }));
}

/// The box of all of them (kept with the answer, for the map after a reload).
function boxOf(geo) {
  const all = [Infinity, Infinity, -Infinity, -Infinity];
  for (const g of geo) {
    const b = bboxOf(g.geojson);
    if (b) extend(all, [[b[0], b[1]], [b[2], b[3]]]);
  }
  return all[0] <= all[2] ? all.map(x => Math.round(x * 1e5) / 1e5) : null;
}

/// For an answer that counts (`count of SELECTION`, one output, not grouped): the query that
/// shows what was counted, for "Show them on the map". `null` otherwise.
function mapQueryOf(query) {
  if (!query) return null;
  const lines = query.split("\n");
  // outputs: lines that are not assignments, settings or comments
  const outputs = lines.map((l, i) => [l, i]).filter(([l]) => l.trim() && !/^\s/.test(l) && !/^\s*#/.test(l) && !/^[A-Za-z_][A-Za-z0-9_]*\s*=[^=>]/.test(l));
  if (outputs.length !== 1) return null;
  const [line, i] = outputs[0];
  const m = line.trim().match(/^count of (.+)$/);
  if (!m || /\b(per|on each|from)\b/.test(m[1]) || /,/.test(m[1].replace(/\([^()]*\)|\[[^\]]*\]|'[^']*'/g, ""))) return null;
  let sel = m[1].trim();
  if (sel.startsWith("(") && sel.endsWith(")")) sel = sel.slice(1, -1);
  const out = lines.slice();
  out[i] = sel;
  return out.join("\n");
}

/// The data of a chart from the results: a time series (a line, time on the x axis) or
/// values per category (bars, the categories on the y axis). `null` if there is none.
function chartOf(outputs) {
  const TIME = ["year", "month", "week", "day"];
  for (const o of outputs) {
    const rows = Array.isArray(o.value) ? o.value : null;
    if (!rows || rows.length < 2 || !rows.every(r => r && typeof r === "object" && !Array.isArray(r))) continue;
    // elements (with their tags) are drawn on a map, not charted
    if (rows.some(r => "tags" in r || "geometry" in r)) continue;
    const keys = Object.keys(rows[0]);
    // coordinates and identities are never values of a chart (places belong on a map)
    const NOT = ["lon", "lat", "id", "version", "@id", "@version"];
    const num = keys.find(k => !NOT.includes(k) && rows.every(r => typeof r[k] === "number" || (r[k] && typeof r[k].value === "number")));
    if (!num) continue;
    const val = r => (typeof r[num] === "number" ? r[num] : r[num].value);
    const label = v => v && typeof v === "object" ? (v.tags && v.tags.name) || v["@label"] || `${v.type || ""} ${v.id || ""}`.trim() : String(v);
    const time = keys.find(k => TIME.includes(k));
    if (time) {
      // further keys: one line each (at most 8)
      const other = keys.find(k => k !== time && k !== num);
      const series = new Map();
      for (const r of rows) {
        const name = other ? label(r[other]) : o.expr;
        if (!series.has(name)) series.set(name, []);
        series.get(name).push({ x: String(r[time]), y: val(r) });
      }
      return { kind: "time", unit: time, series: [...series.entries()].slice(0, 8).map(([name, points]) => ({ name, points })) };
    }
    const cat = keys.find(k => k !== num);
    if (!cat) continue;
    const bars = rows.map(r => ({ name: label(r[cat]), value: val(r) })).filter(b => b.name !== "none");
    bars.sort((a, b) => b.value - a.value);
    return { kind: "bars", bars: bars.slice(0, 15), more: Math.max(0, bars.length - 15) };
  }
  return null;
}

/// A simple chart (d3): lines over time, or horizontal bars per category.
const AnswerChart = {
  props: { chart: Object },
  setup(props) {
    const el = ref(null);
    function draw() {
      if (!el.value || typeof d3 === "undefined") return;
      const c = props.chart;
      const box = d3.select(el.value);
      box.selectAll("*").remove();
      const W = el.value.clientWidth || 600;
      if (c.kind === "bars") {
        const row = 22, left = Math.min(220, 12 + 7 * Math.max(...c.bars.map(b => b.name.length)));
        const H = c.bars.length * row + 8;
        const svg = box.append("svg").attr("width", W).attr("height", H);
        const x = d3.scaleLinear().domain([0, d3.max(c.bars, b => b.value) || 1]).range([0, W - left - 60]);
        const g = svg.selectAll("g").data(c.bars).join("g").attr("transform", (_, i) => `translate(0,${i * row + 4})`);
        g.append("text").attr("class", "label").attr("x", left - 8).attr("y", row / 2).attr("dy", "0.32em").attr("text-anchor", "end")
          .text(b => b.name.length > 30 ? b.name.slice(0, 29) + "…" : b.name);
        g.append("rect").attr("x", left).attr("y", 3).attr("height", row - 7).attr("rx", 2).attr("fill", chatColors()[0]).attr("width", 0)
          .transition().duration(500).attr("width", b => Math.max(1, x(b.value)));
        g.append("text").attr("class", "value").attr("y", row / 2).attr("dy", "0.32em").attr("x", b => left + x(b.value) + 6).text(b => d3.format(",")(b.value));
        if (c.more) box.append("div").attr("class", "chart-note").text(`and ${c.more} more`);
        return;
      }
      const H = 220, m = { top: 10, right: 16, bottom: 26, left: 48 };
      const svg = box.append("svg").attr("width", W).attr("height", H);
      const xs = c.series[0].points.map(p => p.x);
      const x = d3.scalePoint().domain(xs).range([m.left, W - m.right]).padding(0.3);
      const ymax = d3.max(c.series.flatMap(s => s.points.map(p => p.y))) || 1;
      const y = d3.scaleLinear().domain([0, ymax]).nice().range([H - m.bottom, m.top]);
      // at most about one label per 60 px
      const step = Math.ceil(xs.length / Math.max(1, Math.floor((W - m.left - m.right) / 60)));
      svg.append("g").attr("class", "axis").attr("transform", `translate(0,${H - m.bottom})`)
        .call(d3.axisBottom(x).tickValues(xs.filter((_, i) => i % step === 0)).tickSizeOuter(0));
      svg.append("g").attr("class", "axis").attr("transform", `translate(${m.left},0)`).call(d3.axisLeft(y).ticks(5).tickFormat(d3.format("~s")));
      const line = d3.line().x(p => x(p.x)).y(p => y(p.y));
      c.series.forEach((s, i) => {
        const color = chatColors()[i % COLORS.length];
        const path = svg.append("path").datum(s.points).attr("fill", "none").attr("stroke", color).attr("stroke-width", 2).attr("d", line);
        const len = path.node().getTotalLength();
        path.attr("stroke-dasharray", `${len} ${len}`).attr("stroke-dashoffset", len).transition().duration(700).attr("stroke-dashoffset", 0);
        svg.append("g").selectAll("circle").data(s.points).join("circle").attr("cx", p => x(p.x)).attr("cy", p => y(p.y)).attr("r", 2.5).attr("fill", color)
          .append("title").text(p => `${p.x}: ${d3.format(",")(p.y)}`);
      });
      if (c.series.length > 1) {
        const lg = box.append("div").attr("class", "chart-legend");
        c.series.forEach((s, i) => lg.append("span").html(`<i style="background:${chatColors()[i % COLORS.length]}"></i>${escapeHtml(s.name)}`));
      }
    }
    onMounted(() => nextTick(draw));
    return { el };
  },
  template: `<div class="answer-chart" ref="el"></div>`,
};

const ChatView = {
  components: { AnswerMap, AnswerChart, Pick, AboutBox },
  props: { info: Object, db: String },
  emits: ["advanced", "open"],
  setup(props, { emit }) {
    const turns = ref(load("chat", []));
    const text = ref("");
    const busy = ref(false);
    const label = ref("");
    const input = ref(null);
    let job = null;
    let timer = null;
    // kept: questions, answers, queries, charts; not the geometries (too large) nor the state
    // of the animation
    watch(turns, v => store("chat", v.map(t => ({ ...t, outputs: undefined, geo: undefined, typed: undefined, reveal: undefined, regenerating: undefined }))), { deep: true });

    function history() {
      const m = [];
      for (const t of turns.value) {
        m.push({ role: "user", content: t.question });
        // the model sees its earlier queries and questions back (not the answers): follow-up
        // questions such as "and in Findorff?" refer to them
        const reply = t.query ? `QUERY:\n${t.query}` : t.ask ? `ASK: ${t.ask}` : null;
        if (reply) m.push({ role: "assistant", content: reply });
      }
      return m;
    }
    async function ask() {
      const q = text.value.trim();
      if (!q || busy.value) return;
      turns.value.push({ question: q });
      text.value = "";
      busy.value = true;
      label.value = "understanding the question";
      nextTick(scrollDown);
      try {
        const r = await fetch("api/chat", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ messages: history(), db: props.db }) });
        const j = await r.json();
        if (!j.job) throw new Error(j.error || "the server did not start");
        job = j.job;
        poll();
      } catch (e) {
        done({ error: e.message });
      }
    }
    async function poll() {
      try {
        const j = await (await fetch("api/job/" + job)).json();
        if (j.state === "running") {
          label.value = (j.progress && j.progress.label) || label.value;
          timer = setTimeout(poll, POLL_MS);
          return;
        }
        const c = j.chat || {};
        const outputs = (j.result && j.result.outputs) || [];
        // a chart for values over time or per category, else a map for geometries
        // (also without a text: a map may be the whole answer)
        const ok = !c.error && !c.ask && j.state !== "cancelled";
        const chart = ok ? chartOf(outputs) : null;
        const geo = !chart && ok ? geoOf(outputs) : [];
        done({ ...c, cancelled: j.state === "cancelled", outputs, chart, geo: geo.length ? geo : null, box: boxOf(geo), mapped: geo.length > 0, seconds: j.seconds });
      } catch (e) {
        done({ error: "Lost the connection to knut web: " + e.message });
      }
    }
    function done(fields) {
      clearTimeout(timer);
      const t = turns.value[turns.value.length - 1];
      Object.assign(t, fields);
      busy.value = false;
      job = null;
      if (t.answer || t.ask) typeOut(t);
      nextTick(() => { scrollDown(); input.value && input.value.focus(); });
    }
    // the geometries of an answer again (after a reload): its query runs once more; for an
    // answer that counts, the query of what was counted ("Show them on the map")
    function showOnMap(t) {
      t.mapQuery = mapQueryOf(t.query);
      regenerate(t);
    }
    async function regenerate(t) {
      const script = t.mapQuery || t.query;
      if (t.regenerating || !script) return;
      t.regenerating = true;
      try {
        const r = await fetch("api/run", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ script, db: props.db }) });
        const { job: id } = await r.json();
        for (;;) {
          await new Promise(res => setTimeout(res, POLL_MS));
          const j = await (await fetch("api/job/" + id)).json();
          if (j.state === "running") continue;
          const geo = geoOf((j.result && j.result.outputs) || []);
          if (geo.length) {
            t.geo = geo;
            t.box = boxOf(geo);
          }
          break;
        }
      } catch (e) {
        // stays as it is: a click tries again
      }
      t.regenerating = false;
    }
    // editing a question asks it again: the answers from there on are dropped
    const editing = ref(null);
    const editText = ref("");
    const editBox = ref(null);
    function startEdit(i) {
      editing.value = i;
      editText.value = turns.value[i].question;
      nextTick(() => {
        const box = Array.isArray(editBox.value) ? editBox.value[0] : editBox.value;
        if (box) { box.focus(); box.setSelectionRange(box.value.length, box.value.length); }
      });
    }
    function submitEdit() {
      const q = editText.value.trim();
      if (!q || busy.value || editing.value === null) return;
      turns.value.splice(editing.value);
      editing.value = null;
      text.value = q;
      ask();
    }
    // a question that failed (knut web not reachable, …) is sent once more
    function retry(i) {
      if (busy.value) return;
      const q = turns.value[i].question;
      turns.value.splice(i);
      text.value = q;
      ask();
    }
    function editKey(e) {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); submitEdit(); }
      else if (e.key === "Escape") { e.preventDefault(); editing.value = null; }
    }
    // the part of the answer written so far
    const shown = t => { const s = t.ask || t.answer || ""; return t.typed == null ? s : s.slice(0, t.typed); };
    // the answer appears as if written, quickly; then, a moment later, its map or chart
    function typeOut(t) {
      const text = t.answer || t.ask;
      t.typed = 0;
      t.reveal = false;
      const total = Math.min(1500, Math.max(300, text.length * 12));
      const t0 = performance.now();
      const tick = () => {
        const k = Math.min(1, (performance.now() - t0) / total);
        t.typed = Math.round(text.length * k);
        if (k < 1) return requestAnimationFrame(tick);
        setTimeout(() => { t.reveal = true; nextTick(scrollDown); }, 250);
      };
      requestAnimationFrame(tick);
    }
    async function cancel() {
      if (job) await fetch("api/job/" + job + "/cancel", { method: "POST" }).catch(() => {});
    }
    function clear() {
      if (busy.value) return;
      turns.value = [];
      nextTick(() => input.value && input.value.focus());
    }
    // the list scrolls to its end: the newest answer sits a little above the input field
    function scrollDown(smooth = true) {
      const el = document.querySelector(".chat .turns");
      if (!el) return;
      el.scrollTo({ top: el.scrollHeight, behavior: smooth ? "smooth" : "auto" });
    }
    // ↑ and ↓ go through the earlier questions (from the first or last line of the field); the
    // text being written comes back after the newest one
    let back = null;
    let draft = "";
    function browse(e, dir) {
      const ta = e.target;
      const before = ta.value.slice(0, ta.selectionStart), after = ta.value.slice(ta.selectionEnd);
      if (dir < 0 ? before.includes("\n") : after.includes("\n")) return;
      const qs = turns.value.map(t => t.question);
      if (!qs.length) return;
      if (back === null) {
        if (dir > 0) return;
        draft = text.value;
        back = qs.length;
      }
      const next = back + dir;
      if (next < 0) return;
      e.preventDefault();
      back = next >= qs.length ? null : next;
      text.value = back === null ? draft : qs[back];
      nextTick(() => ta.setSelectionRange(ta.value.length, ta.value.length));
    }
    watch(text, v => { if (back !== null && v !== turns.value[back]?.question) back = null; });
    function key(e) {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); back = null; ask(); }
      else if (e.key === "Escape" && busy.value) cancel();
      else if (e.key === "ArrowUp") browse(e, -1);
      else if (e.key === "ArrowDown") browse(e, 1);
    }
    onMounted(() => nextTick(() => { scrollDown(false); input.value && input.value.focus(); }));
    // the logo on the start page: still; it moves once 100 ms after the first character, and
    // again whenever the field was emptied and one starts anew
    const loader = ref(null);
    const loaderSvg = ref("");
    fetch("knut-loading.svg").then(r => r.text()).then(svg => { loaderSvg.value = loaderMarkup(svg); }).catch(() => {});
    let beat = null;
    function play() {
      const el = loader.value;
      if (!el) return;
      el.classList.remove("play");
      void el.offsetWidth;
      el.classList.add("play");
    }
    watch(text, (now, before) => {
      if (turns.value.length || !now || before) return;
      clearTimeout(beat);
      beat = setTimeout(play, 100);
    });
    onUnmounted(() => clearTimeout(beat));
    // about knut, in a box over the page
    const about = ref(false);
    // the advanced editor only after a word of warning (unless one asked not to be asked again)
    const warn = ref(false);
    const dontAsk = ref(false);
    function toAdvanced() {
      if (load("advanced:no-warning", false)) emit("advanced");
      else { dontAsk.value = false; warn.value = true; }
    }
    function confirmAdvanced() {
      if (dontAsk.value) store("advanced:no-warning", true);
      warn.value = false;
      emit("advanced");
    }
    // a new conversation only after asking: the current one is gone then
    const asking = ref(false);
    const closeAbout = e => { if (e.key === "Escape") { warn.value = false; asking.value = false; } };
    window.addEventListener("keydown", closeAbout);
    onUnmounted(() => window.removeEventListener("keydown", closeAbout));
    // copies a text; the symbol shows a check for a moment
    const copied = ref(null);
    function copy(text, id) {
      if (navigator.clipboard) navigator.clipboard.writeText(text).catch(() => {});
      copied.value = id;
      setTimeout(() => { if (copied.value === id) copied.value = null; }, 1200);
    }
    return { turns, text, busy, label, input, ask, cancel, clear, key, emit, fmtSeconds, theme, THEMES, ICONS, copied, copy, shown, regenerate, showOnMap, mapQueryOf,
      editing, editText, editBox, startEdit, submitEdit, editKey, retry, loader, loaderSvg,
      about, accent, ACCENTS, warn, dontAsk, toAdvanced, confirmAdvanced, asking };
  },
  template: `
    <div :class="['chat', { start: !turns.length }]">
      <!-- the logo moves to the corner once the one in the middle is gone -->
      <div :class="['chat-brand', { shown: turns.length }]" aria-hidden="true"><img src="knut.svg" alt=""></div>
      <div class="chat-top">
        <template v-if="turns.length">
          <button class="icon" :disabled="busy" title="Start a new conversation" aria-label="Start a new conversation" @click="asking = true" v-html="ICONS.restart"></button>
          <span class="sep">|</span>
        </template>
        <Pick :items="ACCENTS" v-model="accent" label="Accent colour" />
        <Pick :items="THEMES" v-model="theme" label="Appearance" />
        <button class="icon" title="About knut" aria-label="About knut" @click="about = true" v-html="ICONS.bird"></button>
        <span class="sep">|</span>
        <button class="icon" title="Advanced query editor" aria-label="Advanced query editor" @click="toAdvanced" v-html="ICONS.rocket"></button>
      </div>
      <div class="turns">
        <div v-if="!turns.length" ref="loader" class="chat-logo knut-loader" role="img" aria-label="knut" v-html="loaderSvg"></div>
        <div v-for="(t, i) in turns" :key="i" class="turn">
          <!-- a question being edited: asked again from here on (Enter), or left as it was (Esc) -->
          <div v-if="editing === i" class="question-edit">
            <textarea ref="editBox" v-model="editText" rows="1" @keydown="editKey" aria-label="Edit the question"></textarea>
            <div class="edit-buttons">
              <button class="small" @click="editing = null">Cancel</button>
              <button class="small primary" :disabled="!editText.trim() || busy" @click="submitEdit">Send</button>
            </div>
          </div>
          <div v-else class="question-row">
            <div class="question">{{ t.question }}</div>
            <div class="q-tools">
              <button class="copy" :title="copied === 'q' + i ? 'Copied' : 'Copy'" aria-label="Copy the question" @click="copy(t.question, 'q' + i)"
                v-html="copied === 'q' + i ? ICONS.check : ICONS.copy"></button>
              <button v-if="!busy" class="copy" title="Edit and ask again" aria-label="Edit the question" @click="startEdit(i)" v-html="ICONS.pencil"></button>
            </div>
          </div>
          <div class="reply">
            <div v-if="t.ask || t.answer" class="answer-row">
              <button class="copy" :title="copied === 'a' + i ? 'Copied' : 'Copy'" aria-label="Copy the answer" @click="copy(t.ask || t.answer, 'a' + i)"
                v-html="copied === 'a' + i ? ICONS.check : ICONS.copy"></button>
              <p class="answer">{{ shown(t) }}</p>
            </div>
            <p v-else-if="t.cancelled" class="note">Cancelled.</p>
            <div v-else-if="t.error" class="error-row">
              <p class="note bad">{{ t.error }}</p>
              <button v-if="!busy" class="retry" title="Ask again" aria-label="Ask the question again" @click="retry(i)" v-html="ICONS.retry"></button>
            </div>
            <p v-else-if="busy && i === turns.length - 1" class="note busy">{{ label }} …</p>
            <Transition name="fade">
              <AnswerChart v-if="t.chart && t.reveal !== false" :chart="t.chart" />
              <AnswerMap v-else-if="(t.geo || t.box) && t.reveal !== false" :results="t.geo || []" :box="t.box" :stale="!t.geo"
                :loading="!!t.regenerating" @regenerate="regenerate(t)" />
              <button v-else-if="t.answer && t.reveal !== false && mapQueryOf(t.query)" class="show-map" :disabled="!!t.regenerating"
                @click="showOnMap(t)">{{ t.regenerating ? 'drawing the map…' : 'Show them on the map' }}</button>
            </Transition>
            <!-- right below the answer: the time it took; opened, the query and a way to the editor -->
            <details v-if="t.query" class="query">
              <summary :title="'The query' + (t.seconds ? '; the whole answer took ' + fmtSeconds(t.seconds) : '')">{{ t.seconds ? fmtSeconds(t.seconds) : 'query' }}</summary>
              <div class="query-code">
                <pre>{{ t.query }}</pre>
                <div class="query-tools">
                  <span v-if="t.query_seconds != null" class="query-time" title="The time the query took to run (without the language model)">{{ fmtSeconds(t.query_seconds) }}</span>
                  <button v-if="!(busy && i === turns.length - 1)" class="icon" title="Open in the advanced query editor" aria-label="Open in the advanced query editor"
                    @click="emit('open', t)" v-html="ICONS.rocket"></button>
                </div>
              </div>
              <ul v-if="t.problems && t.problems.length" class="problems"><li v-for="(p, k) in t.problems" :key="k"><pre>{{ p }}</pre></li></ul>
            </details>
          </div>
        </div>
      </div>
      <div class="chat-foot">
      <div :class="['chat-input', { busy }]">
        <textarea ref="input" v-model="text" rows="1" :placeholder="turns.length ? 'Ask a follow-up question …' : 'Ask about the OpenStreetMap data, e.g. How many benches are there in Vegesack?'"
          @keydown="key" :disabled="busy && false"></textarea>
        <button v-if="!busy" :class="['primary', 'ask', { empty: !text.trim() }]" :disabled="!text.trim()" :tabindex="text.trim() ? 0 : -1" title="Ask (Enter)" @click="ask">Ask</button>
        <button v-else class="cancel" title="Cancel (Esc)" @click="cancel">Cancel</button>
      </div>
      <p v-if="turns.length" class="chat-disclaimer">knut is experimental and might be mistaken.</p>
      </div>
      <AboutBox :info="info" :open="about" @close="about = false" />
      <Transition name="fade">
        <div v-if="asking" class="about-overlay" @click.self="asking = false">
          <div class="about-box confirm-box" role="dialog" aria-label="Start a new conversation">
            <h3>Start a new conversation</h3>
            <p>Are you sure you want to delete the current conversation?</p>
            <div class="confirm-buttons">
              <button @click="asking = false">Cancel</button>
              <button class="primary" @click="asking = false; clear()">Delete current conversation</button>
            </div>
          </div>
        </div>
      </Transition>
      <Transition name="fade">
        <div v-if="warn" class="about-overlay" @click.self="warn = false">
          <div class="about-box confirm-box" role="dialog" aria-label="Open the advanced query editor">
            <h3>Advanced query editor</h3>
            <p>The advanced editor is designed for users who are comfortable with writing queries using the knut language themselves. Are you sure you want to switch? (You can always switch back using the <span class="inline-icon" v-html="ICONS.chat"></span> sign.)</p>
            <label class="dont-ask"><input type="checkbox" v-model="dontAsk"> Don't ask again</label>
            <div class="confirm-buttons">
              <button @click="warn = false">Stay</button>
              <button class="primary" @click="confirmAdvanced">Open advanced editor</button>
            </div>
          </div>
        </div>
      </Transition>
    </div>`,
};

const App = {
  components: { Panel, MapView, ChatView, AboutBox },
  setup() {
    const map = ref(null);
    const editorHost = ref(null);
    let editor = null;
    const state = reactive({
      tab: "query",
      about: false,
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
      // "text" (the text interface: questions in plain language, if the server has a language
      // model) or "advanced"; "simple" is the old name of "text"
      mode: load("mode", "text") === "simple" ? "text" : load("mode", "text"),
    });
    watch(() => state.mode, v => store("mode", v));
    // shown at once when chosen (before the server has said whether it has a language model,
    // so that the advanced editor does not flash up); without a model it fades over to it
    const textMode = computed(() => state.mode === "text" && (!state.info || !!state.info.chat));
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
        if (res.outputs.length) nudge(".panel.right");
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
    // a turn of the text interface in the editor, its results on the map
    function openTurn(t) {
      state.mode = "advanced";
      state.tab = "query";
      nextTick(() => {
        if (editor) editor.setValue(t.query);
        state.query = t.query;
        if (t.outputs && t.outputs.length) {
          state.results = t.outputs;
          state.elapsed = t.query_seconds || 0;
          state.visible = t.outputs.map(() => true);
          map.value.show(t.outputs);
          nudge(".panel.right");
        } else {
          run();
        }
      });
    }

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
      summary, toggleLayer, zoomTo, saveJson, saveGeojson, copyText, COLORS, fmtSeconds, theme, textMode, openTurn,
      themes: THEMES, ICONS, accent, accents: ACCENTS,
    };
  },
  template: `
    <Transition name="cross">
      <ChatView v-if="textMode" :info="state.info" :db="state.db" @advanced="state.mode = 'advanced'" @open="openTurn" />
    </Transition>
    <MapView ref="map" :info="state.info" />

    <Panel side="left" label="the query panel">
      <template #header>
        <img class="brand" src="icon.svg" alt="" aria-hidden="true">
        <nav class="tabs" role="tablist">
          <button v-for="t in ['query', 'examples', 'docs']" :key="t" role="tab"
            :class="{ active: state.tab === t }" @click="state.tab = t">{{ t[0].toUpperCase() + t.slice(1) }}</button>
          <button role="tab" :class="['symbol', { active: state.tab === 'settings' }]" title="Settings" aria-label="Settings" @click="state.tab = 'settings'" v-html="ICONS.settings"></button>
        </nav>
      </template>
      <template #tools>
        <button class="icon" title="About knut" aria-label="About knut" @click="state.about = true" v-html="ICONS.bird"></button>
      </template>

      <div v-show="state.tab === 'query'" class="tabpane">
        <div class="cm-host" ref="editorHost"></div>
        <!-- the messages right below the editor, over the buttons -->
        <div v-if="state.messages" class="messages">
          <pre><template v-for="(l, i) in messageLines" :key="i"><a v-if="l.line" :class="l.cls" @click="goToLine(l.line)">{{ l.text }}</a><span v-else :class="l.cls">{{ l.text }}</span>
</template></pre>
        </div>
        <div class="actions">
          <button v-if="!state.running" class="primary" :disabled="!state.query.trim()" title="Run (⌘/Ctrl + Enter)" @click="run">Run</button>
          <button v-else class="cancel" :disabled="state.cancelling" title="Cancel (Esc)" @click="cancel">Cancel</button>
          <button title="Clear the query, the results and the map" @click="reset">Reset</button>
          <div v-if="state.running" class="progress">
            <div class="label"><span>{{ progressLabel }}</span><span>{{ progressRight }}</span></div>
            <div :class="['bar', { busy: progressPct === null }]"><div class="fill" :style="{ width: (progressPct ?? 30) + '%' }"></div></div>
          </div>
          <span v-else-if="state.status" :class="['status', state.statusKind]" :title="state.status">{{ state.status }}</span>
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
        </section>
        <section>
          <h3>Appearance</h3>
          <div class="themes" role="radiogroup" aria-label="Appearance">
            <button v-for="t in themes" :key="t.id" role="radio" :aria-checked="theme === t.id"
              :class="{ active: theme === t.id }" :title="t.title" @click="theme = t.id">
              <span v-html="t.icon"></span>{{ t.label }}
            </button>
          </div>
        </section>
        <section>
          <h3>Accent colour</h3>
          <div class="themes" role="radiogroup" aria-label="Accent colour">
            <button v-for="a in accents" :key="a.id" role="radio" :aria-checked="accent === a.id"
              :class="{ active: accent === a.id }" :title="a.title" @click="accent = a.id">
              <span class="swatch" :style="{ background: a.color }"></span>{{ a.label }}
            </button>
          </div>
        </section>
      </div>
    </Panel>

    <Panel side="right" label="the results">
      <template #header>
        <h2>Results <span class="meta">{{ summary }}</span></h2>
      </template>
      <template #tools>
        <button v-if="state.info && state.info.chat" class="icon" title="Text interface: questions in plain language" aria-label="Text interface"
          @click="state.mode = 'text'" v-html="ICONS.chat"></button>
      </template>
      <template #strip>
        <button v-if="state.info && state.info.chat" class="icon" title="Text interface: questions in plain language" aria-label="Text interface"
          @click="state.mode = 'text'" v-html="ICONS.chat"></button>
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
    </Panel>
    <AboutBox :info="state.info" :open="state.about" @close="state.about = false" />`,
};

createApp(App).mount("#app");
