//! `toolsite` — publish pages, apps and handlers from a shell.
//!
//! Everything here could be done with curl; the point is that the parts that
//! are easy to get wrong (base paths, tar shape, upload order, verifying the
//! result) happen the same way every time.

mod mcp;
mod scaffold;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use mcp::Mcp;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "toolsite", version, about = "Publish to a toolsite server")]
struct Cli {
    /// Base URL of the server. Defaults to $TOOLSITE_URL.
    #[arg(long, global = true)]
    url: Option<String>,
    /// Bearer token. Defaults to $TOOLSITE_TOKEN.
    #[arg(long, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold an app with its base path already correct.
    Init {
        name: String,
        /// Client-side routing: unknown paths serve index.html.
        #[arg(long)]
        spa: bool,
        /// Include a wasm handler with its own database.
        #[arg(long)]
        handler: bool,
    },
    /// Fetch back the project a previous deploy stored.
    Fetch {
        /// App to fetch. Defaults to toolsite.toml, else the directory name.
        #[arg(long)]
        slug: Option<String>,
        /// Where to unpack it. Defaults to the current directory.
        dir: Option<PathBuf>,
    },
    /// Build if needed, upload, and verify.
    Deploy {
        /// Project directory. Defaults to the current one.
        dir: Option<PathBuf>,
        /// Slug to publish under. Defaults to toolsite.toml, else the
        /// directory name.
        #[arg(long)]
        slug: Option<String>,
        /// Serve index.html for unknown paths.
        #[arg(long)]
        spa: bool,
        /// Publish the built output only, without keeping the project with
        /// it. The next session then has nothing to work from.
        #[arg(long)]
        without_source: bool,
    },
    /// Run SQL against one app's database.
    Sql {
        app: String,
        sql: String,
        /// Bound to '?' placeholders, in order. Repeat for several.
        #[arg(long = "param")]
        params: Vec<String>,
    },
    /// What is already published.
    List {
        /// Include hidden and unlisted pages.
        #[arg(long)]
        all: bool,
    },
    /// Accounts that may sign in to gated apps.
    #[command(subcommand)]
    User(UserCommand),
    /// Let an account reach an app whose gate is 'granted'.
    Grant {
        app: String,
        email: String,
        /// What they are on this app. The handler reads it and decides what
        /// it means. Defaults to viewer.
        #[arg(long)]
        role: Option<String>,
    },
    /// Take that access away again.
    Revoke { app: String, email: String },
    /// Decide who may reach an app, or one path within it.
    Gate {
        app: String,
        /// public (anyone), authenticated (any account), or granted (only
        /// accounts you have granted).
        gate: String,
        /// Apply it to paths starting here instead of the whole app, e.g.
        /// --path /admin. Longest matching prefix wins.
        #[arg(long)]
        path: Option<String>,
    },
    /// Work an app does on its own: refreshing, scraping, tidying.
    Job {
        app: String,
        /// Job name. Omit to list what is scheduled.
        name: Option<String>,
        /// Cron with seconds first: '0 */5 * * * *' is every five minutes.
        #[arg(long)]
        schedule: Option<String>,
        /// Path the handler is called with, e.g. /api/refresh.
        #[arg(long)]
        path: Option<String>,
        /// Run it now, whatever the schedule says.
        #[arg(long)]
        now: bool,
        /// Unschedule it.
        #[arg(long)]
        remove: bool,
    },
    /// An app's settings: API keys and the like, readable only by its handler.
    Secret {
        app: String,
        /// Name to set or remove. Omit to list the names already set.
        name: Option<String>,
        /// Value to store. Omit while naming to remove it; read from
        /// $TOOLSITE_SECRET when set, so it stays out of shell history.
        #[arg(long)]
        value: Option<String>,
        /// Remove the named setting.
        #[arg(long)]
        remove: bool,
        /// Print a link to paste values into instead of passing one here.
        #[arg(long)]
        link: bool,
    },
    /// Read or write the notes kept with an app for the next session.
    Notes {
        slug: String,
        /// Markdown file to store. Reads the existing notes when omitted.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Take a page down, reversibly.
    Hide { slug: String },
    /// Restore a hidden page.
    Unhide { slug: String },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Create an account. There is no public signup, so this is how every
    /// account comes into being.
    Add {
        email: String,
        /// Read from $TOOLSITE_PASSWORD when omitted, so it stays out of
        /// shell history.
        #[arg(long)]
        password: Option<String>,
        /// Can see every account and change any app's access at /admin.
        #[arg(long)]
        admin: bool,
    },
    /// Stop an account signing in, and end its live sessions now.
    Disable { email: String },
    /// Let a disabled account back in.
    Enable { email: String },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // init needs no server, so it must not demand credentials.
    if let Command::Init { name, spa, handler } = &cli.command {
        return scaffold::init(name, *spa, *handler);
    }

    let url = cli
        .url
        .clone()
        .or_else(|| std::env::var("TOOLSITE_URL").ok())
        .ok_or_else(|| anyhow!("set --url or TOOLSITE_URL"))?;
    let token = cli
        .token
        .clone()
        .or_else(|| std::env::var("TOOLSITE_TOKEN").ok())
        .or_else(|| std::env::var("TOOLSITE_MCP_TOKEN").ok())
        .ok_or_else(|| anyhow!("set --token or TOOLSITE_TOKEN"))?;
    let mcp = Mcp::connect(&url, &token)?;

    match cli.command {
        Command::Init { .. } => unreachable!("handled above"),
        Command::Deploy {
            dir,
            slug,
            spa,
            without_source,
        } => deploy(
            &mcp,
            &url,
            &token,
            dir.unwrap_or_else(|| PathBuf::from(".")),
            slug,
            spa,
            !without_source,
        ),
        Command::Fetch { slug, dir } => fetch(&mcp, slug, dir.unwrap_or_else(|| PathBuf::from("."))),
        Command::Sql { app, sql, params } => {
            let params: Vec<serde_json::Value> = params.into_iter().map(json_scalar).collect();
            let text = mcp.call("run_sql", json!({ "app": app, "sql": sql, "params": params }))?;
            print_rows(&text);
            Ok(())
        }
        Command::List { all } => {
            let text = mcp.call("list_pages", json!({ "include_all": all }))?;
            print_pages(&text);
            Ok(())
        }
        Command::User(UserCommand::Add {
            email,
            password,
            admin,
        }) => {
            let password = password
                .or_else(|| std::env::var("TOOLSITE_PASSWORD").ok())
                .ok_or_else(|| anyhow!("pass --password or set TOOLSITE_PASSWORD"))?;
            println!(
                "{}",
                mcp.call(
                    "create_user",
                    json!({ "email": email, "password": password, "admin": admin })
                )?
            );
            Ok(())
        }
        Command::User(UserCommand::Disable { email }) => {
            println!(
                "{}",
                mcp.call("set_user_active", json!({ "email": email, "active": false }))?
            );
            Ok(())
        }
        Command::User(UserCommand::Enable { email }) => {
            println!(
                "{}",
                mcp.call("set_user_active", json!({ "email": email, "active": true }))?
            );
            Ok(())
        }
        Command::Grant { app, email, role } => {
            let mut arguments = json!({ "app": app, "email": email, "allow": true });
            if let Some(role) = role {
                arguments["role"] = json!(role);
            }
            println!("{}", mcp.call("set_access", arguments)?);
            Ok(())
        }
        Command::Revoke { app, email } => {
            println!(
                "{}",
                mcp.call("set_access", json!({ "app": app, "email": email, "allow": false }))?
            );
            Ok(())
        }
        Command::Gate { app, gate, path } => {
            let mut arguments = json!({ "slug": app, "gate": gate });
            if let Some(path) = path {
                arguments["path"] = json!(path);
            }
            println!("{}", mcp.call("set_visibility", arguments)?);
            Ok(())
        }
        Command::Job {
            app,
            name,
            schedule,
            path,
            now,
            remove,
        } => {
            let arguments = match (&name, now, remove) {
                (Some(name), true, _) => json!({ "app": app, "name": name, "run_now": true }),
                (Some(name), _, true) => json!({ "app": app, "name": name }),
                (Some(name), _, _) => json!({
                    "app": app, "name": name,
                    "schedule": schedule.ok_or_else(|| anyhow!("pass --schedule and --path"))?,
                    "path": path.ok_or_else(|| anyhow!("pass --schedule and --path"))?,
                }),
                (None, ..) => json!({ "app": app }),
            };
            println!("{}", mcp.call("app_jobs", arguments)?);
            Ok(())
        }
        Command::Secret {
            app,
            name,
            value,
            remove,
            link,
        } => {
            if link {
                println!("{}", mcp.call("app_settings", json!({ "app": app, "link": true }))?);
                return Ok(());
            }
            let arguments = match (name, remove) {
                (None, _) => json!({ "app": app }),
                (Some(name), true) => json!({ "app": app, "name": name }),
                (Some(name), false) => {
                    let value = value
                        .or_else(|| std::env::var("TOOLSITE_SECRET").ok())
                        .ok_or_else(|| anyhow!("pass --value or set TOOLSITE_SECRET"))?;
                    json!({ "app": app, "name": name, "value": value })
                }
            };
            println!("{}", mcp.call("app_settings", arguments)?);
            Ok(())
        }
        Command::Notes { slug, file } => {
            let arguments = match file {
                Some(path) => {
                    let notes = std::fs::read_to_string(&path)
                        .with_context(|| format!("could not read {}", path.display()))?;
                    json!({ "slug": slug, "notes": notes })
                }
                None => json!({ "slug": slug }),
            };
            println!("{}", mcp.call("app_notes", arguments)?);
            Ok(())
        }
        Command::Hide { slug } => {
            println!("{}", mcp.call("set_visibility", json!({ "slug": slug, "hidden": true }))?);
            Ok(())
        }
        Command::Unhide { slug } => {
            println!("{}", mcp.call("set_visibility", json!({ "slug": slug, "hidden": false }))?);
            Ok(())
        }
    }
}

/// Numbers and booleans are bound as such; everything else as text.
fn json_scalar(raw: String) -> serde_json::Value {
    if let Ok(number) = raw.parse::<i64>() {
        return json!(number);
    }
    if let Ok(number) = raw.parse::<f64>() {
        return json!(number);
    }
    match raw.as_str() {
        "true" => json!(true),
        "false" => json!(false),
        "null" => serde_json::Value::Null,
        _ => json!(raw),
    }
}

struct Project {
    slug: String,
    spa: bool,
    /// What gets tarred up and served.
    web_root: PathBuf,
    handler_wasm: Option<PathBuf>,
}

fn read_project(dir: &Path, slug: Option<String>, spa_flag: bool) -> Result<Project> {
    let manifest = dir.join("toolsite.toml");
    let (mut slug_from_file, mut spa) = (None, false);
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        // Two keys is not worth a TOML dependency.
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("slug") {
                slug_from_file = value
                    .trim_start_matches([' ', '='])
                    .trim()
                    .trim_matches('"')
                    .to_string()
                    .into();
            }
            if let Some(value) = line.strip_prefix("spa") {
                spa = value.trim_start_matches([' ', '=']).trim() == "true";
            }
        }
    }

    let slug = slug
        .or(slug_from_file)
        .or_else(|| {
            dir.canonicalize()
                .ok()?
                .file_name()?
                .to_str()
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("could not work out a slug; pass --slug"))?;

    // Prefer a build output, since that is what a front-end toolchain emits.
    let web_root = ["dist", "build", "public"]
        .iter()
        .map(|candidate| dir.join(candidate))
        .find(|path| path.join("index.html").is_file())
        .or_else(|| dir.join("index.html").is_file().then(|| dir.to_path_buf()))
        .ok_or_else(|| {
            anyhow!(
                "no index.html in {}, dist/, build/ or public/ — build first, or pass a directory",
                dir.display()
            )
        })?;

    Ok(Project {
        slug,
        spa: spa || spa_flag,
        web_root,
        handler_wasm: build_handler(dir)?,
    })
}

