//! The look of the pages toolsite serves itself — the index, sign-in, consent
//! and admin. Published apps are not styled from here; they bring their own.
//!
//! One set of tokens and one set of element and component rules, so a page
//! added later inherits the theme instead of growing its own. Colours are
//! defined once and flipped by `prefers-color-scheme`; nothing below
//! hard-codes a colour.
//!
//! The vocabulary is deliberately the one a shadcn page uses — card, badge,
//! tabs, field, button variants — because people recognise it, and because
//! keeping to a known set stops every new page inventing a class. There is
//! no build step: the stylesheet is this string, the behaviour is the short
//! script at the bottom, and the browser's own `<dialog>` does the confirms.

use maud::{html, Markup, PreEscaped, DOCTYPE};

/// Design tokens plus the element and component rules every page shares.
pub const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #fafafa;
  --fg: #09090b;
  --muted: #71717a;
  --card: #ffffff;
  --border: #e4e4e7;
  --soft: #f4f4f5;
  --primary: #18181b;
  --primary-fg: #fafafa;
  --accent: #4f46e5;
  --danger: #dc2626;
  --danger-soft: #fef2f2;
  --ok: #15803d;
  --ok-soft: #f0fdf4;
  --radius: .5rem;
  --gap: .75rem;
  --shadow: 0 1px 2px #0000000d;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #09090b;
    --fg: #fafafa;
    --muted: #a1a1aa;
    --card: #18181b;
    --border: #27272a;
    --soft: #1f1f23;
    --primary: #fafafa;
    --primary-fg: #18181b;
    --accent: #818cf8;
    --danger: #ef4444;
    --danger-soft: #2a1515;
    --ok: #4ade80;
    --ok-soft: #132a1c;
    --shadow: none;
  }
}

* { box-sizing: border-box; }
body {
  margin: 0;
  padding: 3rem 1.5rem;
  background: var(--bg);
  color: var(--fg);
  font: 15px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Inter", sans-serif;
  -webkit-font-smoothing: antialiased;
}
.container { max-width: 44rem; margin: 0 auto; }
.narrow { max-width: 22rem; }

h1 { font-size: 1.5rem; font-weight: 600; letter-spacing: -.01em; margin: 0 0 .25rem; }
h2 { font-size: 1rem; font-weight: 600; margin: 2rem 0 .5rem; }
h3 { font-size: .95rem; font-weight: 600; margin: 0; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
p { margin: 0 0 .75rem; }
.muted { color: var(--muted); font-size: .9rem; margin: 0 0 1.5rem; }
.small { font-size: .85rem; }
code, pre {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
code {
  background: var(--soft);
  padding: .1rem .35rem; border-radius: .3rem; font-size: .85em;
}
pre {
  background: var(--soft); border: 1px solid var(--border); border-radius: var(--radius);
  padding: .75rem .9rem; overflow-x: auto; font-size: .85rem; margin: 0 0 .75rem;
}
pre code { background: none; padding: 0; font-size: inherit; }
hr { border: 0; border-top: 1px solid var(--border); margin: 1.5rem 0; }

/* Anything that sits on the background as its own block. */
.card {
  display: flex; align-items: center; gap: var(--gap);
  padding: .7rem .9rem;
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); color: var(--fg); text-decoration: none;
  box-shadow: var(--shadow);
  transition: border-color .15s ease;
}
a.card:hover { border-color: var(--muted); text-decoration: none; }
.stack { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .5rem; }

/* A panel with a heading: the unit an admin page is built from. */
.panel {
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); box-shadow: var(--shadow);
  margin: 0 0 1rem;
}
.panel-head { padding: 1rem 1.25rem .25rem; }
.panel-head p { color: var(--muted); font-size: .85rem; margin: .15rem 0 0; }
.panel-body { padding: .75rem 1.25rem 1.25rem; }
.panel-body > :last-child { margin-bottom: 0; }
.panel-foot {
  padding: .75rem 1.25rem; border-top: 1px solid var(--border);
  display: flex; gap: .5rem; align-items: center; justify-content: flex-end;
  background: var(--soft); border-radius: 0 0 var(--radius) var(--radius);
}
.panel.danger { border-color: color-mix(in srgb, var(--danger) 40%, var(--border)); }
.grid-2 { display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; }
@media (max-width: 40rem) { .grid-2 { grid-template-columns: 1fr; } }

/* Label/value pairs on an overview. */
.kv { display: grid; grid-template-columns: max-content 1fr; gap: .4rem 1.5rem; margin: 0; }
.kv dt { color: var(--muted); font-size: .85rem; }
.kv dd { margin: 0; min-width: 0; overflow-wrap: anywhere; }

