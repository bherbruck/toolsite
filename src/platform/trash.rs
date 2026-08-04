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

use crate::{config::Config, content::slug::valid_slug};
use std::path::PathBuf;

/// Everything that can belong to one slug, beyond its own directory.
const SIDECARS: [&str; 8] = [
    "html", "meta", "icon", "notes", "source", "secrets", "jobs", "migrations",
];

fn trash_dir(config: &Config) -> PathBuf {
    config.data_dir.join(".trash")
}

/// Moves a slug's files out of the way. Returns what was moved, so a caller
/// can say what happened rather than only that it finished.
pub fn remove(config: &Config, slug: &str, at: u64) -> Result<Vec<String>, String> {
    if !valid_slug(slug) {
        return Err(format!("invalid slug '{slug}'"));
    }

    // Timestamped so removing the same slug twice does not overwrite the
    // first removal, which would be destroying data by another route.
    let destination = trash_dir(config).join(format!("{at}-{}", slug.replace('/', "-")));
    let mut moved = Vec::new();

    let app_dir = config.data_dir.join(slug);
    if app_dir.is_dir() {
        std::fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
        std::fs::rename(&app_dir, destination.join("app")).map_err(|e| e.to_string())?;
        moved.push(format!("{slug}/"));
    }

    for extension in SIDECARS {
        let path = config.data_dir.join(format!("{slug}.{extension}"));
        if path.is_file() {
            std::fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
            std::fs::rename(&path, destination.join(format!("slug.{extension}")))
                .map_err(|e| e.to_string())?;
            moved.push(format!("{slug}.{extension}"));
        }
    }

    if moved.is_empty() {
        return Err(format!("nothing published at '{slug}'"));
    }
    Ok(moved)
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

        let moved = remove(&config, "app", 1_000).unwrap();
        assert_eq!(moved.len(), 3, "{moved:?}");
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

/// Moves only the single page at a slug, leaving an app of the same name.
/// This is the shape of the mess an accidental page upload makes.
pub fn remove_page_only(config: &Config, slug: &str, at: u64) -> Result<Vec<String>, String> {
    if !valid_slug(slug) {
        return Err(format!("invalid slug '{slug}'"));
    }
    let page = config.data_dir.join(format!("{slug}.html"));
    if !page.is_file() {
        return Err(format!("no single page at '{slug}'"));
    }
    let destination = trash_dir(config).join(format!("{at}-{}", slug.replace('/', "-")));
    std::fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
    std::fs::rename(&page, destination.join("slug.html")).map_err(|e| e.to_string())?;
    Ok(vec![format!("{slug}.html")])
}