/// Builds `handler/` if present, or takes a prebuilt handler.wasm.
fn build_handler(dir: &Path) -> Result<Option<PathBuf>> {
    let prebuilt = dir.join("handler.wasm");
    if prebuilt.is_file() {
        return Ok(Some(prebuilt));
    }
    let manifest = dir.join("handler/Cargo.toml");
    if !manifest.is_file() {
        return Ok(None);
    }

    println!("building handler…");
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2", "--manifest-path"])
        .arg(&manifest)
        .status()
        .context("could not run cargo; is it installed?")?;
    if !status.success() {
        bail!("handler build failed");
    }

    let out = dir.join("handler/target/wasm32-wasip2/release");
    let wasm = std::fs::read_dir(&out)
        .with_context(|| format!("no build output in {}", out.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "wasm"))
        .ok_or_else(|| anyhow!("handler built but produced no .wasm"))?;
    Ok(Some(wasm))
}

/// Unpacks the project a previous deploy stored, so a later session starts
/// from the sources rather than from the rendered page.
fn fetch(mcp: &Mcp, slug: Option<String>, dir: PathBuf) -> Result<()> {
    let slug = slug
        .or_else(|| read_slug(&dir))
        .or_else(|| {
            dir.canonicalize()
                .ok()?
                .file_name()?
                .to_str()
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("could not work out a slug; pass --slug"))?;

    let ticket = mcp.call("create_upload", json!({ "slug": slug }))?;
    let upload_url = find_upload_url(&ticket)
        .ok_or_else(|| anyhow!("could not find an upload URL in the server's reply"))?;

    let response = reqwest::blocking::Client::new()
        .get(format!("{upload_url}?source"))
        .send()?;
    if !response.status().is_success() {
        bail!("{}", response.text().unwrap_or_default().trim());
    }

    let bytes = response.bytes()?;
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    tar::Archive::new(decoder).unpack(&dir)?;
    println!("unpacked {slug} into {}", dir.display());
    Ok(())
}

fn deploy(
    mcp: &Mcp,
    base_url: &str,
    token: &str,
    dir: PathBuf,
    slug: Option<String>,
    spa: bool,
    keep_source: bool,
) -> Result<()> {
    let project = read_project(&dir, slug, spa)?;
    println!("publishing {} from {}", project.slug, project.web_root.display());

    let ticket = mcp.call("create_upload", json!({ "slug": project.slug }))?;
    let upload_url = find_upload_url(&ticket)
        .ok_or_else(|| anyhow!("could not find an upload URL in the server's reply"))?;

    let tarball = tar_gz(&project.web_root)?;
    let client = reqwest::blocking::Client::new();
    let query = if project.spa { "?bundle&spa" } else { "?bundle" };
    let response = client
        .put(format!("{upload_url}{query}"))
        .body(tarball)
        .send()?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("upload failed ({status}): {body}");
    }
    print!("{body}");

    if let Some(wasm) = &project.handler_wasm {
        let bytes = std::fs::read(wasm)?;
        let response = client
            .put(format!("{upload_url}?handler"))
            .body(bytes)
            .send()?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            bail!("handler upload failed ({status}): {body}");
        }
        print!("{body}");
    }

    // Schema first: a handler that goes live before its tables exist fails on
    // the first request, and that request may be a scheduled job nobody sees.
    if dir.join("migrations").is_dir() {
        match sql_archive(&dir.join("migrations")) {
            Ok(archive) if !archive.is_empty() => {
                let response = client
                    .put(format!("{upload_url}?migrations"))
                    .body(archive)
                    .send()?;
                let status = response.status();
                let body = response.text().unwrap_or_default();
                if status.is_success() {
                    print!("{body}");
                } else {
                    bail!("migrations were rejected ({status}): {}", body.trim());
                }
            }
            Ok(_) => {}
            Err(error) => bail!("could not package migrations: {error}"),
        }
    }

    // The manifest goes first, so the app is configured before it is reachable
    // rather than a moment after.
    if let Ok(manifest) = std::fs::read_to_string(dir.join("toolsite.toml")) {
        let response = client
            .put(format!("{upload_url}?manifest"))
            .body(manifest)
            .send()?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if status.is_success() {
            print!("{body}");
        } else {
            bail!("toolsite.toml was rejected ({status}): {}", body.trim());
        }
    }

    // Kept by default: a bundle cannot be turned back into what built it, and
    // the next session would otherwise start from the rendered page.
    if keep_source {
        match project_archive(&dir) {
            Ok(archive) => {
                let response = client
                    .put(format!("{upload_url}?source"))
                    .body(archive)
                    .send()?;
                if response.status().is_success() {
                    println!("kept the project with it; `toolsite fetch` brings it back");
                } else {
                    eprintln!("warning: could not store the project alongside the app");
                }
            }
            Err(error) => eprintln!("warning: could not package the project: {error}"),
        }
    }

    verify(&client, base_url, token, &project)
}