/* Forms. */
input, select, textarea, button {
  font: inherit; color: inherit;
  padding: .45rem .7rem;
  border: 1px solid var(--border); border-radius: calc(var(--radius) - .1rem);
  background: var(--card);
}
input::placeholder, textarea::placeholder { color: var(--muted); }
input:focus-visible, select:focus-visible, textarea:focus-visible, button:focus-visible {
  outline: 2px solid var(--accent); outline-offset: 1px;
}
textarea { width: 100%; min-height: 8rem; resize: vertical; }
button, .btn {
  display: inline-flex; align-items: center; gap: .4rem;
  padding: .45rem .9rem; cursor: pointer; white-space: nowrap;
  background: var(--primary); color: var(--primary-fg); border-color: var(--primary);
  border-radius: calc(var(--radius) - .1rem); font-weight: 500; font-size: .9rem;
  text-decoration: none; line-height: 1.4;
}
button:hover, .btn:hover { opacity: .9; text-decoration: none; }
button.quiet, .btn.quiet, button.secondary, .btn.secondary {
  background: var(--card); color: var(--fg); border-color: var(--border);
}
button.quiet:hover, .btn.quiet:hover, button.secondary:hover, .btn.secondary:hover {
  background: var(--soft); opacity: 1;
}
button.ghost, .btn.ghost { background: transparent; border-color: transparent; color: var(--fg); }
button.ghost:hover, .btn.ghost:hover { background: var(--soft); opacity: 1; }
button.danger, .btn.danger { background: var(--danger); border-color: var(--danger); color: #fff; }
button.danger.quiet, .btn.danger.quiet {
  background: transparent; color: var(--danger); border-color: var(--border);
}
button.danger.quiet:hover { background: var(--danger-soft); }
button.sm, .btn.sm { padding: .25rem .6rem; font-size: .8rem; }
button:disabled { opacity: .5; cursor: not-allowed; }
form.row { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; margin: .75rem 0; }
form.column { display: flex; flex-direction: column; gap: var(--gap); }
.field { display: flex; flex-direction: column; gap: .3rem; margin: 0 0 1rem; }
.field label { font-size: .85rem; font-weight: 500; }
.field .help { color: var(--muted); font-size: .8rem; margin: 0; }
.field input, .field select { width: 100%; max-width: 28rem; }
.choices { display: flex; flex-direction: column; gap: .4rem; margin: 0 0 1rem; }
.choice {
  display: grid; grid-template-columns: auto 1fr; gap: .25rem .75rem; align-items: start;
  padding: .6rem .8rem; border: 1px solid var(--border); border-radius: var(--radius);
  cursor: pointer; background: var(--card);
}
.choice:has(input:checked) { border-color: var(--primary); background: var(--soft); }
.choice input { margin: .3rem 0 0; }
.choice strong { font-weight: 500; }
.choice span { grid-column: 2; color: var(--muted); font-size: .85rem; }
.actions { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; }
.actions.end { justify-content: flex-end; }
/* The one question a consent page asks: two buttons, room between them. */
.actions.consent { gap: .75rem; margin-top: .75rem; }
.actions.consent button { flex: 1; justify-content: center; }

/* Tables: a list of things, each row a place to go. */
table { width: 100%; border-collapse: collapse; font-size: .9rem; }
th {
  text-align: left; font-weight: 500; color: var(--muted); font-size: .8rem;
  padding: .5rem .75rem; border-bottom: 1px solid var(--border);
}
td { padding: .6rem .75rem; border-bottom: 1px solid var(--border); vertical-align: middle; }
tr:last-child td { border-bottom: 0; }
tbody tr:hover td { background: var(--soft); }
td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
td.actions-cell { text-align: right; white-space: nowrap; }
td.actions-cell form { display: inline-flex; margin: 0; }
/* A table fills its panel edge to edge. Inside a padded body it bleeds out
   past the padding; placed directly in the panel it is simply full width. */
.panel-body > table { margin: 0 -1.25rem; width: calc(100% + 2.5rem); }
.panel-body > table + form { margin-top: 1rem; }
.panel table th:first-child, .panel table td:first-child { padding-left: 1.25rem; }
.panel table th:last-child, .panel table td:last-child { padding-right: 1.25rem; }
.panel > table { table-layout: auto; }
.panel > table tr:last-child td:first-child { border-bottom-left-radius: var(--radius); }
.panel > table tr:last-child td:last-child { border-bottom-right-radius: var(--radius); }
.panel { overflow: hidden; }
a.row-link { color: inherit; font-weight: 500; }
a.row-link:hover { text-decoration: underline; }

/* Badges. */
.badge {
  display: inline-flex; align-items: center; gap: .3rem;
  padding: .1rem .55rem; border-radius: 999px; font-size: .75rem; font-weight: 500;
  border: 1px solid var(--border); background: var(--soft); color: var(--fg);
  white-space: nowrap;
}
.badge.ok { background: var(--ok-soft); color: var(--ok); border-color: transparent; }
.badge.warn { background: var(--danger-soft); color: var(--danger); border-color: transparent; }
.badge.solid { background: var(--primary); color: var(--primary-fg); border-color: transparent; }

/* Tabs across the top of a detail page. Links, so each tab is a URL. */
/* The underline is drawn inside the tab, so the row is exactly its own
   height and never grows a scrollbar; a narrow screen can still swipe it. */
.tabs {
  display: flex; gap: .25rem; border-bottom: 1px solid var(--border);
  margin: 1rem 0 1.5rem; overflow-x: auto; scrollbar-width: none;
}
.tabs::-webkit-scrollbar { display: none; }
.tabs a {
  padding: .5rem .8rem; color: var(--muted); font-size: .9rem; font-weight: 500;
  white-space: nowrap;
}
.tabs a:hover { color: var(--fg); text-decoration: none; }
.tabs a.active { color: var(--fg); box-shadow: inset 0 -2px 0 var(--primary); }

/* The strip above a page: where you are, and what you can do here. */
.crumbs { display: flex; gap: .4rem; align-items: center; color: var(--muted); font-size: .85rem; margin: 0 0 .5rem; }
.crumbs a { color: var(--muted); }
.crumbs span::before { content: "/"; margin-right: .4rem; opacity: .5; }
.crumbs span:first-child::before { content: none; }
.title-row { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: .25rem; }
.title-row .muted { margin: 0; }

/* A combobox: the menu hangs under the input and is exactly its width. */
.combo { position: relative; display: inline-block; width: 100%; max-width: 28rem; }
form.row .combo { width: auto; flex: 1 1 14rem; }
.combo input { width: 100%; }
.combo-menu {
  position: absolute; left: 0; right: 0; top: calc(100% + .25rem); z-index: 30;
  margin: 0; padding: .25rem; list-style: none;
  background: var(--card); border: 1px solid var(--border); border-radius: var(--radius);
  box-shadow: 0 8px 24px #0002; max-height: 16rem; overflow-y: auto;
}
.combo-menu[hidden] { display: none; }
.combo-menu li {
  display: flex; align-items: baseline; gap: .5rem; min-width: 0;
  padding: .4rem .6rem; border-radius: calc(var(--radius) - .15rem);
  cursor: pointer; font-size: .9rem;
}
.combo-menu li .value { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.combo-menu li .hint { color: var(--muted); font-size: .8rem; margin-left: auto; white-space: nowrap; }
.combo-menu li:hover, .combo-menu li[aria-selected="true"] { background: var(--soft); }
.combo-menu li.none { color: var(--muted); cursor: default; }
.combo-menu li.none:hover { background: none; }

/* Radios dressed as a segmented control, for a short fixed choice in a row form. */
.seg-item { display: inline-flex; align-items: center; gap: .3rem; padding: .35rem .6rem; font-size: .85rem; cursor: pointer; }
.seg-item + .seg-item { border-left: 1px solid var(--border); }
.seg-item:has(input:checked) { background: var(--soft); font-weight: 500; }
.seg-item input { margin: 0; }

/* A segmented control: two or three choices, one active. */
.seg { display: inline-flex; border: 1px solid var(--border); border-radius: calc(var(--radius) - .1rem); overflow: hidden; }
.seg button {
  border: 0; border-radius: 0; background: var(--card); color: var(--muted);
  padding: .3rem .7rem; font-size: .85rem; font-weight: 500;
}
.seg button + button { border-left: 1px solid var(--border); }
.seg button[aria-pressed="true"] { background: var(--soft); color: var(--fg); }
.seg button:hover { opacity: 1; background: var(--soft); }

/* The index has two views over one list: the same markup, restyled. Cards
   is the default; List packs each entry into one bordered row. */
/* Cards: a grid of tiles, icon on top. The stack is the markup either way;
   only the presentation changes with the toggle. */
.stack#list:not(.view-list) {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(11rem, 1fr)); gap: .75rem;
}
.stack#list:not(.view-list) .card { align-items: center; gap: .75rem; padding: .85rem 1rem; }
.stack#list:not(.view-list) .icon { width: 2.25rem; height: 2.25rem; }
.stack#list:not(.view-list) .meta { min-width: 0; flex: 1; }
.stack#list:not(.view-list) .meta .slug { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.stack.view-list { gap: 0; border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden; background: var(--card); }
.stack.view-list .card { border: 0; border-bottom: 1px solid var(--border); border-radius: 0; box-shadow: none; padding: .45rem .75rem; }
.stack.view-list li:last-child .card { border-bottom: 0; }
.stack.view-list .card:hover { background: var(--soft); }
.stack.view-list .icon { flex-basis: 1.6rem; width: 1.6rem; height: 1.6rem; border-radius: .35rem; }
.stack.view-list .icon-text { font-size: .95rem; }
.stack.view-list .meta { flex-direction: row; align-items: baseline; gap: .6rem; flex: 1; min-width: 0; }
.stack.view-list .meta .slug { margin-left: auto; white-space: nowrap; }

/* The app browser: one level at a time, as rows or as tiles. Both are in
   the page; the toggle shows one. */
.seg.icons button { display: inline-flex; align-items: center; padding: .35rem .55rem; }
#list.browse:not(.view-list) .list-only { display: none; }
#list.browse.view-list .cards-only { display: none; }
.tiles {
  list-style: none; margin: 0; padding: 0;
  display: grid; grid-template-columns: repeat(auto-fill, minmax(11rem, 1fr)); gap: .75rem;
}
.tile { position: relative; }
.tile .card { align-items: center; gap: .75rem; padding: .85rem 1rem; height: 100%; }
.tile .meta { min-width: 0; flex: 1; }
.tile .meta .slug { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.tile .row-tools { position: absolute; top: .35rem; right: .35rem; display: none; background: var(--card); border-radius: .4rem; }
.tile:hover .row-tools, .tile:focus-within .row-tools { display: inline-flex; }
.rows {
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); overflow: hidden;
}
.row {
  display: flex; align-items: center; gap: .6rem;
  padding: .45rem .75rem; border-bottom: 1px solid var(--border); min-height: 2.6rem;
}
.rows > :last-child, .rows > :last-child > summary.row { border-bottom: 0; }
.row:hover { background: var(--soft); }
.row .icon { flex: 0 0 1.6rem; width: 1.6rem; height: 1.6rem; border-radius: .35rem; }
.row .icon-text { font-size: .95rem; }
.row-name { color: var(--fg); font-weight: 500; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.row-meta { margin-left: auto; color: var(--muted); font-size: .8rem; white-space: nowrap; }
.row-tools { display: inline-flex; gap: .2rem; margin-left: .5rem; }
.row .row-tools { visibility: hidden; }
.row:hover .row-tools, .row:focus-within .row-tools { visibility: visible; }
@media (hover: none) {
  .row .row-tools { visibility: visible; }
  .tile .row-tools { display: inline-flex; }
}
details.project > summary { list-style: none; cursor: pointer; }
details.project > summary::-webkit-details-marker { display: none; }
.chev, .chev-space { flex: 0 0 1rem; width: 1rem; text-align: center; color: var(--muted); }
.chev::before { content: "\25B8"; display: inline-block; transition: transform .15s ease; }
details[open] > summary .chev::before { transform: rotate(90deg); }
details.project > .children { border-bottom: 1px solid var(--border); }
details.project > .children > .row, details.project > .children > details > summary.row { padding-left: 2.25rem; }
details.project > .children .children > .row, details.project > .children .children > details > summary.row { padding-left: 3.75rem; }
details.project > .children .children .children > .row, details.project > .children .children .children > details > summary.row { padding-left: 5.25rem; }
.children .empty-row { margin: 0; padding: .5rem 2.25rem; }
/* A project marker: the same box as an app's icon, muted, outline only. */
.icon.folder-icon { color: var(--muted); background: var(--soft); border-color: var(--border); }
.folder-icon .folder-open { display: none; }
details[open] > summary .folder-icon .folder-open { display: inline; }
details[open] > summary .folder-icon .folder-closed { display: none; }
@media (prefers-reduced-motion: reduce) { .chev::before { transition: none; } }
table.permissions select { padding: .2rem .5rem; font-size: .85rem; }
table.permissions tr.inherited td { color: var(--muted); }
table.permissions .add-row td { background: var(--soft); }
table.permissions .add-row form.row { margin: 0; }
form.inline { display: inline; margin: 0; }

/* Search above a list: narrows the page as you type, searches on Enter. */
form.search { margin: 0 0 1rem; }
form.search input[type=search] { margin: 0; }
.pager {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem;
  margin: .75rem 0 0; color: var(--muted); font-size: .85rem;
}
.pager .actions a.btn { font-size: .8rem; }

/* One-line result of the last action, set by the server on redirect. */
.flash {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem;
  padding: .6rem .9rem; margin: 0 0 1.25rem;
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); font-size: .9rem;
}
.flash.ok { border-color: color-mix(in srgb, var(--ok) 40%, var(--border)); background: var(--ok-soft); }
.flash.error { border-color: color-mix(in srgb, var(--danger) 40%, var(--border)); background: var(--danger-soft); }
.flash button { padding: .1rem .4rem; }

/* The browser's own dialog, dressed. */
dialog {
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); color: var(--fg); padding: 1.25rem 1.5rem; max-width: 26rem;
  box-shadow: 0 10px 40px #0003;
}
dialog::backdrop { background: #0006; }
dialog h3 { margin-bottom: .5rem; }
dialog p { color: var(--muted); font-size: .9rem; }
dialog .actions { justify-content: flex-end; margin-top: 1rem; }

/* A value to copy once: a token, a link. */
.secret {
  display: flex; align-items: center; gap: .5rem;
  background: var(--soft); border: 1px solid var(--border); border-radius: var(--radius);
  padding: .6rem .8rem; margin: 0 0 .75rem;
}
.secret code { flex: 1; background: none; padding: 0; overflow-wrap: anywhere; font-size: .85rem; }

/* Index header, kept for pages still on `ui::page`. */
.head {
  display: flex; align-items: flex-start; justify-content: space-between;
  gap: 1rem; margin-bottom: 1.5rem;
}
.head .muted { margin-bottom: 0; }
.nav { display: flex; gap: .5rem; align-items: center; }

/* The shell: a rail pinned to the viewport, content offset past it. Below
   52rem the same rail slides in as a drawer. The toggle is a checkbox, so a
   page that wants no script still gets a working drawer. */
.drawer-toggle { position: absolute; opacity: 0; pointer-events: none; }
.sidebar {
  position: fixed; top: 0; left: 0; bottom: 0; z-index: 20;
  width: 15rem; padding: 1.25rem .75rem; overflow-y: auto;
  display: flex; flex-direction: column; gap: .1rem;
  background: var(--card); border-right: 1px solid var(--border);
  font-size: .9rem;
}
.brand {
  display: flex; align-items: center; gap: .5rem;
  font-weight: 600; padding: .25rem .6rem; margin-bottom: 1rem; color: var(--fg);
}
.brand:hover { text-decoration: none; }
.brand .mark {
  width: 1.5rem; height: 1.5rem; border-radius: .4rem;
  background: var(--primary); color: var(--primary-fg);
  display: grid; place-items: center; font-size: .8rem;
}
.nav-group { margin-top: 1rem; }
.nav-group > .label {
  padding: 0 .6rem .3rem; font-size: .72rem; font-weight: 500;
  letter-spacing: .04em; text-transform: uppercase; color: var(--muted);
}
.sidebar a:not(.brand) {
  display: flex; align-items: center; justify-content: space-between; gap: .5rem;
  padding: .4rem .6rem; border-radius: .4rem;
  color: var(--fg); text-decoration: none;
}
.sidebar a:not(.brand):hover { background: var(--soft); }
.sidebar a.active { background: var(--soft); font-weight: 500; }
.sidebar .count { color: var(--muted); font-size: .75rem; }
.sidebar .spacer { margin-top: auto; padding-top: .75rem; border-top: 1px solid var(--border); }
.sidebar .who { padding: .25rem .6rem; color: var(--muted); font-size: .8rem; overflow: hidden; text-overflow: ellipsis; }
.sidebar a.who { display: block; color: var(--muted); white-space: nowrap; }
.shell { margin-left: 15rem; }
.main { max-width: 52rem; margin: 0 auto; min-width: 0; }
.main > h1 { margin-bottom: .25rem; }
.drawer-open { display: none; }
.scrim {
  position: fixed; inset: 0; z-index: 10; background: #0006;
  opacity: 0; pointer-events: none; transition: opacity .2s ease;
}

@media (max-width: 52rem) {
  body { padding: 1.5rem 1rem; }
  .shell { margin-left: 0; }
  .sidebar {
    width: min(16rem, 80vw);
    transform: translateX(-100%); transition: transform .2s ease;
  }
  .drawer-toggle:checked ~ .shell .sidebar { transform: none; }
  .drawer-toggle:checked ~ .scrim { opacity: 1; pointer-events: auto; }
  .drawer-open { display: inline-flex; margin-bottom: 1.25rem; }
}
@media (prefers-reduced-motion: reduce) { .sidebar, .scrim { transition: none; } }

/* Search box on the index. */
input[type=search] { width: 100%; margin-bottom: 1.25rem; }

/* The square beside a listed page. */
.icon {
  flex: 0 0 2.25rem; width: 2.25rem; height: 2.25rem;
  border-radius: .45rem; display: grid; place-items: center; overflow: hidden;
  background: var(--soft); border: 1px solid var(--border);
}
.icon img { width: 100%; height: 100%; object-fit: contain; }
.icon-text { font-size: 1.25rem; line-height: 1; border: none; background: none; }
.icon-gen {
  background: hsl(var(--h) 55% 45%); border-color: transparent; color: #fff;
  font-size: .8rem; font-weight: 600;
}

.meta { display: flex; flex-direction: column; min-width: 0; }
.meta .title { font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.meta .slug { color: var(--muted); font-size: .8rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.when::before { content: "\00b7"; margin: 0 .35rem; }
.empty, .no-match { color: var(--muted); text-align: center; padding: 2rem 0; }
.no-match { display: none; }
"#;

/// Behaviour every shell page gets: confirm dialogs for anything marked
/// `data-confirm`, copy buttons for anything marked `data-copy`, and a close
/// button on the flash. Small enough to read in one sitting, which is the
/// bar for not having a build step.
pub const SHELL_SCRIPT: &str = r#"
<script>
(() => {
  const dialog = document.getElementById('confirm');
  if (dialog) {
    let pending = null;
    document.querySelectorAll('form[data-confirm]').forEach((form) => {
      form.addEventListener('submit', (event) => {
        if (form.dataset.confirmed) return;
        event.preventDefault();
        pending = form;
        dialog.querySelector('h3').textContent = form.dataset.confirm;
        dialog.querySelector('p').textContent = form.dataset.confirmDetail || '';
        const go = dialog.querySelector('[data-go]');
        go.textContent = form.dataset.confirmLabel || 'Continue';
        go.className = form.dataset.confirmDanger ? 'danger' : '';
        dialog.showModal();
      });
    });
    dialog.querySelector('[data-go]').addEventListener('click', () => {
      if (!pending) return;
      pending.dataset.confirmed = '1';
      dialog.close();
      pending.requestSubmit();
    });
    dialog.querySelector('[data-cancel]').addEventListener('click', () => dialog.close());
  }
  document.querySelectorAll('[data-copy]').forEach((button) => {
    button.addEventListener('click', async () => {
      const text = document.getElementById(button.dataset.copy)?.textContent || '';
      try { await navigator.clipboard.writeText(text); } catch {}
      const was = button.textContent;
      button.textContent = 'Copied';
      setTimeout(() => { button.textContent = was; }, 1200);
    });
  });
  document.querySelectorAll('.flash [data-dismiss]').forEach((button) => {
    button.addEventListener('click', () => button.closest('.flash').remove());
  });
  // A button that opens a dialog, filling named fields from its data-fill-*.
  document.querySelectorAll('[data-dialog]').forEach((button) => {
    button.addEventListener('click', () => {
      const target = document.getElementById(button.dataset.dialog);
      if (!target) return;
      Object.keys(button.dataset).filter((k) => k.startsWith('fill') && k.length > 4).forEach((k) => {
        const name = k.charAt(4).toLowerCase() + k.slice(5);
        target.querySelectorAll('[name="' + name + '"]').forEach((f) => { f.value = button.dataset[k]; });
        target.querySelectorAll('[data-show="' + name + '"]').forEach((e) => { e.textContent = button.dataset[k]; });
      });
      target.showModal();
    });
  });
  document.querySelectorAll('dialog [data-close]').forEach((button) => {
    button.addEventListener('click', () => button.closest('dialog').close());
  });
  // A select that saves as soon as it changes.
  document.querySelectorAll('select[data-autosubmit]').forEach((select) => {
    select.addEventListener('change', () => select.form.requestSubmit());
  });
  // Rows opened in place are kept in the address, so a shared link opens the
  // same way. The server renders them open from ?open=.
  const rows = document.querySelectorAll('details[data-rel]');
  if (rows.length) {
    const keep = () => {
      const open = Array.from(document.querySelectorAll('details[data-rel][open]')).map((d) => d.dataset.rel);
      const url = new URL(location.href);
      if (open.length) url.searchParams.set('open', open.join(',')); else url.searchParams.delete('open');
      history.replaceState(null, '', url.pathname + url.search + url.hash);
    };
    rows.forEach((d) => d.addEventListener('toggle', keep));
  }
  // A combobox: the input fetches matches as the person types and shows
  // them in a menu under itself. Without script it is a text input.
  document.querySelectorAll('.combo').forEach((combo) => {
    const input = combo.querySelector('input[data-search]');
    const menu = combo.querySelector('.combo-menu');
    if (!input || !menu) return;
    let timer = null;
    let controller = null;
    let items = [];
    let active = -1;
    const close = () => {
      menu.hidden = true;
      menu.replaceChildren();
      input.setAttribute('aria-expanded', 'false');
      input.removeAttribute('aria-activedescendant');
      items = [];
      active = -1;
    };
    const pick = (value) => { input.value = value; close(); input.focus(); };
    const highlight = (index) => {
      active = index;
      items.forEach((li, i) => {
        li.setAttribute('aria-selected', i === index ? 'true' : 'false');
        if (i === index) {
          input.setAttribute('aria-activedescendant', li.id);
          li.scrollIntoView({ block: 'nearest' });
        }
      });
    };
    const show = (found, q) => {
      menu.replaceChildren();
      items = [];
      active = -1;
      if (!found.length) {
        const none = document.createElement('li');
        none.className = 'none';
        none.textContent = 'No matches for ' + q;
        menu.appendChild(none);
      }
      found.forEach((raw, i) => {
        const item = typeof raw === 'string' ? { value: raw } : raw;
        const li = document.createElement('li');
        li.id = menu.id + '-' + i;
        li.setAttribute('role', 'option');
        li.setAttribute('aria-selected', 'false');
        const value = document.createElement('span');
        value.className = 'value';
        value.textContent = item.value;
        li.appendChild(value);
        if (item.label && item.label !== item.value) {
          const hint = document.createElement('span');
          hint.className = 'hint';
          hint.textContent = item.label;
          li.appendChild(hint);
        }
        // mousedown, so the pick lands before the input blurs.
        li.addEventListener('mousedown', (event) => { event.preventDefault(); pick(item.value); });
        li.addEventListener('mousemove', () => highlight(i));
        menu.appendChild(li);
        items.push(li);
      });
      menu.hidden = false;
      input.setAttribute('aria-expanded', 'true');
    };
    input.addEventListener('input', () => {
      clearTimeout(timer);
      const q = input.value.trim();
      if (!q) { close(); return; }
      timer = setTimeout(async () => {
        if (controller) controller.abort();
        controller = new AbortController();
        try {
          // A search that depends on another field of the same form
          // sends that field's value along, named by data-search-with.
          let url = input.dataset.search + '?q=' + encodeURIComponent(q);
          const withField = input.dataset.searchWith;
          if (withField && input.form && input.form.elements[withField]) {
            url += '&' + encodeURIComponent(withField) + '=' + encodeURIComponent(input.form.elements[withField].value);
          }
          const res = await fetch(url, {
            credentials: 'same-origin', signal: controller.signal,
          });
          if (!res.ok) return;
          show(await res.json(), q);
        } catch {}
      }, 150);
    });
    input.addEventListener('keydown', (event) => {
      if (menu.hidden) return;
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        if (items.length) highlight((active + 1) % items.length);
      } else if (event.key === 'ArrowUp') {
        event.preventDefault();
        if (items.length) highlight((active - 1 + items.length) % items.length);
      } else if (event.key === 'Enter') {
        // The menu is open: Enter chooses, it does not submit.
        event.preventDefault();
        if (active >= 0) pick(items[active].querySelector('.value').textContent);
        else if (items.length === 1) pick(items[0].querySelector('.value').textContent);
        else close();
      } else if (event.key === 'Escape') {
        event.preventDefault();
        close();
      } else if (event.key === 'Tab') {
        close();
      }
    });
    document.addEventListener('mousedown', (event) => {
      if (!combo.contains(event.target)) close();
    });
  });
})();
</script>
"#;

/// Narrows a list as the reader types: the element `#list`'s children are
/// shown or hidden by their `data-slug` and `data-title`, and `#no-match`
/// appears when nothing is left. Shared by the index and the apps list.
pub const FILTER_SCRIPT: &str = r#"
<script>
(() => {
  const input = document.getElementById('q');
  const list = document.getElementById('list');
  const noMatch = document.getElementById('no-match');
  if (input && list) {
    const items = Array.from(list.querySelectorAll('[data-slug]'));
    input.addEventListener('input', () => {
      const q = input.value.trim().toLowerCase();
      let visible = 0;
      items.forEach((item) => {
        const match = (item.dataset.slug + ' ' + (item.dataset.title || '')).includes(q);
        item.style.display = match ? '' : 'none';
        if (match) visible++;
      });
      if (noMatch) noMatch.style.display = (items.length > 0 && visible === 0 && q !== '') ? 'block' : 'none';
    });
  }
  // Cards or rows: remembered per browser, never sent anywhere.
  const buttons = Array.from(document.querySelectorAll('[data-view]'));
  if (list && buttons.length) {
    const key = 'toolsite.view';
    const apply = (view) => {
      list.classList.toggle('view-list', view === 'list');
      buttons.forEach((b) => b.setAttribute('aria-pressed', b.dataset.view === view ? 'true' : 'false'));
    };
    let saved = 'cards';
    try { saved = localStorage.getItem(key) || 'cards'; } catch {}
    apply(saved);
    buttons.forEach((b) => b.addEventListener('click', () => {
      apply(b.dataset.view);
      try { localStorage.setItem(key, b.dataset.view); } catch {}
    }));
  }
})();
</script>
"#;

/// A full document. `script` is emitted verbatim at the end of the body, so
/// callers keep control of anything interactive.
pub fn page(title: &str, body: Markup, script: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                div."container" { (body) }
                @if let Some(script) = script {
                    (PreEscaped(script))
                }
            }
        }
    }
}

