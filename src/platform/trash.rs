//! Taking something down for good, without destroying it.
//!
//! Nothing else here deletes: hiding a page keeps it, replacing a bundle
//! keeps the database, a manifest that drops a job keeps the schema. But junk
//! accumulates — a probe published as a page, an app nobody wants — and with
//! no way to remove it, `set_visibility` is the only answer and it hides the
//! good copy along with the bad.
//!
//! So removal moves everything belonging to a slug into `.trash/`, which no
//! URL can reach and no listing walks. It disappears from the site and is
//! still on disk if it turns out to have mattered.

use crate::{
    config::Config,
    content::{
        files::{self, Files},
        slug::valid_slug,
    },
};

/// Everything that can belong to one slug, beyond its own directory.
const SIDECARS: [&str; 13] = [
    "html", "meta", "icon", "notes", "source", "secrets", "jobs", "migrations", "exports", "deploys", "devices", "repo", "tools",
];

/// The sidecars that are published files, which a site on Postgres keeps
/// in its bucket. The rest are rows there, copied into the trash as files.
const PUBLISHED: [&str; 3] = ["html", "icon", "source"];

/// What a removal did: the trash entry it filled, and what went into it.
#[derive(Debug)]
pub struct Removed {
    pub entry: String,
    pub moved: Vec<String>,
}

/// Takes the tokens kept at `app` out of use, into the trash as a removal
/// would: for the first publish at a name, whose tokens were minted before
/// there was an app to hold them, by whoever could then. Returns what was
/// moved, which is nothing at a fresh name.
pub fn retire_tokens(config: &Config, app: &str, at: u64) -> Result<Vec<String>, String> {
    if !crate::platform::tokens::valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    let files = files::of(config);
    let entry = files.trash_entry_blocking(&format!("{app}-tokens"), at)?;
    let mut moved = Vec::new();
    let outcome = (|| {
        for (extension, text) in crate::platform::tokens::of(config).retire_blocking(app, at)? {
            files.trash_write_blocking(&entry, &format!("slug.{extension}"), text.as_bytes())?;
            moved.push(format!("{app}.{extension} (records)"));
        }
        if !files.by_generation() {
            for kind in crate::platform::tokens::Kind::ALL {
                let extension = kind.extension();
                if files.trash_move_blocking(&entry, &format!("{app}.{extension}"), &format!("slug.{extension}"))? {
                    moved.push(format!("{app}.{extension}"));
                }
            }
        }
        Ok(())
    })();
    if moved.is_empty() {
        files.trash_discard_blocking(&entry);
    }
    outcome.map(|()| moved)
}

/// Moves a slug's files out of the way. Returns what was moved, so a caller
/// can say what happened rather than only that it finished.
pub fn remove(config: &Config, slug: &str, at: u64) -> Result<Removed, String> {
    if !valid_slug(slug) {
        return Err(format!("invalid slug '{slug}'"));
    }
    let files = files::of(config);
    let entry = files.trash_entry_blocking(slug, at)?;
    let mut moved = Vec::new();
    let outcome = take(config, &*files, slug, at, &entry, &mut moved);
    if moved.is_empty() {
        files.trash_discard_blocking(&entry);
    }
    outcome?;
    if moved.is_empty() {
        return Err(format!("nothing published at '{slug}'"));
    }

    // An app's permissions go with it, so the next app or project at this
    // path starts with nobody on it. A copy stays with the files, so putting
    // the app back can put its people back too.
    if !slug.contains('/') {
        let project = files
            .trash_read_blocking(&entry, "slug.meta")?
            .or(files.trash_read_blocking(&entry, "app/index.meta")?)
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|meta| meta.get("project").and_then(|p| p.as_str()).map(str::to_string))
            .filter(|p| !p.is_empty());
        let path = match project {
            Some(project) => format!("{project}/{slug}"),
            None => slug.to_string(),
        };
        let kept = crate::accounts::users::forget_app(config, slug, &path)?;
        let text = serde_json::to_string_pretty(&kept).map_err(|e| e.to_string())?;
        files.trash_write_blocking(&entry, "permissions.json", text.as_bytes())?;
    }
    let record = serde_json::json!({ "slug": slug, "removed_at": at });
    files.trash_write_blocking(&entry, "removed.json", record.to_string().as_bytes())?;
    Ok(Removed { entry, moved })
}