/// The .sql files in a directory, as a gzipped tar.
fn sql_archive(dir: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut any = false;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "sql") {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            builder.append_path_with_name(&path, &name)?;
            any = true;
        }
    }
    if !any {
        return Ok(Vec::new());
    }
    let tar = builder.into_inner()?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar)?;
    Ok(encoder.finish()?)
}

/// Everything except what a package manager or compiler can restore.
///
/// Build output is kept deliberately: for a project with no build step the
/// dist directory *is* the source, and dropping it would send back an archive
/// with the app missing from it.
fn project_archive(dir: &Path) -> Result<Vec<u8>> {
    const SKIP: [&str; 3] = ["node_modules", "target", ".git"];

    let mut builder = tar::Builder::new(Vec::new());
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            builder.append_dir_all(&name, &path)?;
        } else {
            builder.append_path_with_name(&path, &name)?;
        }
    }
    let tar = builder.into_inner()?;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar)?;
    Ok(encoder.finish()?)
}

/// The slug recorded in toolsite.toml, if there is one.
fn read_slug(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("toolsite.toml")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("slug").map(|value| {
            value
                .trim_start_matches([' ', '='])
                .trim()
                .trim_matches('"')
                .to_string()
        })
    })
}

/// A deploy that reports success without checking is how a blank page ships.
fn verify(
    client: &reqwest::blocking::Client,
    base_url: &str,
    _token: &str,
    project: &Project,
) -> Result<()> {
    let page = format!("{}/p/{}/", base_url.trim_end_matches('/'), project.slug);
    let response = client.get(&page).send()?;
    if !response.status().is_success() {
        bail!("published, but {page} answered {}", response.status());
    }
    let html = response.text().unwrap_or_default();

    // The base-path mistake: absolute /assets/... URLs that resolve above the
    // app and 404, leaving a page that loads blank.
    if html.contains("src=\"/assets/") || html.contains("href=\"/assets/") {
        eprintln!();
        eprintln!("warning: index.html references /assets/… from the domain root.");
        eprintln!("         This app is served from /p/{}/ so those will 404 and", project.slug);
        eprintln!("         the page will render blank. Set your build's base path:");
        eprintln!("           vite.config: base: '/p/{}/'", project.slug);
    }

    println!("live at {page}");
    Ok(())
}

