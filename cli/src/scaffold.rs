//! `toolsite init` — writes a project already configured for the one thing
//! that is easy to get wrong: apps are served from `/p/<slug>/`, never the
//! domain root.

use crate::react;
use anyhow::{bail, Result};
use std::path::Path;

/// Vendored so a scaffolded handler compiles without the server checkout.
const WIT: &str = include_str!("../../wit/toolsite.wit");

pub fn init(name: &str, spa: bool, handler: bool, react: bool) -> Result<()> {
    let root = Path::new(name);
    if root.exists() {
        bail!("{name} already exists");
    }
    // A React app routes on the client, so it wants the spa fallback whether
    // or not anybody remembered to ask for it.
    let spa = spa || react;

    std::fs::create_dir_all(root)?;
    std::fs::write(root.join("toolsite.toml"), manifest(name, spa, handler))?;
    if react {
        react::write(root, name)?;
    } else {
        std::fs::create_dir_all(root.join("dist"))?;
        std::fs::write(root.join("dist/index.html"), index_html(name, handler))?;
    }

    std::fs::write(
        root.join("NOTES.md"),
        format!(
            "# {name}\n\n\
             Written for whoever works on this next — a published app is a rendered\n\
             page, and its source does not come back out of it.\n\n\
             ## Schema\n\n\
             See migrations/. Add a numbered file for each change.\n\n\
             ## Decisions\n\n\
             ## Unfinished\n"
        ),
    )?;

    if handler {
        write_handler(root, name)?;
        std::fs::create_dir_all(root.join("migrations"))?;
        std::fs::write(
            root.join("migrations/001_initial.sql"),
            "-- Numbered, applied in order, each exactly once. Add a file for\n             -- the next change rather than editing this one: databases that\n             -- already ran it will never run it again.\n\n             create table visits (\n    at integer not null\n);\n",
        )?;
    }

    println!("created {name}/");
    if react {
        println!("  src/App.tsx          the app; Tailwind classes work already");
        println!("  vite.config.ts       base is /p/{name}/, which is the part that breaks");
    } else {
        println!("  dist/index.html      the page, ready to deploy as-is");
    }
    if handler {
        println!("  handler/             server-side code, gets its own database");
    }
    if handler {
        println!("  migrations/          the app's schema, applied on deploy");
    }
    println!("  toolsite.toml        gate, routes and jobs");
    println!("  NOTES.md             what the next session needs to know");
    println!();
    // deploy installs and builds; saying otherwise makes this look dearer
    // than hand-writing a page, which is how that decision gets made wrong.
    println!("Next: cd {name} && toolsite deploy");
    if react {
        println!("       (deploy runs npm install and npm run build for you)");
    }
    Ok(())
}

/// What the app needs, in one file that travels with the source. `deploy`
/// applies it, so this is the thing to edit rather than remembering commands.
fn manifest(name: &str, spa: bool, handler: bool) -> String {
    let jobs = if handler {
        "\n# Work this app does on its own. Six cron fields, seconds first.\n\
         # [[job]]\n# name = \"refresh\"\n# schedule = \"0 */5 * * * *\"\n\
         # path = \"/api/refresh\"\n"
    } else {
        ""
    };
    format!(
        "slug = \"{name}\"\n\
         spa = {spa}\n\
         \n\
         # public, authenticated, or granted.\n\
         gate = \"public\"\n\
         \n\
         # Guard part of the app. Longest matching prefix wins.\n\
         # [[route]]\n# path = \"/admin\"\n# gate = \"granted\"\n\
         {jobs}"
    )
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
