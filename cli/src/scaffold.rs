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
        include_str!("../../templates/handler/Cargo.toml").replace("NAME", name),
    )?;

    std::fs::write(
        root.join("handler/src/lib.rs"),
        include_str!("../../templates/handler/src/lib.rs"),
    )?;
    Ok(())
}
