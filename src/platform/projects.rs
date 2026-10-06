//! The rules for running projects: who may create one, move an app between
//! them, and give or take access on them. The admin forms, the app browser
//! and the `projects` MCP tool all come here, so a rule is written once and
//! means the same wherever it is asked.
//!
//! The actor is an account, or `None` for a static token or the stdio
//! transport, which have every power. A function says no with a [`Problem`]:
//! `Refused` when the actor lacks the scope (a form answers 403), `Invalid`
//! when the request itself cannot be done (a form shows it as a message).

use crate::{
    accounts::users::{self, Scope, ScopeGrant, User},
    config::Config,
    content::store::{self, read_meta, write_meta, Folder},
    platform::{admin, export},
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Problem {
    /// The actor lacks the scope this needs. Names where and what.
    Refused(String),
    /// The request cannot be done as asked.
    Invalid(String),
}

impl Problem {
    pub(crate) fn message(&self) -> &str {
        match self {
            Problem::Refused(m) | Problem::Invalid(m) => m,
        }
    }
}

/// How a place reads in a message: a path, or "the site" for the top.
pub(crate) fn place(path: &str) -> String {
    if path.is_empty() {
        "the site".to_string()
    } else {
        path.to_string()
    }
}

fn refused(needed: Scope, path: &str) -> Problem {
    Problem::Refused(format!("You need {needed} access at {} for this.", place(path)))
}

/// What the actor holds at a project path. Full power holds admin.
pub(crate) async fn holds(config: &Arc<Config>, actor: Option<&User>, path: &str) -> Option<Scope> {
    match actor {
        None => Some(Scope::Admin),
        Some(user) => admin::held(config, user, path).await,
    }
}

async fn need(config: &Arc<Config>, actor: Option<&User>, path: &str, needed: Scope) -> Result<(), Problem> {
    match holds(config, actor, path).await {
        Some(have) if have >= needed => Ok(()),
        _ => {
            if let Some(user) = actor {
                tracing::warn!(email = %user.email, path = %path, needed = %needed, "projects refused: scope");
            }
            Err(refused(needed, path))
        }
    }
}

fn clean(path: &str) -> Result<String, Problem> {
    let path = path.trim().trim_matches('/').to_string();
    if users::valid_prefix(&path) {
        Ok(path)
    } else {
        Err(Problem::Invalid(format!(
            "'{path}' is not a project path: segments of letters, numbers, '-' or '_', joined by '/'"
        )))
    }
}

/// One project as a caller sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Node {
    pub path: String,
    pub name: String,
    /// Apps in it or below that the caller may see.
    pub apps: usize,
    /// What the caller holds here, if anything.
    pub scope: Option<String>,
}

