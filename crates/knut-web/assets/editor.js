// knut web: the query editor (CodeMirror 6) with highlighting, messages and completion.
//
// The packages come through the import map in index.html: one pinned instance each.

import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection, placeholder } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab, toggleLineComment } from "@codemirror/commands";
import { StreamLanguage, HighlightStyle, syntaxHighlighting, bracketMatching } from "@codemirror/language";
import { tags } from "@lezer/highlight";
import { setDiagnostics, lintGutter } from "@codemirror/lint";
import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from "@codemirror/autocomplete";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";

// ----- the language ---------------------------------------------------------------------

const KEYWORDS = new Set([
  "with", "within", "on", "from", "to", "before", "after", "between", "and", "or", "not", "in", "pick", "exactly",
  "randomly", "using", "seed", "the", "all", "some", "no", "nearest", "of", "by", "per", "having", "closer", "than",
  "equal", "near", "overlapping", "next", "previous", "define", "split", "at", "each", "path", "isochrone",
  "dataset", "graph", "groups", "ordered", "cell", "tile", "zoom", "chunk", "correlate", "regress", "confusion",
  "fit", "powerlaw", "forecast", "classify", "geocode", "geocoded",
]);
const ATOMS = new Set(["none", "true", "false", "foot", "bicycle", "car", "transit", "en", "de"]);
const KINDS = new Set(["node", "nodes", "way", "ways", "area", "areas", "relation", "relations", "changeset", "changesets",
  "user", "users", "note", "notes", "key", "keys"]);
const FUNCS = new Set(["count", "sum", "average", "distribution", "share", "entropy", "gini", "variance", "density",
  "spread", "streaks", "transitions", "distinct", "earliest", "latest", "days", "weeks", "months", "years", "user",
  "first", "last", "nearest", "graph"]);
const SETTINGS = new Set(["knut_db", "seed", "messages", "ignore_warnings", "near_distance"]);
const UNITS = new Set(["m", "km", "mi", "mile", "miles", "mil", "m2", "km2", "mi2", "mile2", "miles2", "mil2", "ha",
  "mph", "knots", "knot", "t", "kg", "g", "s", "min", "h", "days", "weeks", "months", "years", "day", "week", "month", "year"]);
