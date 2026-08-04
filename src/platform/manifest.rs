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
    content::store::{read_meta, write_meta, PathRule},
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
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    pub gate: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub path: String,
}

const GATES: [&str; 3] = ["public", "authenticated", "granted"];

/// Applies a manifest to one app, reporting what changed so a deploy says
/// what it did rather than only that it finished.
pub async fn apply(config: &Config, app: &str, toml_text: &str) -> Result<Vec<String>, String> {
    let manifest: Manifest = toml::from_str(toml_text).map_err(|e| {
        // serde names the offending key, which is the whole value here: the
        // difference between [[job]] and [[jobs]] is invisible otherwise.
        format!(
            "could not read toolsite.toml: {e}\nKeys it takes: slug, spa, gate, icon, \
             allow_http, [[route]] (path, gate), [[job]] (name, schedule, path)."
        )
    })?;

    // Everything is checked before anything is written: half an applied
    // manifest is worse than a rejected one.
    if let Some(gate) = &manifest.gate {
        if !GATES.contains(&gate.as_str()) {
            return Err(format!("gate must be one of {}", GATES.join(", ")));
        }
    }
    for route in &manifest.routes {
        if !route.path.starts_with('/') {
            return Err(format!("route path must start with '/', got {}", route.path));
        }
        if !GATES.contains(&route.gate.as_str()) {
            return Err(format!(
                "route {} has gate {}, which is not one of {}",
                route.path,
                route.gate,
                GATES.join(", ")
            ));
        }
    }

    let mut changed = Vec::new();
    let mut meta = read_meta(config, app).await;

    if let Some(spa) = manifest.spa {
        if meta.spa != spa {
            meta.spa = spa;
            changed.push(format!("spa = {spa}"));
        }
    }
    if let Some(gate) = manifest.gate {
        if meta.gate != gate {
            changed.push(format!("gate = {gate}"));
            meta.gate = gate;
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
        assert_eq!(meta.gate_for("/"), "public");
        assert_eq!(meta.gate_for("/triage"), "authenticated");
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
        assert_eq!(read_meta(&config, "board").await.gate, "public");
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