/// The moving half of `remove`, into `entry`, adding to `moved` as it goes
/// so a failure part way still says what went.
fn take(config: &Config, files: &dyn Files, slug: &str, at: u64, entry: &str, moved: &mut Vec<String>) -> Result<(), String> {
    // An app's records and tokens go first: a removal that fails after
    // this leaves tokens that open nothing, never tokens that would open
    // the next app published at this name. On files they are sidecars,
    // moved below; on Postgres they are rows, written here as the sidecars
    // they would have been, so the trash reads the same either way.
    if !slug.contains('/') {
        let records = crate::platform::records::of(config).retire_blocking(slug, at)?;
        let tokens = crate::platform::tokens::of(config).retire_blocking(slug, at)?;
        for (extension, text) in records.into_iter().chain(tokens) {
            files.trash_write_blocking(entry, &format!("slug.{extension}"), text.as_bytes())?;
            moved.push(format!("{slug}.{extension} (records)"));
        }
    }

    // The files move in the app's turn, so no publish lands between them,
    // and on the bucket the app's generation moves on before the turn ends,
    // so no runner's cache serves what left.
    let app = files::app_of(slug).to_string();
    let turn = files.take_turn_blocking(&app)?;
    let before = moved.len();
    let outcome = (|| -> Result<(), String> {
        if files.trash_move_blocking(entry, slug, "app")? {
            moved.push(format!("{slug}/"));
        }
        for extension in SIDECARS {
            if files.by_generation() && !PUBLISHED.contains(&extension) {
                continue;
            }
            if files.trash_move_blocking(entry, &format!("{slug}.{extension}"), &format!("slug.{extension}"))? {
                moved.push(format!("{slug}.{extension}"));
            }
        }
        if files.by_generation() {
            // The app's database is still a file on this runner's volume:
            // it goes too, to the trash on the same volume, so the next app
            // at the name starts with none.
            let local = config.data_dir.join(slug);
            if local.is_dir() {
                let kept = config.data_dir.join(".trash").join(entry).join("app");
                if let Some(parent) = kept.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&local, &kept).map_err(|e| e.to_string())?;
                moved.push(format!("{slug}/ (data on this runner's volume)"));
            }
        }
        Ok(())
    })();
    // Counted even when a move failed part way: what did move is gone from
    // the bucket, and no runner's cache may go on serving it.
    let counted = if files.by_generation() && moved.len() > before {
        crate::state::wait(crate::content::catalog::of(config).bump_generation(&app)).map(|_| ())
    } else {
        Ok(())
    };
    turn.release_blocking();
    outcome?;
    counted?;

    // The files are gone, so whatever the app holds open goes now, before
    // the catalog step: a removal that fails there must not leave its
    // sockets and instance running.
    if !slug.contains('/') && !moved.is_empty() {
        use crate::state::events::{AppChange, AppEvents};
        config.app_events().app_changed(AppChange { app: slug, hidden: false, removed: true });
    }

    // What the catalog held for the slug and everything under it. On files
    // that is the sidecars just moved; on Postgres the rows, which are
    // written beside the files here so the trash reads the same either way.
    let retired = crate::content::catalog::of(config).retire_blocking(slug, at)?;
    for (page, meta, notes) in &retired.pages {
        let name = if page == slug { "slug".to_string() } else { format!("slug{}", page[slug.len()..].replace('/', "-")) };
        if let Some(meta) = meta {
            files.trash_write_blocking(entry, &format!("{name}.meta"), meta.as_bytes())?;
        }
        if let Some(notes) = notes {
            files.trash_write_blocking(entry, &format!("{name}.notes"), notes.as_bytes())?;
        }
        moved.push(format!("{page} (catalog)"));
    }
    Ok(())
}