/// A two-column page: navigation on the left, content on the right. Below
/// 52rem the sidebar becomes a drawer behind a `\u{2630}` toggle. The confirm
/// dialog and the shell script come with it, so a page only has to mark a
/// form `data-confirm` to get a real confirmation.
pub fn shell(title: &str, sidebar: Markup, body: Markup, script: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                input."drawer-toggle" id="drawer" type="checkbox";
                label."scrim" for="drawer" {}
                div."shell" {
                    aside."sidebar" { (sidebar) }
                    main."main" {
                        label."drawer-open btn quiet" for="drawer" { "\u{2630} Menu" }
                        (body)
                    }
                }
                dialog id="confirm" {
                    h3 {}
                    p {}
                    div."actions" {
                        button."quiet" type="button" data-cancel { "Cancel" }
                        button type="button" data-go { "Continue" }
                    }
                }
                (PreEscaped(SHELL_SCRIPT))
                @if let Some(script) = script {
                    (PreEscaped(script))
                }
            }
        }
    }
}

/// A small form centred in the viewport: sign in, choose a password.
pub fn form_page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
                style {
                    (PreEscaped(
                        "body { display: grid; place-items: center; min-height: 100vh; padding: 1rem; }"
                    ))
                }
            }
            body { div."container narrow" { (body) } }
        }
    }
}

