//! The look of the pages toolsite serves itself — the index, sign-in, and
//! admin. Published apps are not styled from here; they bring their own.
//!
//! One set of tokens and one set of element rules, so a page added later
//! inherits the theme instead of growing its own. Colours are defined once
//! and flipped by `prefers-color-scheme`; nothing below hard-codes a colour.

use maud::{html, Markup, PreEscaped, DOCTYPE};

/// Design tokens plus the element and component rules every page shares.
pub const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f7f8;
  --fg: #1a1a1a;
  --muted: #6b7280;
  --card: #ffffff;
  --border: #e5e7eb;
  --accent: #4f46e5;
  --danger: #b91c1c;
  --radius: .5rem;
  --gap: .75rem;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111114;
    --fg: #e8e8ea;
    --muted: #9198a1;
    --card: #1a1a1f;
    --border: #2a2a30;
    --accent: #818cf8;
    --danger: #ef4444;
  }
}

* { box-sizing: border-box; }
body {
  margin: 0;
  padding: 3rem 1.5rem;
  background: var(--bg);
  color: var(--fg);
  font: 16px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}
.container { max-width: 44rem; margin: 0 auto; }
.narrow { max-width: 22rem; }

h1 { font-size: 1.4rem; margin: 0 0 .25rem; }
h2 { font-size: 1rem; margin: 2rem 0 .5rem; letter-spacing: .01em; }
a { color: var(--accent); }
.muted { color: var(--muted); font-size: .9rem; margin: 0 0 1.5rem; }
code {
  background: color-mix(in srgb, var(--muted) 18%, transparent);
  padding: .05rem .3rem; border-radius: .25rem; font-size: .85em;
}

/* Anything that sits on the background as its own block. */
.card {
  display: flex; align-items: center; gap: var(--gap);
  padding: .7rem .9rem;
  border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--card); color: var(--fg);
  text-decoration: none; font-size: .95rem;
  transition: border-color .15s ease;
}
a.card:hover { border-color: var(--accent); }
a.card::after { content: "\2192"; color: var(--muted); margin-left: auto; }

.stack { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .5rem; }

input, select, button {
  font: inherit; font-size: .95rem;
  padding: .5rem .65rem;
  border-radius: .4rem;
  border: 1px solid var(--border);
  background: var(--card); color: var(--fg);
}
input[type=checkbox] { width: auto; padding: 0; }
input:focus, select:focus { outline: 2px solid var(--accent); outline-offset: 1px; }
input[readonly] { color: var(--muted); }
button {
  border: 0; background: var(--accent); color: #fff; cursor: pointer;
  padding: .5rem 1rem;
}
button.danger { background: var(--danger); }
button.quiet { background: transparent; color: var(--muted); border: 1px solid var(--border); }

form.row { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; margin: .75rem 0; }
form.column { display: flex; flex-direction: column; gap: var(--gap); }
label { display: inline-flex; align-items: center; gap: .35rem; font-size: .9rem; }

table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: .45rem .5rem; border-bottom: 1px solid var(--border); }
th { font-size: .8rem; font-weight: 600; color: var(--muted); text-transform: uppercase; letter-spacing: .04em; }
td form { margin: 0; }

/* Search box on the index. */
input[type=search] { width: 100%; margin-bottom: 1.25rem; }

/* The square beside a listed page. */
.icon {
  flex: 0 0 2.25rem; width: 2.25rem; height: 2.25rem;
  border-radius: .45rem; display: grid; place-items: center; overflow: hidden;
  background: var(--bg); border: 1px solid var(--border);
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