/// Puts a removal back: its published files where they were, its meta and
/// notes, and on files the sidecars it took other than its tokens. Its
/// permissions and its tokens stay in the entry on either backend, and its
/// records on Postgres, for a person to put back on purpose: a token was
/// taken out of use by the removal, and a restore is not its holder's say.
/// Refused when anything is published at the slug now, asked in the app's
/// turn so no publish lands between the answer and the files going back.
pub fn restore(config: &Config, entry: &str) -> Result<Vec<String>, String> {
    let files = files::of(config);
    let record = files
        .trash_read_blocking(entry, "removed.json")?
        .ok_or_else(|| format!("{entry} is not a removal that can be put back"))?;
    let slug = serde_json::from_slice::<serde_json::Value>(&record)
        .ok()
        .and_then(|record| record.get("slug").and_then(|s| s.as_str()).map(str::to_string))
        .filter(|slug| valid_slug(slug))
        .ok_or_else(|| format!("{entry} does not say what it removed"))?;
    let app = files::app_of(&slug).to_string();

    let mut back = Vec::new();
    let turn = files.take_turn_blocking(&app)?;
    let outcome = (|| -> Result<(), String> {
        let in_use = if slug.contains('/') {
            files::path_blocking(config, &format!("{slug}.html")).is_some()
                || files::path_blocking(config, &format!("{slug}/index.html")).is_some()
        } else {
            files::app_exists_blocking(config, &slug)
        };
        if in_use {
            return Err(format!("something is published at {slug} now; remove it before putting {entry} back"));
        }
        if files.untrash_blocking(entry, "app", &slug)? {
            back.push(format!("{slug}/"));
        }
        let tokens = crate::platform::tokens::Kind::ALL.map(|kind| kind.extension());
        for extension in SIDECARS {
            if (files.by_generation() && !PUBLISHED.contains(&extension)) || tokens.contains(&extension) {
                continue;
            }
            if files.untrash_blocking(entry, &format!("slug.{extension}"), &format!("{slug}.{extension}"))? {
                back.push(format!("{slug}.{extension}"));
            }
        }
        if files.by_generation() {
            let kept = config.data_dir.join(".trash").join(entry).join("app");
            let local = config.data_dir.join(&slug);
            if kept.is_dir() && !local.exists() {
                if let Some(parent) = local.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&kept, &local).map_err(|e| e.to_string())?;
                back.push(format!("{slug}/ (data on this runner's volume)"));
            }
            // The rows a removal took went into the entry as sidecars.
            if let Some(meta) = files.trash_read_blocking(entry, "slug.meta")? {
                let meta: crate::content::store::PageMeta =
                    serde_json::from_slice(&meta).map_err(|e| format!("{entry}'s meta could not be read: {e}"))?;
                crate::content::catalog::of(config).update_meta_blocking(
                    &slug,
                    Box::new(move |stored| {
                        *stored = meta;
                        Ok(())
                    }),
                )?;
                back.push(format!("{slug} (meta)"));
            }
            if let Some(notes) = files.trash_read_blocking(entry, "slug.notes")? {
                let notes = String::from_utf8_lossy(&notes).into_owned();
                crate::state::wait(crate::content::catalog::of(config).set_notes(&slug, &notes))?;
            }
        }
        Ok(())
    })();
    // Counted whenever anything came back, a restore that failed part way
    // included, so every runner reads what is in the bucket now.
    let counted = if files.by_generation() && !back.is_empty() {
        crate::state::wait(crate::content::catalog::of(config).bump_generation(&app)).map(|_| ())
    } else {
        Ok(())
    };
    turn.release_blocking();
    outcome?;
    counted?;
    if back.is_empty() {
        return Err(format!("{entry} holds nothing to put back"));
    }
    Ok(back)
}

