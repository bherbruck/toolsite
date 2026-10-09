//! Per-app records as files under `DATA_DIR`, as they always were:
//! `<app>.secrets` (name to sealed value), `<app>.tools`,
//! `<app>.migrations`, `<app>.repo` and `<app>.jobs` (name to job) beside
//! the app, and `.site/github.json` for the site.
//!
//! Each sidecar changes under a lock of its own in this process and is
//! written to a dotted temporary file renamed into place, so a reader never
//! sees half of one and a crash leaves either version. No slug can name a
//! sidecar or the temporary file, and `store::platform_file` refuses both to
//! the public routes and to bundles.

use super::{AppRecords, DocEdit};
use crate::content::catalog::files::write_aside;
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

pub struct Files {
    data_dir: PathBuf,
}

/// One lock per sidecar this process has changed, by its path.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

fn held<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    held(&LOCKS).entry(path.to_path_buf()).or_default().clone()
}

/// A sidecar's text; none when there is no file.
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} could not be read: {e}", name_of(path))),
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Writes a sidecar, or removes it when there is nothing to keep.
fn write(path: &Path, text: Option<&str>) -> Result<(), String> {
    match text {
        Some(text) => write_aside(path, text.as_bytes()),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        },
    }
}

impl Files {
    pub fn new(data_dir: PathBuf) -> Files {
        Files { data_dir }
    }

    fn sidecar(&self, app: &str, extension: &str) -> PathBuf {
        self.data_dir.join(format!("{app}.{extension}"))
    }

    fn installations_path(&self) -> PathBuf {
        self.data_dir.join(".site").join("github.json")
    }

    /// A settings file. One that exists but does not parse is an error, not
    /// an empty map: writing a change over it would lose every setting.
    fn read_settings(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        match read(&self.sidecar(app, "secrets"))? {
            Some(text) => serde_json::from_str(&text).map_err(|e| format!("{app}'s settings could not be read: {e}")),
            None => Ok(BTreeMap::new()),
        }
    }

    /// A jobs file, by name. One that exists but does not parse is an
    /// error, as a settings file is: a change written over it would lose
    /// every job.
    fn read_jobs(&self, app: &str) -> Result<BTreeMap<String, serde_json::Value>, String> {
        match read(&self.sidecar(app, "jobs"))? {
            Some(text) => serde_json::from_str(&text).map_err(|e| format!("{app}'s jobs could not be read: {e}")),
            None => Ok(BTreeMap::new()),
        }
    }

    fn jobs_text(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(self.read_jobs(app)?.into_iter().map(|(name, job)| (name, job.to_string())).collect())
    }

    /// Changes an app's jobs file with it held, written by rename, as every
    /// job file has been since a reader caught one half written.
    fn change_jobs<T>(
        &self,
        app: &str,
        change: impl FnOnce(&mut BTreeMap<String, serde_json::Value>) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let path = self.sidecar(app, "jobs");
        let lock = lock_for(&path);
        let _one_writer = held(&lock);
        let mut jobs = self.read_jobs(app)?;
        let (answer, write) = change(&mut jobs)?;
        if write {
            write_aside(&path, super::pretty(&jobs)?.as_bytes())?;
        }
        Ok(answer)
    }

    fn update_job_now(&self, app: &str, name: &str, edit: DocEdit<'_>) -> Result<bool, String> {
        self.change_jobs(app, |jobs| {
            let Some(job) = jobs.get_mut(name) else {
                return Ok((false, false));
            };
            let next = edit(Some(&job.to_string()))?;
            *job = serde_json::from_str(&next).map_err(|e| format!("a job is not JSON: {e}"))?;
            Ok((true, true))
        })
    }

    /// Every app with a sidecar of this kind, sorted. Names that are not an
    /// app's are passed over.
    fn apps_with(&self, extension: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.data_dir) else {
            return Vec::new();
        };
        let suffix = format!(".{extension}");
        let mut apps: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let app = name.strip_suffix(&suffix)?;
                crate::platform::export::valid_app(app).then(|| app.to_string())
            })
            .collect();
        apps.sort();
        apps
    }

    /// Runs `edit` on a sidecar's text with the sidecar held, and writes
    /// what it answers.
    fn change(&self, path: &Path, edit: DocEdit<'_>) -> Result<String, String> {
        let lock = lock_for(path);
        let _one_writer = held(&lock);
        let current = read(path)?;
        let next = edit(current.as_deref())?;
        write_aside(path, next.as_bytes())?;
        Ok(next)
    }
}

#[async_trait]
impl AppRecords for Files {
    async fn settings(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        self.read_settings(app)
    }

