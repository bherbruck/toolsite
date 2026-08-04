//! The Vite + React + Tailwind scaffold, which exists because of what agents
//! actually do when left to choose.
//!
//! Given a shell with node in it, a model will still hand-write a single
//! `index.html` with a `<script>` block and inline styles, then spend the rest
//! of the session maintaining it by string replacement. Not because it is
//! better — because it is the path with no setup step, and the setup step is
//! where a decision gets made.
//!
//! So the setup step is removed. `init --react` writes a project that builds
//! unmodified, with `base` already `/p/<slug>/` — the one thing that is easy
//! to get wrong and produces a blank page rather than an error.

use anyhow::Result;
use std::path::Path;

pub fn write(root: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(root.join("src"))?;

    std::fs::write(root.join("package.json"), package_json(name))?;
    std::fs::write(root.join("vite.config.ts"), vite_config(name))?;
    std::fs::write(root.join("tsconfig.json"), TSCONFIG)?;
    std::fs::write(root.join("index.html"), index_html(name))?;
    std::fs::write(root.join("src/main.tsx"), MAIN_TSX)?;
    std::fs::write(root.join("src/App.tsx"), app_tsx(name))?;
    std::fs::write(root.join("src/index.css"), INDEX_CSS)?;
    std::fs::write(root.join(".gitignore"), "node_modules/\ndist/\n")?;
    Ok(())
}

/// Caret ranges, which float within a major and not past it. That is the
/// whole hazard: this scaffold was written with vite ^7 on the day vite 8 was
/// latest, installed cleanly, built cleanly, and quietly handed everyone a
/// toolchain a major behind. `npm install` succeeding proves nothing about
/// currency, so the ignored test compares these against the registry.
fn package_json(name: &str) -> String {
    format!(
        r#"{{
  "name": "{name}",
  "private": true,
  "type": "module",
  "scripts": {{
    "dev": "vite",
    "build": "vite build",
    "preview": "vite preview"
  }},
  "dependencies": {{
    "react": "^19.0.0",
    "react-dom": "^19.0.0"
  }},
  "devDependencies": {{
    "@tailwindcss/vite": "^4.0.0",
    "@types/react": "^19.0.0",
    "@types/react-dom": "^19.0.0",
    "@vitejs/plugin-react": "^6.0.0",
    "tailwindcss": "^4.0.0",
    "typescript": "^7.0.0",
    "vite": "^8.0.0"
  }}
}}
"#
    )
}

/// The whole reason this file exists. An app is served from `/p/<slug>/`, so
/// a default Vite build asks for `/assets/…` at the domain root, gets the
/// index page back, and renders nothing. It looks like a success: 200s all
/// round, blank screen.
fn vite_config(name: &str) -> String {
    format!(
        r#"import {{ defineConfig }} from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({{
  // Served from a subpath, never the domain root. Without this the build
  // loads and renders blank, because every asset 404s.
  base: '/p/{name}/',
  plugins: [react(), tailwindcss()],
}})
"#
    )
}

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "lib": ["ES2022", "DOM", "DOM.Iterable"],
    "module": "ESNext",
    "moduleResolution": "bundler",
    "jsx": "react-jsx",
    "strict": true,
    "skipLibCheck": true,
    "noEmit": true,
    "types": ["vite/client"]
  },
  "include": ["src"]
}
"#;

fn index_html(name: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>{name}</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
"#
    )
}

const MAIN_TSX: &str = r#"import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App'
import './index.css'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
"#;

/// Tailwind 4 is one import; there is no config file to write and no content
/// globs to keep correct.
const INDEX_CSS: &str = "@import \"tailwindcss\";\n";