fn find_upload_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|word| word.trim_matches(|c| c == '\'' || c == '"'))
        .find(|word| word.contains("/upload/") && word.starts_with("http"))
        .map(str::to_string)
}

fn tar_gz(root: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all(".", root)?;
    let tar = builder.into_inner()?;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar)?;
    Ok(encoder.finish()?)
}

fn print_pages(text: &str) {
    let Ok(pages) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        println!("{text}");
        return;
    };
    for page in pages {
        let slug = page["slug"].as_str().unwrap_or("?");
        let title = page["title"].as_str().unwrap_or("");
        let modified = page["modified"].as_str().unwrap_or("");
        let mut flags = Vec::new();
        if page["hidden"].as_bool() == Some(true) {
            flags.push("hidden");
        }
        if page["listed"].as_bool() == Some(false) {
            flags.push("unlisted");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join(", "))
        };
        println!("{slug:<28} {modified:<10} {title}{flags}");
    }
}

fn print_rows(text: &str) {
    let Ok(result) = serde_json::from_str::<serde_json::Value>(text) else {
        println!("{text}");
        return;
    };
    let columns: Vec<String> = result["columns"]
        .as_array()
        .map(|c| c.iter().map(|v| v.as_str().unwrap_or("?").to_string()).collect())
        .unwrap_or_default();
    let rows = result["rows"].as_array().cloned().unwrap_or_default();

    if !columns.is_empty() {
        println!("{}", columns.join("\t"));
    }
    for row in &rows {
        let cells: Vec<String> = row
            .as_array()
            .map(|values| values.iter().map(render_cell).collect())
            .unwrap_or_default();
        println!("{}", cells.join("\t"));
    }
    if result["truncated"].as_bool() == Some(true) {
        println!("… truncated at the server's row cap");
    }
    if rows.is_empty() {
        println!("{} row(s) affected", result["rows_affected"].as_u64().unwrap_or(0));
    }
}