/// The projects the actor may see, with what it holds at each. An account
/// sees what the app browser shows it; full power sees every project and
/// counts every app.
pub(crate) async fn tree(config: &Arc<Config>, actor: Option<&User>) -> Vec<Node> {
    let counted: Vec<(String, usize)> = match actor {
        Some(user) => crate::content::browse::visible_projects(config, Some(user)).await,
        None => {
            let apps = store::apps_with_folders(config).await;
            store::list_folders(config)
                .await
                .into_iter()
                .map(|folder| {
                    let below = apps.iter().filter(|(_, at)| users::prefix_covers(&folder.path, at)).count();
                    (folder.path, below)
                })
                .collect()
        }
    };
    let mut nodes = Vec::with_capacity(counted.len());
    for (path, apps) in counted {
        let scope = holds(config, actor, &path).await.map(|s| s.to_string());
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        nodes.push(Node { path, name, apps, scope });
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    nodes
}

/// Creates `parent/name`. Admin at the parent.
pub(crate) async fn create(config: &Arc<Config>, actor: Option<&User>, parent: &str, name: &str) -> Result<Folder, Problem> {
    let parent = clean(parent)?;
    need(config, actor, &parent, Scope::Admin).await?;
    let folder = store::create_folder(config, &parent, name.trim()).await.map_err(Problem::Invalid)?;
    tracing::info!(by = %actor.map(|u| u.email.as_str()).unwrap_or("token"), folder = %folder.path, "project created");
    Ok(folder)
}

/// Moves an app into `target` (empty for the top level). Admin on the app
/// where it is now and at the target, since access from the old projects
/// stops and access from the new ones starts. Returns the app's new path.
pub(crate) async fn move_app(config: &Arc<Config>, actor: Option<&User>, app: &str, target: &str) -> Result<String, Problem> {
    if !export::valid_app(app) {
        return Err(Problem::Invalid(format!("'{app}' is not an app name")));
    }
    if !store::app_exists(config, app).await {
        return Err(Problem::Invalid(format!("There is no app '{app}'.")));
    }
    let target = clean(target)?;
    let from = admin::app_path(config, app).await;
    if let Some(user) = actor
        && !admin::held_on(config, user, app).await.is_some_and(|have| have >= Scope::Admin)
    {
        tracing::warn!(email = %user.email, path = %from, "projects refused: move from");
        return Err(refused(Scope::Admin, &from));
    }
    if !store::folder_exists(config, &target).await {
        return Err(Problem::Invalid(format!("There is no project '{target}'.")));
    }
    need(config, actor, &target, Scope::Admin).await?;
    let to = if target.is_empty() { app.to_string() } else { format!("{target}/{app}") };
    if from == to {
        return Ok(to);
    }
    let mut meta = read_meta(config, app).await;
    meta.project = (!target.is_empty()).then(|| target.clone());
    write_meta(config, app, &meta)
        .await
        .map_err(|_| Problem::Invalid("The app was not moved.".into()))?;
    let (config2, old, new) = (config.clone(), from.clone(), to.clone());
    let _ = tokio::task::spawn_blocking(move || users::move_scopes(&config2, &old, &new)).await;
    tracing::info!(by = %actor.map(|u| u.email.as_str()).unwrap_or("token"), app, from = %from, to = %to, "app moved");
    Ok(to)
}

/// Who holds access at a project: set there, and set above it.
pub(crate) struct Holders {
    pub direct: Vec<ScopeGrant>,
    pub inherited: Vec<ScopeGrant>,
}

/// Holders at `path`, without a scope check: for a caller that has already
/// checked, such as a page that is only drawn for an admin.
pub(crate) async fn holders_unchecked(config: &Arc<Config>, path: &str) -> Holders {
    let scopes = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_scopes(&config).unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    let mut direct: Vec<ScopeGrant> = scopes.iter().filter(|row| row.prefix == path).cloned().collect();
    let mut inherited: Vec<ScopeGrant> = scopes
        .iter()
        .filter(|row| row.prefix != path && users::prefix_covers(&row.prefix, path))
        .cloned()
        .collect();
    direct.sort_by(|a, b| a.email.cmp(&b.email));
    inherited.sort_by(|a, b| (a.prefix.len(), &a.email).cmp(&(b.prefix.len(), &b.email)));
    Holders { direct, inherited }
}

/// Holders at `path`. Admin there.
pub(crate) async fn holders(config: &Arc<Config>, actor: Option<&User>, path: &str) -> Result<Holders, Problem> {
    let path = clean(path)?;
    need(config, actor, &path, Scope::Admin).await?;
    Ok(holders_unchecked(config, &path).await)
}

/// Gives `email` `scope` at `path`. Admin there, and never more than the
/// actor holds there; the path is the one asked for, so nothing above it.
pub(crate) async fn grant(config: &Arc<Config>, actor: Option<&User>, path: &str, email: &str, scope: Scope) -> Result<(), Problem> {
    let path = clean(path)?;
    need(config, actor, &path, Scope::Admin).await?;
    if let Some(user) = actor
        && !user.is_admin
    {
        let mine = holds(config, actor, &path).await.unwrap_or(Scope::Viewer);
        if scope > mine {
            return Err(Problem::Refused(format!(
                "You hold {mine} at {} and cannot give {scope}.",
                place(&path)
            )));
        }
    }
    let email = email.trim().to_lowercase();
    let (config2, who, at, by) = (config.clone(), email.clone(), path.clone(), actor.map(|u| u.email.clone()));
    tokio::task::spawn_blocking(move || users::grant_scope(&config2, &who, &at, scope, by.as_deref()))
        .await
        .map_err(|_| Problem::Invalid("Access was not changed.".into()))?
        .map_err(Problem::Invalid)?;
    tracing::info!(by = %actor.map(|u| u.email.as_str()).unwrap_or("token"), account = %email, prefix = %path, scope = %scope, "scope granted");
    Ok(())
}

/// Takes away what `email` holds at `path` itself. Admin there.
pub(crate) async fn revoke(config: &Arc<Config>, actor: Option<&User>, path: &str, email: &str) -> Result<(), Problem> {
    let path = clean(path)?;
    need(config, actor, &path, Scope::Admin).await?;
    let email = email.trim().to_lowercase();
    let (config2, who, at) = (config.clone(), email.clone(), path.clone());
    tokio::task::spawn_blocking(move || users::revoke_scope(&config2, &who, &at))
        .await
        .map_err(|_| Problem::Invalid("Access was not changed.".into()))?
        .map_err(Problem::Invalid)?;
    tracing::info!(by = %actor.map(|u| u.email.as_str()).unwrap_or("token"), account = %email, prefix = %path, "scope revoked");
    Ok(())
}
