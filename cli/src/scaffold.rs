//! `toolsite init` — writes a project already configured for the one thing
//! that is easy to get wrong: apps are served from `/p/<slug>/`, never the
//! domain root.

use anyhow::{bail, Result};
use std::path::Path;

/// Vendored so a scaffolded handler compiles without the server checkout.
const WIT: &str = include_str!("../../wit/toolsite.wit");

pub fn init(name: &str, spa: bool, handler: bool) -> Result<()> {
    let root = Path::new(name);
    if root.exists() {
        bail!("{name} already exists");
    }
    std::fs::create_dir_all(root.join("dist"))?;

    std::fs::write(root.join("toolsite.toml"), format!("slug = \"{name}\"\nspa = {spa}\n"))?;
    std::fs::write(root.join("dist/index.html"), index_html(name, handler))?;

    if handler {
        write_handler(root, name)?;
    }

    println!("created {name}/");
    println!("  dist/index.html      the page, ready to deploy as-is");
    if handler {
        println!("  handler/             server-side code, gets its own database");
    }
    println!("  toolsite.toml        slug and routing mode");
    println!();
    println!("Next: cd {name} && toolsite deploy");
    if spa {
        println!();
        println!("Building with Vite? Set base: '/p/{name}/' — assets 404 without it.");
    }
    Ok(())
}

fn index_html(name: &str, handler: bool) -> String {
    let demo = if handler {
        r#"
<p id="out">loading…</p>
<script type="module">
  // Same-origin: the app is mounted at /p/<slug>/, so a relative URL is right.
  const res = await fetch('api/hello');
  document.getElementById('out').textContent = await res.text();
</script>"#
    } else {
        "\n<p>Edit dist/index.html and run <code>toolsite deploy</code>.</p>"
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name}</title>
<style>
  body {{ font: 16px/1.6 system-ui, sans-serif; max-width: 34rem;
         margin: 4rem auto; padding: 0 1rem; color-scheme: light dark; }}
</style>
</head>
<body>
<h1>{name}</h1>{demo}
</body>
</html>
"#
    )
}

fn write_handler(root: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(root.join("handler/src"))?;
    std::fs::create_dir_all(root.join("handler/wit"))?;
    std::fs::write(root.join("handler/wit/toolsite.wit"), WIT)?;

    std::fs::write(
        root.join("handler/Cargo.toml"),
        format!(
            r#"# Built for wasm32-wasip2, so it must stay out of any host workspace.
[workspace]

[package]
name = "{name}-handler"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.51"

[profile.release]
opt-level = "s"
strip = true
"#
        ),
    )?;

    std::fs::write(
        root.join("handler/src/lib.rs"),
        r#"wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use toolsite::app::db;

struct Handler;

fn text(status: u16, body: impl Into<String>) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        body: body.into().into_bytes(),
    }
}

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        // The host passes the path relative to this app, with /api still on
        // it, so strip the prefix the way any router would.
        let route = req.path.strip_prefix("/api").unwrap_or(&req.path);

        match (req.method.as_str(), route) {
            ("GET", "/hello") => {
                // State must live in the database: every request gets a fresh
                // instance, so globals do not survive.
                if let Err(e) = db::query(
                    "create table if not exists visits (at integer)",
                    &[],
                ) {
                    return text(500, format!("{e:?}"));
                }
                if let Err(e) = db::query("insert into visits values (0)", &[]) {
                    return text(500, format!("{e:?}"));
                }
                match db::query("select count(*) from visits", &[]) {
                    Ok(rows) => match rows.values.first().and_then(|r| r.first()) {
                        Some(db::Value::Integer(n)) => text(200, format!("visit #{n}")),
                        _ => text(500, "unexpected shape"),
                    },
                    Err(e) => text(500, format!("{e:?}")),
                }
            }
            _ => text(404, "not found"),
        }
    }
}

export!(Handler);
"#,
    )?;
    Ok(())
}