// --- pieces pages are built from ------------------------------------------

/// The outcome of the last action, carried across a redirect.
#[derive(Debug, Clone, PartialEq)]
pub struct Flash {
    pub ok: bool,
    pub text: String,
}

pub fn flash(flash: Option<&Flash>) -> Markup {
    html! {
        @if let Some(flash) = flash {
            div."flash"."ok"[flash.ok]."error"[!flash.ok] role="status" {
                span { (flash.text) }
                button."ghost sm" type="button" data-dismiss aria-label="Dismiss" { "\u{2715}" }
            }
        }
    }
}

/// A panel: heading, optional description, body.
pub fn panel(title: &str, description: Option<&str>, body: Markup) -> Markup {
    html! {
        section."panel" {
            div."panel-head" {
                h3 { (title) }
                @if let Some(description) = description { p { (description) } }
            }
            div."panel-body" { (body) }
        }
    }
}

/// A row of links across the top of a detail page; `active` names the one
/// the reader is on.
pub fn tabs(items: &[(&str, &str, &str)], active: &str) -> Markup {
    html! {
        nav."tabs" {
            @for (key, label, href) in items {
                a."active"[*key == active] href=(href) { (label) }
            }
        }
    }
}

/// A text input that suggests matches from `search_url` (`?q=` appended) as
/// the person types, through the shared shell script. It submits whatever
/// was typed, so it works with no script, and it never renders the whole
/// set of apps or accounts into the page the way a `<select>` would.
/// A text input with a menu of matches from `search_url?q=` under it, the
/// width of the input, picked with the keyboard or the mouse. Without
/// script it is a text input that submits whatever was typed.
pub fn combobox(name: &str, search_url: &str, placeholder: &str) -> Markup {
    combobox_prefilled(name, search_url, placeholder, "", None)
}