const TIME_UNITS = new Set(["day", "week", "month", "year"]);
const MONTHS = /^(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*$/i;
const FIELDS = ["type", "osmType", "id", "version", "time", "deleted", "visible", "keys", "tags", "geometry", "center",
  "lon", "lat", "user", "creator", "changeset", "next", "previous", "unchangedBefore", "unchangedAfter", "length",
  "area", "nodes", "members", "roles", "versions", "firstEdit", "lastEdit", "activeSpan", "activeDays", "changesets",
  "edits", "name", "count", "closedAt", "duration", "bbox", "comment", "status", "text", "created", "closed",
  "comments", "values", "number", "lower", "upper", "items", "key", "map", "filter", "edges", "accuracy",
  "predictions", "slope", "intercept", "r2", "alpha", "xmin", "address", "housenumber", "street", "postcode",
  "suburb", "district", "city", "county", "state", "state_code", "country", "country_code", "continent", "score",
  "distance"];

const knut = StreamLanguage.define({
  name: "knut",
  startState: () => ({ after: null, line: true }),
  token(stream, st) {
    if (stream.sol()) st.line = true;
    if (stream.eatSpace()) return null;
    const lineStart = st.line;
    st.line = false;
    const prev = st.after;
    st.after = null;
    if (stream.match(/#.*/)) return "comment";
    if (stream.peek() === "'") {
      stream.next();
      while (!stream.eol()) if (stream.next() === "'") break;
      return "string";
    }
    if (stream.match(/\d{4}-\d{1,2}(-\d{1,2})?(T\d{1,2}:\d{2}(:\d{2})?(Z|[+-]\d{2}:\d{2})?)?/)) return "date";
    if (stream.match(/\d+(\.\d+)?\.\.\.\d+(\.\d+)?/)) return "number";
    // a coordinate of a point: 8.8079E, 53.0758N
    if (stream.match(/\d+(\.\d+)?[EWNS](?![A-Za-z0-9_])/)) return "number";
    if (stream.match(/\d+(\.\d+)?/)) { st.after = "num"; return "number"; }
    if ((prev === "num" || prev === "tag") && (stream.match("km/h") || stream.match("%"))) return "unit";
    const glued = stream.pos > 0 && !/\s/.test(stream.string[stream.pos - 1]);
    if (glued && stream.match(/:(\[[^\]]*\]|[A-Za-z0-9_]+(:[A-Za-z0-9_]+)*)/)) {
      st.after = "tagkey";
      return "tagKey";
    }
    if (prev === "tagkey" && stream.match(/=('[^']*'|[A-Za-z0-9_+\-]+)/)) return "tagValue";
    if (prev === "tagkey") st.after = "tag";
    if (glued && stream.match(/\.[A-Za-z_][A-Za-z0-9_]*/)) return "field";
    if (stream.match(/->|=>|==|!=|<=|>=|\?\?|[-+*/<>=]/)) {
      if (prev === "tagkey") st.after = "tag";
      return "operator";
    }
    if (stream.match(/reverse-geocoded?(?![A-Za-z0-9_])/)) return "keyword";
    const w = stream.match(/[A-Za-z_][A-Za-z0-9_]*/);
    if (w) {
      const word = w[0];
      if ((prev === "num" || prev === "tag") && UNITS.has(word)) return "unit";
      if (prev === "tagkey" && UNITS.has(word)) return "unit";
      if (lineStart && SETTINGS.has(word) && stream.match(/\s*=/, false)) return "setting";
      if (lineStart && word === "define") return "keyword";
      if (FUNCS.has(word) && stream.match(/\s+(version\s+)?of\b/, false)) return "func";
      if (KINDS.has(word)) return "kind";
      if (KEYWORDS.has(word)) return "keyword";
      if (ATOMS.has(word)) return "atom";
      if (TIME_UNITS.has(word) || MONTHS.test(word)) return "date";
      return "variable";
    }
    stream.next();
    return null;
  },
  tokenTable: {
    comment: tags.comment,
    string: tags.string,
    number: tags.number,
    date: tags.special(tags.number),
    unit: tags.unit,
    tagKey: tags.attributeName,
    tagValue: tags.attributeValue,
    field: tags.propertyName,
    operator: tags.operator,
    setting: tags.definitionKeyword,
    keyword: tags.keyword,
    kind: tags.typeName,
    func: tags.function(tags.variableName),
    atom: tags.atom,
    variable: tags.variableName,
  },
  languageData: { commentTokens: { line: "#" } },
});

const highlight = HighlightStyle.define([
  { tag: tags.comment, color: "var(--hl-comment)", fontStyle: "italic" },
  { tag: tags.string, color: "var(--hl-string)" },
  { tag: [tags.number, tags.special(tags.number)], color: "var(--hl-number)" },
  { tag: tags.unit, color: "var(--hl-unit)" },
  { tag: tags.attributeName, color: "var(--hl-tag)" },
  { tag: tags.attributeValue, color: "var(--hl-string)" },
  { tag: tags.propertyName, color: "var(--hl-field)" },
  { tag: tags.operator, color: "var(--hl-operator)" },
  { tag: [tags.keyword, tags.definitionKeyword], color: "var(--hl-keyword)", fontWeight: "600" },
  { tag: tags.typeName, color: "var(--hl-kind)", fontWeight: "600" },
  { tag: tags.function(tags.variableName), color: "var(--hl-func)" },
  { tag: tags.atom, color: "var(--hl-atom)" },
  { tag: tags.variableName, color: "var(--fg)" },
]);

const theme = EditorView.theme({
  "&": { height: "100%", fontSize: "13px", backgroundColor: "var(--bg-code)", color: "var(--fg)" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--mono)", lineHeight: "1.5" },
  ".cm-content": { caretColor: "var(--fg)", padding: "8px 0" },
  ".cm-gutters": { backgroundColor: "var(--bg-code)", color: "var(--fg-soft)", border: "none", borderRight: "1px solid var(--border)" },
  ".cm-activeLine": { backgroundColor: "var(--hl-active)" },
  ".cm-activeLineGutter": { backgroundColor: "var(--hl-active)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": { backgroundColor: "var(--hl-selection) !important" },
  ".cm-cursor": { borderLeftColor: "var(--fg)" },
  ".cm-tooltip": { backgroundColor: "var(--bg)", color: "var(--fg)", border: "1px solid var(--border)", borderRadius: "6px" },
  ".cm-tooltip-lint": { maxWidth: "520px" },
  ".cm-diagnosticText": { whiteSpace: "pre-wrap", fontFamily: "var(--mono)", fontSize: "12px" },
  ".cm-tooltip-autocomplete ul li[aria-selected]": { backgroundColor: "var(--accent)", color: "var(--accent-fg)" },
  ".cm-placeholder": { color: "var(--fg-soft)" },
});

// ----- completion -----------------------------------------------------------------------

const COMPLETIONS = [
  ...[...KEYWORDS].map(label => ({ label, type: "keyword" })),
  ...[...KINDS].map(label => ({ label, type: "type" })),
  ...[...FUNCS].map(label => ({ label: label + " of", type: "function", apply: label + " of " })),
  ...[...SETTINGS].map(label => ({ label, type: "variable", detail: "setting", apply: label + " = " })),
  ...["none", "true", "false"].map(label => ({ label, type: "constant" })),
  ...["foot", "bicycle", "car"].map(label => ({ label, type: "constant", detail: "way of moving" })),
];

function complete(ctx) {
  const field = ctx.matchBefore(/\.[A-Za-z_]\w*$|\.$/);
  if (field && field.from > 0 && /\w/.test(ctx.state.sliceDoc(field.from - 1, field.from))) {
    return { from: field.from + 1, options: FIELDS.map(label => ({ label, type: "property" })), validFor: /^\w*$/ };
  }
  const word = ctx.matchBefore(/[A-Za-z_]\w*/);
  if (!word || (word.from === word.to && !ctx.explicit)) return null;
  // no completion inside tag keys (`n:amenity`)
  if (word.from > 0 && ctx.state.sliceDoc(word.from - 1, word.from) === ":") return null;
  return { from: word.from, options: COMPLETIONS, validFor: /^\w*$/ };
}

// ----- editor ---------------------------------------------------------------------------

/** Creates the editor in `parent`. Returns {getValue, setValue, setProblems, goTo, focus}. */
export function createEditor(parent, { doc = "", onChange, onRun, onEscape, hint = "" }) {
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc,
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        history(),
        drawSelection(),
        bracketMatching(),
        closeBrackets(),
        highlightActiveLine(),
        highlightSelectionMatches(),
        lintGutter(),
        autocompletion({ override: [complete], icons: false }),
        placeholder(hint),
        EditorView.lineWrapping,
        knut,
        syntaxHighlighting(highlight),
        theme,
        keymap.of([
          { key: "Mod-Enter", run: () => { onRun && onRun(); return true; } },
          { key: "Escape", run: () => (onEscape ? onEscape() : false) },
          // (un)comments the lines: ⌘⇧/ (on a German keyboard ⌘⇧7, which Safari and Chrome report as ⌘⇧&)
          ...["Mod-Shift-/", "Mod-?", "Mod-Shift-7", "Mod-&"].map(key => ({ key, run: toggleLineComment })),
          ...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...completionKeymap, indentWithTab,
        ]),
        EditorView.updateListener.of(u => { if (u.docChanged && onChange) onChange(u.state.doc.toString()); }),
      ],
    }),
  });

  return {
    getValue: () => view.state.doc.toString(),
    /** Replaces the text as one undoable change. */
    setValue(text) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text }, selection: { anchor: text.length } });
      view.focus();
    },
    /** Marks knut's messages: [{from, to, severity, code, title, text}]. */
    setProblems(problems) {
      const n = view.state.doc.length;
      const ds = (problems || []).map(p => {
        let from = Math.min(p.from, n), to = Math.min(Math.max(p.to, p.from), n);
        if (from === to) { if (to < n) to += 1; else if (from > 0) from -= 1; }
        return { from, to, severity: p.severity, source: p.code, message: p.text || p.title };
      });
      view.dispatch(setDiagnostics(view.state, ds));
    },
    goTo(line) {
      const l = view.state.doc.line(Math.max(1, Math.min(line, view.state.doc.lines)));
      view.dispatch({ selection: { anchor: l.from, head: l.to }, scrollIntoView: true });
      view.focus();
    },
    focus: () => view.focus(),
  };
}