/// Fetches from the handler, because an app that needs a server is the reason
/// to be here at all, and a relative URL is the correct one under `/p/<slug>/`.
fn app_tsx(name: &str) -> String {
    format!(
        r#"import {{ useEffect, useState }} from 'react'

export default function App() {{
  const [greeting, setGreeting] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {{
    // Relative on purpose: the app is mounted at /p/{name}/, so 'api/hello'
    // resolves against it. A leading slash would escape to the domain root.
    fetch('api/hello')
      .then((res) => (res.ok ? res.text() : Promise.reject(new Error(`HTTP ${{res.status}}`))))
      .then(setGreeting)
      .catch((e) => setError(String(e)))
  }}, [])

  return (
    <main className="mx-auto max-w-2xl px-6 py-16">
      <h1 className="text-3xl font-semibold tracking-tight">{name}</h1>
      <p className="mt-2 text-sm text-neutral-500">
        Edit <code className="rounded bg-neutral-500/10 px-1 py-0.5">src/App.tsx</code>, then{{' '}}
        <code className="rounded bg-neutral-500/10 px-1 py-0.5">npm run build &amp;&amp; toolsite deploy</code>.
      </p>

      <div className="mt-8 rounded-lg border border-neutral-500/20 p-4">
        <div className="text-xs uppercase tracking-wide text-neutral-500">from the handler</div>
        <div className="mt-1 font-mono text-sm">
          {{error ? <span className="text-red-500">{{error}}</span> : (greeting ?? 'loading…')}}
        </div>
      </div>
    </main>
  )
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scaffold(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        write(&root, name).unwrap();
        dir
    }

    #[test]
    fn the_base_path_is_set_before_anyone_has_to_learn_why() {
        let dir = scaffold("dash");
        let config = std::fs::read_to_string(dir.path().join("dash/vite.config.ts")).unwrap();
        assert!(
            config.contains("base: '/p/dash/'"),
            "a default base publishes a blank page: {config}"
        );
    }

    #[test]
    fn nothing_reaches_for_the_domain_root() {
        // An absolute URL in a fetch or an import escapes the app's subpath
        // and lands on the site index, which answers 200 with the wrong body.
        let dir = scaffold("dash");
        let app = std::fs::read_to_string(dir.path().join("dash/src/App.tsx")).unwrap();
        assert!(app.contains("fetch('api/hello')"), "{app}");
        assert!(!app.contains("fetch('/api"), "an absolute path leaves the app: {app}");
    }

    /// Every dependency's major must still be the registry's latest.
    ///
    /// A caret range floats inside a major and never past it, so a scaffold
    /// keeps installing perfectly while falling further behind. That is not
    /// hypothetical: this file shipped with vite ^7 while vite 8 was latest,
    /// and the build test passed, because building is not the property that
    /// broke.
    #[test]
    #[ignore = "asks the npm registry what is current"]
    fn no_dependency_is_a_major_behind() {
        let dir = scaffold("dash");
        let text = std::fs::read_to_string(dir.path().join("dash/package.json")).unwrap();
        let package: serde_json::Value = serde_json::from_str(&text).unwrap();

        let mut behind = Vec::new();
        for section in ["dependencies", "devDependencies"] {
            for (name, range) in package[section].as_object().unwrap() {
                let output = std::process::Command::new("npm")
                    .args(["view", name, "version"])
                    .output()
                    .expect("npm is not on PATH");
                let latest = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let ours = range.as_str().unwrap().trim_start_matches('^');

                let major = |v: &str| v.split('.').next().unwrap_or_default().to_string();
                if major(&latest) != major(ours) {
                    behind.push(format!("{name}: scaffold has {ours}, registry has {latest}"));
                }
            }
        }
        assert!(behind.is_empty(), "the scaffold is stale:\n  {}", behind.join("\n  "));
    }

    /// Needs npm and the network, so it is ignored by default: `cargo test --
    /// --ignored`. It is the only thing that can catch a dependency range
    /// that has stopped resolving, which is how a scaffold rots.
    #[test]
    #[ignore = "runs npm install against the registry"]
    fn the_scaffold_installs_and_builds_as_written() {
        let dir = scaffold("dash");
        let root = dir.path().join("dash");

        for (program, args) in [("npm", vec!["install"]), ("npm", vec!["run", "build"])] {
            let output = std::process::Command::new(program)
                .args(&args)
                .current_dir(&root)
                .output()
                .expect("npm is not on PATH");
            assert!(
                output.status.success(),
                "{program} {args:?} failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let index = std::fs::read_to_string(root.join("dist/index.html")).unwrap();
        assert!(index.contains("/p/dash/assets/"), "built with the wrong base: {index}");

        // Tailwind ran, rather than the class names being decorative.
        let css_dir = std::fs::read_dir(root.join("dist/assets")).unwrap();
        let css: String = css_dir
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()? == "css").then(|| std::fs::read_to_string(path).ok())?
            })
            .collect();
        assert!(css.contains("max-w-2xl"), "tailwind produced no utilities");
    }
}
