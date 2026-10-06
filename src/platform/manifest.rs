//! `toolsite.toml`: an app saying what it needs, rather than someone
//! remembering which commands to run.
//!
//! Configuration belongs with the source, which the platform keeps, so
//! fetching an app brings back the intent as well as the code and a redeploy
//! reproduces it. Commands still work and are the right tool for a one-off;
//! the manifest is for anything meant to survive the session that set it.
//!
//! What it declares, it owns: routes and jobs are replaced wholesale, so
//! deleting a line removes the thing. What it does not mention is left alone,
//! so hiding an app by hand is not undone by the next deploy.

use crate::{
    config::Config,
    content::store::{read_meta, write_meta, PageMeta, PathRule, Policy},
    platform::schedule,
};
use serde::Deserialize;

/// Unknown keys are refused rather than ignored. `[[jobs]]` instead of
/// `[[job]]` used to parse cleanly and schedule nothing, which is the worst
/// kind of failure: the deploy says it worked.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Named here only so a manifest reads completely; the upload ticket
    /// already decided which app this is.
    #[serde(default)]
    pub slug: Option<String>,
    /// Unknown paths serve index.html, for a client-side router.
    #[serde(default)]
    pub spa: Option<bool>,
    /// Who may reach the app: public, authenticated, granted.
    #[serde(default)]
    pub gate: Option<String>,
    /// Emoji or inline SVG shown beside the app on the index.
    #[serde(default)]
    pub icon: Option<String>,
    /// Hosts the handler may reach. Absent leaves whatever is set; an empty
    /// list takes the capability away.
    #[serde(default)]
    pub allow_http: Option<Vec<String>>,
    /// Roles the handler checks with identity::current-role, offered as
    /// suggestions wherever access is granted. Absent leaves whatever is
    /// declared; an empty list withdraws the hint.
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
    /// MCP tools the app offers, each a route into its handler. Declared
    /// wholesale: a tool removed from the file is withdrawn.
    #[serde(default, rename = "tool")]
    pub tools: Vec<ToolDecl>,
    /// What a person may query from outside the app, and the row-level
    /// policies the platform turns into views. Present means declared
    /// wholesale: what the block does not name is withdrawn.
    #[serde(default)]
    pub access: Option<Access>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    /// Hand-written views a person may read. Read only.
    #[serde(default)]
    pub views: Vec<String>,
    /// One policy per table: a view the platform generates, with triggers
    /// when it may write.
    #[serde(default, rename = "table")]
    pub tables: Vec<TablePolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TablePolicy {
    pub table: String,
    /// Defaults to `my_<table>`.
    #[serde(default)]
    pub view: Option<String>,
    #[serde(rename = "where")]
    pub where_: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    pub gate: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDecl {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    pub description: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub open_world: bool,
    /// A JSON Schema: inline as a table, or the path of a file in the
    /// project's stored source.
    #[serde(default)]
    pub input: Option<SchemaRef>,
    #[serde(default)]
    pub output: Option<SchemaRef>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SchemaRef {
    File(String),
    Inline(toml::Table),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub path: String,
}

use crate::content::store::{normalise_gate, GATES};

/// A copy of the meta for a blocking task. PageMeta is not Clone on purpose
/// (it is read from and written to one file), so this goes through serde.
fn meta_snapshot(meta: &PageMeta) -> PageMeta {
    serde_json::from_value(serde_json::to_value(meta).expect("meta serialises")).expect("meta round-trips")
}

/// Applies a manifest to one app, reporting what changed so a deploy says
/// what it did rather than only that it finished.
pub async fn apply(config: &Config, app: &str, toml_text: &str) -> Result<Vec<String>, String> {
    let manifest: Manifest = toml::from_str(toml_text).map_err(|e| {
        // serde names the offending key, which is the whole value here: the
        // difference between [[job]] and [[jobs]] is invisible otherwise.
        format!(
            "could not read toolsite.toml: {e}\nKeys it takes: slug, spa, gate, icon, \
             allow_http, roles, [[route]] (path, gate), [[job]] (name, schedule, path), \
             [access] views, [[access.table]] (table, view, where, owner, write), \
             [[tool]] (name, title, description, path, read_only, destructive, idempotent, \
             open_world, input, output)."
        )
    })?;

    // Everything is checked before anything is written: half an applied
    // manifest is worse than a rejected one.
    if let Some(gate) = &manifest.gate {
        if normalise_gate(gate).is_none() && gate != "default" {
            return Err(format!("gate must be one of {}, or default (granted is the old name for restricted)", GATES.join(", ")));
        }
    }
    for route in &manifest.routes {
        if !route.path.starts_with('/') {
            return Err(format!("route path must start with '/', got {}", route.path));
        }
        if normalise_gate(&route.gate).is_none() {
            return Err(format!(
                "route {} has gate {}, which is not one of {}",
                route.path,
                route.gate,
                GATES.join(", ")
            ));
        }
    }

    let tools = resolve_tools(config, app, &manifest.tools).await?;

    let mut changed = Vec::new();
    let mut meta = read_meta(config, app).await;

    if let Some(spa) = manifest.spa {
        if meta.spa != spa {
            meta.spa = spa;
            changed.push(format!("spa = {spa}"));
        }
    }
    if let Some(gate) = manifest.gate {
        let wanted = (gate != "default").then(|| normalise_gate(&gate).unwrap_or("restricted").to_string());
        if meta.gate != wanted {
            changed.push(format!("gate = {gate}"));
            meta.gate = wanted;
        }
    }

    if let Some(roles) = manifest.roles {
        let roles: Vec<String> = roles
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        if meta.roles != roles {
            changed.push(if roles.is_empty() {
                "roles cleared".to_string()
            } else {
                format!("roles = [{}]", roles.join(", "))
            });
            meta.roles = roles;
        }
    }

    if let Some(allow) = manifest.allow_http {
        if meta.allow_http != allow {
            changed.push(if allow.is_empty() {
                "allow_http cleared".to_string()
            } else {
                format!("allow_http = {}", allow.join(", "))
            });
            meta.allow_http = allow;
        }
    }

    // Declared wholesale: a route removed from the file is removed here.
    let declared: Vec<PathRule> = manifest
        .routes
        .into_iter()
        .map(|route| PathRule {
            prefix: route.path,
            gate: route.gate,
        })
        .collect();
    let same = declared.len() == meta.rules.len()
        && declared
            .iter()
            .all(|rule| meta.rules.iter().any(|existing| existing.prefix == rule.prefix && existing.gate == rule.gate));
    if !same {
        changed.push(format!("{} route rule(s)", declared.len()));
        meta.rules = declared;
    }

    // Access is declared wholesale too. Policies are checked against the
    // app's own database and realised as views and triggers right away;
    // one whose table does not exist yet waits for the migrations.
    if let Some(access) = manifest.access {
        let views: Vec<String> = access.views.iter().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
        for view in &views {
            if !crate::runtime::access::valid_identifier(view) {
                return Err(format!("access: {view:?} is not a view name"));
            }
        }
        let mut policies = Vec::new();
        for declared in access.tables {
            let table = declared.table.trim().to_string();
            policies.push(Policy {
                view: declared
                    .view
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| format!("my_{table}")),
                table,
                where_: declared.where_.trim().to_string(),
                owner: declared.owner.map(|o| o.trim().to_string()).filter(|o| !o.is_empty()),
                write: declared.write,
            });
        }
        let mut seen = std::collections::HashSet::new();
        for policy in &policies {
            if !seen.insert(policy.view.to_lowercase()) || views.iter().any(|v| v.eq_ignore_ascii_case(&policy.view)) {
                return Err(format!("access: the view name {} is used twice", policy.view));
            }
        }
        if meta.queryable != views {
            changed.push(if views.is_empty() {
                "access views cleared".to_string()
            } else {
                format!("access views = [{}]", views.join(", "))
            });
            meta.queryable = views;
        }
        if meta.policies != policies {
            changed.push(if policies.is_empty() {
                "access policies cleared".to_string()
            } else {
                format!(
                    "access policies for {}",
                    policies.iter().map(|p| p.table.as_str()).collect::<Vec<_>>().join(", ")
                )
            });
            meta.policies = policies;
        }
        let regenerated = {
            let (config, app, meta_copy) = (config.clone_for_task(), app.to_string(), meta_snapshot(&meta));
            tokio::task::spawn_blocking(move || crate::runtime::access::regenerate(&config, &app, &meta_copy))
                .await
                .map_err(|e| e.to_string())??
        };
        if meta.generated != regenerated.generated {
            meta.generated = regenerated.generated;
        }
        if meta.access_salt.as_deref() != Some(regenerated.salt.as_str()) {
            meta.access_salt = Some(regenerated.salt);
        }
        changed.extend(regenerated.notes);
    }

    write_meta(config, app, &meta)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(icon) = manifest.icon {
        let path = config.data_dir.join(format!("{app}.icon"));
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::write(path, icon)
            .await
            .map_err(|e| e.to_string())?;
        changed.push("icon".to_string());
    }

    // Tools are declared wholesale too.
    let current_tools = crate::platform::app_tools::read(config, app);
    if current_tools != tools {
        crate::platform::app_tools::write(config, app, &tools)?;
        changed.push(if tools.is_empty() {
            "tools withdrawn".to_string()
        } else {
            format!("{} tool(s): {}", tools.len(), tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "))
        });
    }

    // Jobs too, but their history survives: a schedule that did not change
    // keeps when it last ran and how it went.
    let existing = schedule::read_jobs(config, app);
    let declared_names: Vec<String> = manifest.jobs.iter().map(|job| job.name.clone()).collect();
    for name in existing.keys() {
        if !declared_names.contains(name) {
            let _ = schedule::remove_job(config, app, name);
            changed.push(format!("job {name} removed"));
        }
    }
    for job in manifest.jobs {
        let unchanged = existing
            .get(&job.name)
            .is_some_and(|current| current.schedule == job.schedule && current.path == job.path);
        if unchanged {
            continue;
        }
        schedule::set_job(config, app, &job.name, &job.schedule, &job.path)?;
        changed.push(format!("job {}", job.name));
    }

    Ok(changed)
}

/// Turns declared tools into stored ones, checking everything first: a
/// manifest with one bad tool applies nothing.
async fn resolve_tools(
    config: &Config,
    app: &str,
    declared: &[ToolDecl],
) -> Result<Vec<crate::platform::app_tools::AppTool>, String> {
    use crate::platform::app_tools::{
        app_may_offer_tools, check_schema, clean_text, full_name, valid_tool_name, valid_tool_path, AppTool,
        MAX_DESCRIPTION, MAX_TITLE, MAX_TOOLS, MAX_TOOL_NAME,
    };
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    if declared.len() > MAX_TOOLS {
        return Err(format!("{} tools declared; an app may declare at most {MAX_TOOLS}", declared.len()));
    }
    if !app_may_offer_tools(app) {
        return Err(format!(
            "{app} cannot offer tools: an app slug with a double underscore or a trailing underscore would make its tool names ambiguous"
        ));
    }
    let mut source: Option<Vec<(String, Vec<u8>)>> = None;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tool in declared {
        let name = tool.name.trim().to_string();
        if !valid_tool_name(&name) {
            return Err(format!(
                "tool {name:?}: a name starts with a letter and is lower-case letters, digits and single underscores, with none at the end"
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(format!("tool {name}: declared twice"));
        }
        let full = full_name(app, &name);
        if full.len() > MAX_TOOL_NAME {
            return Err(format!(
                "tool {name}: its full name {full} is {} characters, over MCP's {MAX_TOOL_NAME}. Use a shorter name.",
                full.len()
            ));
        }
        let path = tool.path.trim().to_string();
        if !valid_tool_path(&path) {
            return Err(format!(
                "tool {name}: path must be a handler route under /api/ made of letters, digits, '-', '_' and '.', got {path:?}"
            ));
        }
        let description = tool.description.trim();
        if description.is_empty() {
            return Err(format!("tool {name}: description is empty; it is what the model reads"));
        }
        if description.chars().count() > MAX_DESCRIPTION || !clean_text(description, true) {
            return Err(format!(
                "tool {name}: description must be at most {MAX_DESCRIPTION} characters with no control characters"
            ));
        }
        if let Some(title) = tool.title.as_deref()
            && (title.chars().count() > MAX_TITLE || !clean_text(title, false))
        {
            return Err(format!("tool {name}: title must be at most {MAX_TITLE} characters on one line"));
        }
        let mut schema = |which: &str, reference: &Option<SchemaRef>| -> Result<Option<serde_json::Value>, String> {
            let Some(reference) = reference else { return Ok(None) };
            let value = match reference {
                SchemaRef::Inline(table) => serde_json::to_value(table).map_err(|e| e.to_string())?,
                SchemaRef::File(file) => {
                    if source.is_none() {
                        let archive = std::fs::read(config.data_dir.join(format!("{app}.source"))).map_err(|_| {
                            format!("tool {name}: {which} names the file {file}, but no source is stored for {app}. Upload the project with ?source first, or put the schema inline.")
                        })?;
                        source = Some(crate::content::bundle::read_all_files(&archive, 5000, 64 * 1024 * 1024)?);
                    }
                    let wanted = file.trim_start_matches("./");
                    let bytes = source
                        .as_ref()
                        .and_then(|files| files.iter().find(|(p, _)| p.trim_start_matches("./") == wanted))
                        .map(|(_, b)| b.clone())
                        .ok_or_else(|| format!("tool {name}: {which} file {file} is not in the stored source"))?;
                    serde_json::from_slice(&bytes).map_err(|e| format!("tool {name}: {which} file {file} is not JSON: {e}"))?
                }
            };
            check_schema(&value, &format!("tool {name}: {which}"))?;
            Ok(Some(value))
        };
        let input = schema("input", &tool.input)?.unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
        let output = schema("output", &tool.output)?;
        out.push(AppTool {
            name,
            title: tool.title.clone().filter(|t| !t.trim().is_empty()),
            description: tool.description.trim().to_string(),
            path,
            read_only: tool.read_only,
            destructive: tool.destructive,
            idempotent: tool.idempotent,
            open_world: tool.open_world,
            input,
            output,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        (
            tempfile::tempdir().unwrap(),
            Config::local(dir.keep(), "test-token"),
        )
    }

    #[tokio::test]
    async fn a_manifest_sets_what_commands_would_have() {
        let (_t, config) = config();
        let changed = apply(
            &config,
            "board",
            r#"
                slug = "board"
                spa = true
                gate = "public"
                icon = "📋"

                [[route]]
                path = "/triage"
                gate = "authenticated"

                [[job]]
                name = "rollup"
                schedule = "0 0 3 * * *"
                path = "/api/rollup"
            "#,
        )
        .await
        .unwrap();
        assert!(!changed.is_empty());

        let meta = read_meta(&config, "board").await;
        assert!(meta.spa);
        assert_eq!(meta.gate_for("/", "granted"), "public");
        assert_eq!(meta.gate_for("/triage", "granted"), "authenticated");
        assert_eq!(schedule::read_jobs(&config, "board").len(), 1);
    }

    #[tokio::test]
    async fn removing_a_line_removes_the_thing() {
        let (_t, config) = config();
        apply(
            &config,
            "board",
            "[[route]]\npath = \"/triage\"\ngate = \"granted\"\n\n\
             [[job]]\nname = \"rollup\"\nschedule = \"0 0 3 * * *\"\npath = \"/api/x\"\n",
        )
        .await
        .unwrap();

        // The manifest owns what it declares, so an empty one clears them.
        apply(&config, "board", "gate = \"public\"\n").await.unwrap();
        assert!(read_meta(&config, "board").await.rules.is_empty());
        assert!(schedule::read_jobs(&config, "board").is_empty());
    }

    #[tokio::test]
    async fn a_job_that_did_not_change_keeps_its_history() {
        let (_t, config) = config();
        let manifest = "[[job]]\nname = \"rollup\"\nschedule = \"0 0 3 * * *\"\npath = \"/api/x\"\n";
        apply(&config, "board", manifest).await.unwrap();

        // Pretend it ran.
        schedule::record_run(&config, "board", "rollup", "200");
        apply(&config, "board", manifest).await.unwrap();

        let jobs = schedule::read_jobs(&config, "board");
        assert_eq!(
            jobs["rollup"].last_status.as_deref(),
            Some("200"),
            "redeploying forgot when the job last ran"
        );
    }

    #[tokio::test]
    async fn nothing_is_written_when_part_of_it_is_wrong() {
        let (_t, config) = config();
        apply(&config, "board", "gate = \"public\"\n").await.unwrap();

        let error = apply(
            &config,
            "board",
            "gate = \"authenticated\"\n\n[[route]]\npath = \"triage\"\ngate = \"public\"\n",
        )
        .await
        .unwrap_err();
        assert!(error.contains("must start with '/'"), "got {error}");

        // The valid half must not have been applied.
        assert_eq!(read_meta(&config, "board").await.gate.as_deref(), Some("public"));
    }

    #[tokio::test]
    async fn outbound_hosts_come_from_the_manifest_and_default_to_none() {
        let (_t, config) = config();
        assert!(read_meta(&config, "app").await.allow_http.is_empty());

        apply(&config, "app", "allow_http = [\"api.github.com\"]\n")
            .await
            .unwrap();
        assert_eq!(
            read_meta(&config, "app").await.allow_http,
            ["api.github.com"]
        );

        // An empty list is a decision, not an omission: it takes it away.
        apply(&config, "app", "allow_http = []\n").await.unwrap();
        assert!(read_meta(&config, "app").await.allow_http.is_empty());
    }

    #[tokio::test]
    async fn a_key_nobody_recognises_is_refused_rather_than_ignored() {
        let (_t, config) = config();
        // The plural is the natural guess and used to schedule nothing while
        // reporting success.
        let error = apply(
            &config,
            "app",
            "[[jobs]]\nname = \"rollup\"\nschedule = \"0 0 3 * * *\"\npath = \"/api/x\"\n",
        )
        .await
        .unwrap_err();
        assert!(error.contains("jobs"), "the error should name the key: {error}");
        assert!(error.contains("[[job]]"), "and say what was meant: {error}");

        // A misspelled field inside a table too.
        assert!(apply(&config, "app", "[[job]]\nname = \"x\"\ncron = \"0 0 3 * * *\"\npath = \"/a\"\n")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_gate_nobody_defined_is_refused() {
        let (_t, config) = config();
        assert!(apply(&config, "board", "gate = \"sort-of-public\"\n")
            .await
            .is_err());
    }
}