    fn settings_blocking(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        self.read_settings(app)
    }

    async fn set_setting(&self, app: &str, name: &str, sealed: Option<&str>) -> Result<bool, String> {
        let path = self.sidecar(app, "secrets");
        let lock = lock_for(&path);
        let _one_writer = held(&lock);
        let mut all = self.read_settings(app)?;
        let had = match sealed {
            Some(sealed) => all.insert(name.to_string(), sealed.to_string()).is_some(),
            None => all.remove(name).is_some(),
        };
        if sealed.is_none() && !had {
            return Ok(false);
        }
        // An app whose last setting went keeps an empty map, as it always
        // did: the file says the app had settings once.
        write_aside(&path, super::pretty(&all)?.as_bytes())?;
        Ok(had)
    }

    async fn tools(&self, app: &str) -> Result<Option<String>, String> {
        read(&self.sidecar(app, "tools"))
    }

    async fn set_tools(&self, app: &str, tools: Option<&str>) -> Result<(), String> {
        let path = self.sidecar(app, "tools");
        let lock = lock_for(&path);
        let _one_writer = held(&lock);
        write(&path, tools)
    }

    async fn apps_with_tools(&self) -> Result<Vec<String>, String> {
        Ok(self.apps_with("tools"))
    }

    fn migrations_blocking(&self, app: &str) -> Result<Option<String>, String> {
        read(&self.sidecar(app, "migrations"))
    }

    fn set_migrations_blocking(&self, app: &str, ladder: &str) -> Result<(), String> {
        let path = self.sidecar(app, "migrations");
        let lock = lock_for(&path);
        let _one_writer = held(&lock);
        write(&path, Some(ladder))
    }

    async fn repo_link(&self, app: &str) -> Result<Option<String>, String> {
        read(&self.sidecar(app, "repo"))
    }

    async fn update_repo_link(&self, app: &str, edit: DocEdit<'_>) -> Result<String, String> {
        self.change(&self.sidecar(app, "repo"), edit)
    }

    async fn repo_links(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for app in self.apps_with("repo") {
            if let Some(text) = read(&self.sidecar(&app, "repo"))? {
                out.push((app, text));
            }
        }
        Ok(out)
    }

    async fn installations(&self) -> Result<Option<String>, String> {
        read(&self.installations_path())
    }

    async fn set_installations(&self, installations: &str) -> Result<(), String> {
        let path = self.installations_path();
        let lock = lock_for(&path);
        let _one_writer = held(&lock);
        write(&path, Some(installations))
    }

    async fn jobs(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        self.jobs_text(app)
    }

    fn jobs_blocking(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        self.jobs_text(app)
    }

    async fn all_jobs(&self) -> Result<Vec<(String, String, String)>, String> {
        let mut out = Vec::new();
        for app in self.apps_with("jobs") {
            // One app's torn file does not stop every other app's jobs.
            match self.jobs_text(&app) {
                Ok(jobs) => out.extend(jobs.into_iter().map(|(name, job)| (app.clone(), name, job))),
                Err(why) => tracing::warn!(app, %why, "jobs skipped"),
            }
        }
        Ok(out)
    }

    async fn set_job(&self, app: &str, name: &str, job: &str, most: usize) -> Result<Result<(), usize>, String> {
        let job: serde_json::Value = serde_json::from_str(job).map_err(|e| format!("a job is not JSON: {e}"))?;
        self.change_jobs(app, |jobs| {
            if !jobs.contains_key(name) && jobs.len() >= most {
                return Ok((Err(jobs.len()), false));
            }
            jobs.insert(name.to_string(), job);
            Ok((Ok(()), true))
        })
    }

    async fn remove_job(&self, app: &str, name: &str) -> Result<bool, String> {
        self.change_jobs(app, |jobs| {
            let had = jobs.remove(name).is_some();
            Ok((had, had))
        })
    }

    async fn update_job(&self, app: &str, name: &str, edit: DocEdit<'_>) -> Result<bool, String> {
        self.update_job_now(app, name, edit)
    }

    fn update_job_blocking(&self, app: &str, name: &str, edit: DocEdit<'_>) -> Result<bool, String> {
        self.update_job_now(app, name, edit)
    }

    async fn fire(&self, _app: &str, _name: &str, _due_at: u64) -> Result<bool, String> {
        Ok(true)
    }

    /// Nothing to take: the sidecars are files, and the trash moves them.
    fn retire_blocking(&self, _app: &str, _at: u64) -> Result<Vec<(&'static str, String)>, String> {
        Ok(Vec::new())
    }
}
