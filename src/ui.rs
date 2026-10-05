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
.tabs {
  display: flex; gap: .25rem; border-bottom: 1px solid var(--border);
  margin: 1rem 0 1.5rem; overflow-x: auto;
}
.tabs a {
  padding: .5rem .8rem; color: var(--muted); font-size: .9rem; font-weight: 500;
  border-bottom: 2px solid transparent; margin-bottom: -1px; white-space: nowrap;
}
.tabs a:hover { color: var(--fg); text-decoration: none; }
.tabs a.active { color: var(--fg); border-bottom-color: var(--primary); }

/* The strip above a page: where you are, and what you can do here. */
.crumbs { display: flex; gap: .4rem; align-items: center; color: var(--muted); font-size: .85rem; margin: 0 0 .5rem; }
.crumbs a { color: var(--muted); }
.crumbs span::before { content: "/"; margin-right: .4rem; opacity: .5; }
.crumbs span:first-child::before { content: none; }
.title-row { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: .25rem; }
.title-row .muted { margin: 0; }

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

/// A value shown once, with a button to copy it.
pub fn secret(id: &str, value: &str) -> Markup {
    html! {
        div."secret" {
            code id=(id) { (value) }
            button."quiet sm" type="button" data-copy=(id) { "Copy" }
        }
    }
}