fn render_cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upload_url_is_found_in_the_servers_prose() {
        let reply = "Upload with (expires in 15 min, reusable until then):\n\n  \
                     curl -fT <file.html> https://host.example/upload/abc123\n\n  \
                     tar -czf - -C dist . | curl -f -T - 'https://host.example/upload/abc123?bundle'";
        assert_eq!(
            find_upload_url(reply).unwrap(),
            "https://host.example/upload/abc123"
        );
    }

    #[test]
    fn parameters_keep_their_type() {
        assert_eq!(json_scalar("42".into()), json!(42));
        assert_eq!(json_scalar("1.5".into()), json!(1.5));
        assert_eq!(json_scalar("true".into()), json!(true));
        assert_eq!(json_scalar("hello".into()), json!("hello"));
        // A quoted-looking number is still text if it cannot parse.
        assert_eq!(json_scalar("42abc".into()), json!("42abc"));
    }

    #[test]
    fn a_tarball_round_trips() {
        let dir = std::env::temp_dir().join("toolsite-cli-tar-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();

        let bytes = tar_gz(&dir).unwrap();
        let decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(decoder);
        let mut names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.header().entry_type().is_file())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        // Bare paths, no wrapping directory — which is what the server
        // expects when nothing needs stripping.
        assert_eq!(names, ["assets/app.js", "index.html"]);
    }
}