/// Moves only the single page at a slug, leaving an app of the same name.
/// This is the shape of the mess an accidental page upload makes.
pub fn remove_page_only(config: &Config, slug: &str, at: u64) -> Result<Removed, String> {
    if !valid_slug(slug) {
        return Err(format!("invalid slug '{slug}'"));
    }
    let files = files::of(config);
    let key = format!("{slug}.html");
    if files::path_blocking(config, &key).is_none() {
        return Err(format!("no single page at '{slug}'"));
    }
    let entry = files.trash_entry_blocking(slug, at)?;
    let app = files::app_of(slug).to_string();
    let turn = files.take_turn_blocking(&app)?;
    let outcome = (|| {
        let moved = files.trash_move_blocking(&entry, &key, "slug.html")?;
        if moved && files.by_generation() {
            crate::state::wait(crate::content::catalog::of(config).bump_generation(&app))?;
        }
        Ok::<bool, String>(moved)
    })();
    turn.release_blocking();
    if !outcome? {
        files.trash_discard_blocking(&entry);
        return Err(format!("no single page at '{slug}'"));
    }
    let record = serde_json::json!({ "slug": slug, "removed_at": at, "page_only": true });
    files.trash_write_blocking(&entry, "removed.json", record.to_string().as_bytes())?;
    Ok(Removed { entry, moved: vec![key] })
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

    #[test]
    fn a_removed_app_leaves_the_site_but_not_the_disk() {
        let (_t, config) = config();
        std::fs::create_dir_all(config.data_dir.join("app")).unwrap();
        std::fs::write(config.data_dir.join("app/index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(config.data_dir.join("app.meta"), "{}").unwrap();
        std::fs::write(config.data_dir.join("app.notes"), "why it existed").unwrap();

        let removed = remove(&config, "app", 1_000).unwrap();
        assert_eq!(removed.moved.len(), 3, "{removed:?}");
        assert_eq!(removed.entry, "1000-app");
        assert!(!config.data_dir.join("app").exists());
        assert!(!config.data_dir.join("app.meta").exists());

        // Still there for whoever regrets it.
        let kept = config.data_dir.join(".trash/1000-app");
        assert!(kept.join("app/index.html").exists());
        assert!(kept.join("slug.notes").exists());
    }

    #[test]
    fn removing_a_page_leaves_an_app_of_the_same_name_alone() {
        let (_t, config) = config();
        // Exactly the mess a probe published as a page makes: a page and an
        // app sharing a slug, where only the page should go.
        std::fs::write(config.data_dir.join("releases.html"), "slug = \"releases\"").unwrap();
        std::fs::create_dir_all(config.data_dir.join("releases")).unwrap();
        std::fs::write(
            config.data_dir.join("releases/index.html"),
            "<title>Release watcher</title>",
        )
        .unwrap();

        remove_page_only(&config, "releases", 2_000).unwrap();
        assert!(!config.data_dir.join("releases.html").exists());
        assert!(
            config.data_dir.join("releases/index.html").exists(),
            "the app went with the page"
        );
    }

    #[test]
    fn removing_twice_does_not_overwrite_the_first_removal() {
        let (_t, config) = config();
        std::fs::write(config.data_dir.join("page.html"), "first").unwrap();
        remove(&config, "page", 10).unwrap();
        std::fs::write(config.data_dir.join("page.html"), "second").unwrap();
        remove(&config, "page", 20).unwrap();

        assert_eq!(
            std::fs::read_to_string(config.data_dir.join(".trash/10-page/slug.html")).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(config.data_dir.join(".trash/20-page/slug.html")).unwrap(),
            "second"
        );
    }

    #[test]
    fn nothing_published_is_said_rather_than_silently_succeeding() {
        let (_t, config) = config();
        assert!(remove(&config, "never-existed", 1).is_err());
        assert!(remove(&config, "../etc", 1).is_err());
    }
}