/// The same control with a starting value, and optionally the name of
/// another field in the same form whose value is sent along with every
/// search (`with`), for a search that depends on a choice made above it.
pub fn combobox_prefilled(
    name: &str,
    search_url: &str,
    placeholder: &str,
    value: &str,
    with: Option<&str>,
) -> Markup {
    combobox_full(name, search_url, placeholder, value, with, "")
}

/// Several comboboxes of one name on a page need an `id_suffix` each, so
/// their menus do not share an id.
pub fn combobox_full(
    name: &str,
    search_url: &str,
    placeholder: &str,
    value: &str,
    with: Option<&str>,
    id_suffix: &str,
) -> Markup {
    let menu_id = format!("{name}{id_suffix}-matches");
    html! {
        div."combo" {
            input name=(name) placeholder=(placeholder) data-search=(search_url)
                  data-search-with=[with] value=(value)
                  autocomplete="off" required
                  role="combobox" aria-autocomplete="list" aria-expanded="false"
                  aria-controls=(menu_id) aria-haspopup="listbox";
            ul."combo-menu" id=(menu_id) role="listbox" hidden {}
        }
    }
}

/// A value shown once, with a button to copy it.
pub fn secret(id: &str, value: &str) -> Markup {
    html! {
        div."secret" {
            code id=(id) { (value) }
            button."quiet sm" type="button" data-copy=(id) { "Copy" }
        }
    }
}
