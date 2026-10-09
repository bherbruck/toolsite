//! One suite every `Files` store must pass, run on the volume always and
//! on a bucket when `TOOLSITE_TEST_S3_ENDPOINT` names one
//! (`scripts/test-postgres.sh` starts MinIO beside Postgres). It pins what
//! serving and publishing rest on: a key comes back as it went in; nothing
//! that is not a key reaches a path or an object, the forged traversal
//! archives `bundle.rs` refuses included; a bundle overlays; a generation's
//! reads never change under it and a newer one never sees an older one's
//! cache; the trash keeps what it takes and gives it back; an upload's
//! pieces meet whichever runner sent them; and one app's writers take
//! turns.

use super::{bucket::Bucket, local::Local, slugs_in, Entry, Files};
use crate::{
    accounts::store::conformance::{drop_postgres_database, postgres_database},
    content::bundle::forged::tarball,
    runtime::blobs::S3,
};
use std::{path::Path, sync::Arc, time::Duration};

/// Runs a blocking store call on a blocking thread, as the server does.
async fn blocking<T: Send + 'static>(files: &Arc<dyn Files>, call: impl FnOnce(&dyn Files) -> T + Send + 'static) -> T {
    let files = files.clone();
    tokio::task::spawn_blocking(move || call(&*files)).await.unwrap()
}

async fn text(files: &dyn Files, key: &str, generation: u64) -> Option<String> {
    let path = files.local(key, generation).await.unwrap()?;
    Some(std::fs::read_to_string(path).unwrap())
}

async fn a_key_comes_back_as_it_went_in(files: &dyn Files) {
    files.put("note.html", "<title>Note</title>".into()).await.unwrap();
    files.put("shop/assets/app-4f2a.js", "console.log(1)".into()).await.unwrap();
    assert_eq!(text(files, "note.html", 1).await.as_deref(), Some("<title>Note</title>"));
    assert_eq!(text(files, "shop/assets/app-4f2a.js", 1).await.as_deref(), Some("console.log(1)"));
    assert_eq!(files.local("shop/assets/missing.js", 1).await.unwrap(), None);
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    files.put("shop/handler.wasm", binary.clone().into()).await.unwrap();
    let path = files.local("shop/handler.wasm", 1).await.unwrap().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), binary);

    assert!(files.delete("note.html").await.unwrap());
    assert!(!files.delete("note.html").await.unwrap(), "a second delete found something");
    assert_eq!(files.local("note.html", 2).await.unwrap(), None);
}

/// Every way a hand-made name could reach somewhere else: climbing out,
/// absolute, the platform's own places on the volume and in the bucket, an
/// app's hidden files, and the empty name.
const NOT_KEYS: [&str; 12] = [
    "..",
    "../escape.html",
    "shop/../../escape.html",
    "/etc/passwd",
    ".toolsite/content/shop/index.html",
    ".toolsite/trash/x/app/index.html",
    ".site/auth.db",
    ".trash/1-old/slug.html",
    "shop/.blobs/data/secret",
    "shop//index.html",
    "",
    "shop/./index.html",
];

async fn nothing_that_is_not_a_key_is_touched(files: &Arc<dyn Files>) {
    for key in NOT_KEYS {
        assert!(files.put(key, "pwned".into()).await.is_err(), "put {key:?}");
        assert!(files.local(key, 1).await.is_err(), "read {key:?}");
        assert!(files.delete(key).await.is_err(), "delete {key:?}");
        assert!(files.exists(key, 1).await.is_err(), "exists {key:?}");
        let owned = key.to_string();
        assert!(blocking(files, move |f| f.trash_move_blocking("1-x", &owned, "app")).await.is_err(), "trash {key:?}");
        let owned = key.to_string();
        assert!(blocking(files, move |f| f.untrash_blocking("1-x", "app", &owned)).await.is_err(), "untrash {key:?}");
        let owned = key.to_string();
        assert!(blocking(files, move |f| f.trash_write_blocking("1-x", &owned, b"x")).await.is_err(), "trash name {key:?}");
    }
    for entry in ["..", "../x", ".x", "a/b", ""] {
        let owned = entry.to_string();
        assert!(blocking(files, move |f| f.trash_write_blocking(&owned, "slug.html", b"x")).await.is_err(), "entry {entry:?}");
    }
    for upload in ["..", "../../x", "/", "not-hex"] {
        let owned = upload.to_string();
        assert!(blocking(files, move |f| f.chunk_put_blocking(&owned, 0, b"x")).await.is_err(), "upload {upload:?}");
    }
    assert!(files.list("../").await.is_err());
    assert!(files.list(".toolsite").await.is_err());
}

/// The forged archives of `bundle.rs`, against this store: one unsafe path
/// refuses the whole bundle before anything is stored, and what is skipped
/// is skipped here too.
async fn forged_bundles_store_nothing_they_should_not(files: &Arc<dyn Files>) {
    for path in ["../escape.html", "a/../../escape.html", "/etc/escape.html"] {
        let body = tarball(&[("index.html", "ok"), (path, "pwned")]);
        let error = blocking(files, move |f| f.put_bundle_blocking("forged", &body).map(|_| ())).await.unwrap_err();
        assert!(error.contains("unsafe path"), "{path:?} gave {error:?}");
    }
    assert!(files.list("forged").await.unwrap().is_empty(), "a refused bundle stored something");
    let top = files.list("").await.unwrap();
    assert!(!top.iter().any(|entry| entry.name.contains("escape")), "{top:?}");

    let body = tarball(&[
        ("index.html", "ok"),
        ("passwd.html@", ""),
        (".env", "SECRET=1"),
        ("handler.wasm", "not from a bundle"),
        ("data.db", "rows"),
        ("index.meta", r#"{"gate":"public"}"#),
        ("inner/page.meta", "{}"),
    ]);
    let unpacked = blocking(files, move |f| f.put_bundle_blocking("forged", &body)).await.unwrap();
    assert_eq!(unpacked.files, ["index.html"]);
    assert!(unpacked.skipped.contains(&"symlinks and special files"));
    assert!(unpacked.skipped.contains(&"dotfiles"));
    let listed = files.list("forged").await.unwrap();
    assert_eq!(listed, vec![Entry { name: "index.html".into(), dir: false }]);
    for key in ["forged/handler.wasm", "forged/data.db", "forged/index.meta", "forged/passwd.html"] {
        assert_eq!(files.local(key, 9).await.unwrap(), None, "{key} was stored");
    }
}

/// A bundle replaces the files it carries and keeps the rest, as it always
/// has; a wrapping directory is stripped.
async fn a_bundle_overlays(files: &Arc<dyn Files>) {
    let first = tarball(&[("dist/index.html", "one"), ("dist/assets/a.js", "a")]);
    blocking(files, move |f| f.put_bundle_blocking("layered", &first)).await.unwrap();
    let second = tarball(&[("index.html", "two"), ("assets/b.js", "b")]);
    blocking(files, move |f| f.put_bundle_blocking("layered", &second)).await.unwrap();
    assert_eq!(text(&**files, "layered/index.html", 20).await.as_deref(), Some("two"));
    assert_eq!(text(&**files, "layered/assets/a.js", 20).await.as_deref(), Some("a"));
    assert_eq!(text(&**files, "layered/assets/b.js", 20).await.as_deref(), Some("b"));
}

/// What the index lists: a loose page, an app by its root, and the pages
/// of a directory with no index of its own; never the platform's places.
async fn published_files_are_listed(files: &Arc<dyn Files>) {
    files.put("listed-note.html", "<title>Note</title>".into()).await.unwrap();
    files.put("board/index.html", "x".into()).await.unwrap();
    files.put("board/inner/page.html", "x".into()).await.unwrap();
    files.put("group/first.html", "x".into()).await.unwrap();
    blocking(files, |f| f.trash_write_blocking("1-old", "slug.html", b"x")).await.unwrap();
    let slugs = slugs_in(&**files).await.unwrap();
    for listed in ["board", "group/first", "listed-note"] {
        assert!(slugs.iter().any(|slug| slug == listed), "{listed} was not listed: {slugs:?}");
    }
    assert!(
        !slugs.iter().any(|slug| slug.starts_with("board/") || slug.starts_with('.') || slug.contains("slug")),
        "an app's inner page or the trash was listed: {slugs:?}"
    );
    let top = files.list("").await.unwrap();
    assert!(top.contains(&Entry { name: "board".into(), dir: true }));
    assert!(top.contains(&Entry { name: "listed-note.html".into(), dir: false }));
    assert!(!top.iter().any(|entry| entry.name.starts_with('.')), "{top:?}");
}

/// The trash keeps what it takes, under an entry no other removal shares,
/// and gives it back only where nothing stands now.
async fn the_trash_keeps_and_gives_back(files: &Arc<dyn Files>) {
    files.put("binned/index.html", "app".into()).await.unwrap();
    files.put("binned/assets/x.js", "x".into()).await.unwrap();
    files.put("binned.icon", "B".into()).await.unwrap();
    let (entry, again) = blocking(files, |f| {
        (f.trash_entry_blocking("binned", 50).unwrap(), f.trash_entry_blocking("binned", 50).unwrap())
    })
    .await;
    assert_ne!(entry, again, "two removals in one second shared an entry");
    let (owned, other) = (entry.clone(), again.clone());
    blocking(files, move |f| {
        f.trash_discard_blocking(&other);
        assert!(f.trash_move_blocking(&owned, "binned", "app").unwrap());
        assert!(f.trash_move_blocking(&owned, "binned.icon", "slug.icon").unwrap());
        assert!(!f.trash_move_blocking(&owned, "binned.source", "slug.source").unwrap(), "moved what was never there");
        f.trash_write_blocking(&owned, "slug.meta", br#"{"hidden":true}"#).unwrap();
    })
    .await;
    assert_eq!(files.local("binned/index.html", 30).await.unwrap(), None, "the app stayed published");
    assert_eq!(files.local("binned.icon", 30).await.unwrap(), None);
    assert!(!files.list("").await.unwrap().iter().any(|e| e.name == "binned"));

    let owned = entry.clone();
    let entries = blocking(files, move |f| {
        assert_eq!(f.trash_read_blocking(&owned, "app/assets/x.js").unwrap().as_deref(), Some(&b"x"[..]));
        assert_eq!(f.trash_read_blocking(&owned, "slug.meta").unwrap().as_deref(), Some(&br#"{"hidden":true}"#[..]));
        f.trash_entries_blocking().unwrap()
    })
    .await;
    assert!(entries.contains(&entry), "{entries:?}");

    // Something stands at the name now: nothing is put back over it.
    files.put("binned/index.html", "newcomer".into()).await.unwrap();
    let owned = entry.clone();
    let refused = blocking(files, move |f| f.untrash_blocking(&owned, "app", "binned")).await;
    assert!(refused.is_err(), "the trash was put back over a newer app");
    assert_eq!(text(&**files, "binned/index.html", 31).await.as_deref(), Some("newcomer"));
    assert!(files.delete("binned/index.html").await.unwrap());

    let owned = entry.clone();
    blocking(files, move |f| {
        assert!(f.untrash_blocking(&owned, "app", "binned").unwrap());
        assert!(f.untrash_blocking(&owned, "slug.icon", "binned.icon").unwrap());
        assert!(!f.untrash_blocking(&owned, "slug.source", "binned.source").unwrap());
    })
    .await;
    assert_eq!(text(&**files, "binned/assets/x.js", 32).await.as_deref(), Some("x"));
    assert_eq!(text(&**files, "binned.icon", 32).await.as_deref(), Some("B"));
}

/// An upload's pieces, stored by one handle and read by another, as two
/// runners would.
async fn pieces_meet_whoever_reads_them(writer: &Arc<dyn Files>, reader: &Arc<dyn Files>) {
    let upload = "0123abcd".to_string();
    let owned = upload.clone();
    blocking(writer, move |f| {
        f.chunk_put_blocking(&owned, 1, b"second").unwrap();
        f.chunk_put_blocking(&owned, 0, b"first").unwrap();
    })
    .await;
    let owned = upload.clone();
    blocking(reader, move |f| {
        assert_eq!(f.chunk_read_blocking(&owned, 0).unwrap().as_deref(), Some(&b"first"[..]));
        assert_eq!(f.chunk_read_blocking(&owned, 1).unwrap().as_deref(), Some(&b"second"[..]));
        assert_eq!(f.chunk_read_blocking(&owned, 2).unwrap(), None);
        // A young upload survives a sweep; a cleared one is gone.
        f.chunks_sweep_blocking(Duration::from_secs(3600));
        assert!(f.chunk_read_blocking(&owned, 0).unwrap().is_some(), "a sweep took a live upload");
        f.chunks_clear_blocking(&owned);
        assert_eq!(f.chunk_read_blocking(&owned, 0).unwrap(), None);
    })
    .await;
}

/// One app's writers take turns, here and through any other handle on the
/// same site; another app's writer never waits on them.
async fn writers_take_turns(first: &Arc<dyn Files>, second: &Arc<dyn Files>) {
    let held = first.take_turn("turns").await.unwrap();
    let other = second.clone();
    let mut waiting = tokio::spawn(async move { other.take_turn("turns").await.unwrap() });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut waiting).await.is_err(),
        "a second writer got the app's turn while the first held it"
    );
    let elsewhere = tokio::time::timeout(Duration::from_secs(5), second.take_turn("other-app")).await;
    elsewhere.expect("another app's writer waited").unwrap().release().await;
    held.release().await;
    let next = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the turn was never passed on").unwrap();
    next.release().await;
}

/// Another runner holding the turns of as many apps as this process lets
/// publish at once leaves this runner free to publish any other: a turn
/// waiting on another runner's lock holds no place here meanwhile.
async fn waiting_on_another_runner_holds_no_place(files: &Arc<dyn Files>, pool: &deadpool_postgres::Pool) {
    let busy: Vec<String> = (0..super::bucket::MAX_TURNS + 2).map(|n| format!("busy-{n}")).collect();
    let mut elsewhere = Vec::new();
    for app in &busy {
        let client = pool.get().await.unwrap();
        client
            .execute("select pg_advisory_lock($1::int4, hashtext($2))", &[&crate::state::pg::LOCK_PUBLISH, app])
            .await
            .unwrap();
        elsewhere.push(client);
    }
    let waiting: Vec<_> = busy
        .iter()
        .map(|app| {
            let (files, app) = (files.clone(), app.clone());
            tokio::spawn(async move { files.take_turn(&app).await.unwrap().release().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let free = tokio::time::timeout(Duration::from_secs(5), files.take_turn("bystander")).await;
    free.expect("an app waited behind other apps' turns held on another runner").unwrap().release().await;
    let blocking = files.clone();
    let free = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || blocking.take_turn_blocking("bystander").map(|turn| turn.release_blocking())),
    )
    .await;
    free.expect("a blocking writer waited behind other apps' turns held on another runner").unwrap().unwrap();
    for (client, app) in elsewhere.iter().zip(&busy) {
        client
            .execute("select pg_advisory_unlock($1::int4, hashtext($2))", &[&crate::state::pg::LOCK_PUBLISH, app])
            .await
            .unwrap();
    }
    for turn in waiting {
        tokio::time::timeout(Duration::from_secs(10), turn).await.expect("a turn was never passed on").unwrap();
    }
}

async fn run(files: Arc<dyn Files>) {
    a_key_comes_back_as_it_went_in(&*files).await;
    nothing_that_is_not_a_key_is_touched(&files).await;
    forged_bundles_store_nothing_they_should_not(&files).await;
    a_bundle_overlays(&files).await;
    published_files_are_listed(&files).await;
    the_trash_keeps_and_gives_back(&files).await;
}

/// The cache's files are named by digest under `.tmp/content`, so nothing
/// in them repeats a key, which no slug or bundle path could name anyway.
fn cache_names_are_digests(data_dir: &Path) {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                walk(&entry.path(), out);
            } else {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let mut names = Vec::new();
    walk(&data_dir.join(".tmp").join("content"), &mut names);
    assert!(!names.is_empty(), "nothing was cached");
    for name in names {
        assert!(name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()), "a cached file named {name}");
    }
}

/// Reads at one generation never change under it, and a newer one never
/// reaches an older one's cache, whether the key was there or not.
async fn a_generation_reads_what_it_read(files: &dyn Files) {
    files.put("cached/index.html", "first".into()).await.unwrap();
    assert_eq!(text(files, "cached/index.html", 1).await.as_deref(), Some("first"));
    assert_eq!(files.local("cached/late.js", 1).await.unwrap(), None);
    files.put("cached/index.html", "second".into()).await.unwrap();
    files.put("cached/late.js", "late".into()).await.unwrap();
    assert_eq!(text(files, "cached/index.html", 1).await.as_deref(), Some("first"), "a generation's read changed");
    assert_eq!(files.local("cached/late.js", 1).await.unwrap(), None);
    assert_eq!(text(files, "cached/index.html", 2).await.as_deref(), Some("second"));
    assert_eq!(text(files, "cached/late.js", 2).await.as_deref(), Some("late"));
    // Generation 0 is an app with nothing published, or taken away.
    assert_eq!(files.local("cached/index.html", 0).await.unwrap(), None);
    assert!(!files.exists("cached", 0).await.unwrap());
    assert!(files.exists("cached", 2).await.unwrap());
}

/// Sets a cached generation's time back past the sweep's grace, as if
/// nothing had read it for a while.
fn backdate(dir: &Path) {
    let then = std::time::SystemTime::now() - Duration::from_secs(600);
    std::fs::File::open(dir).unwrap().set_modified(then).unwrap();
}

/// Many publishes leave one generation's cache on the disk, not one per
/// publish, and an app removed since leaves none once it is asked for.
async fn the_cache_keeps_no_old_generation(files: &dyn Files, data_dir: &Path) {
    let app_dir = data_dir.join(".tmp").join("content").join("swept");
    for generation in 1..=40u64 {
        files.put("swept/index.html", format!("v{generation}").into()).await.unwrap();
        assert_eq!(text(files, "swept/index.html", generation).await, Some(format!("v{generation}")));
        backdate(&app_dir.join(generation.to_string()));
    }
    let kept: Vec<_> = std::fs::read_dir(&app_dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(kept.len() <= 2, "40 publishes left {} generations cached: {kept:?}", kept.len());
    // Removed: its generation is 0 from now on, and the next read takes
    // what this runner kept of it.
    assert_eq!(files.local("swept/index.html", 0).await.unwrap(), None);
    assert!(!app_dir.exists(), "a removed app's cache stayed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_files_conform() {
    let dir = tempfile::tempdir().unwrap();
    let files: Arc<dyn Files> = Arc::new(Local::new(dir.path().to_path_buf()));
    run(files.clone()).await;
    pieces_meet_whoever_reads_them(&files, &files).await;
    let second: Arc<dyn Files> = Arc::new(Local::new(dir.path().to_path_buf()));
    writers_take_turns(&files, &second).await;
    // The volume reads a file as it is now, whatever generation is asked.
    files.put("now/index.html", "one".into()).await.unwrap();
    assert_eq!(text(&*files, "now/index.html", 0).await.as_deref(), Some("one"));
    assert!(!files.by_generation());
    // A removal into the trash leaves the trash where it always was.
    assert!(dir.path().join(".trash").is_dir());
}

/// A bucket of its own on the test server, made now.
pub(crate) fn test_bucket() -> S3 {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} unset; scripts/test-postgres.sh sets it"));
    let name = format!("t-{}", crate::content::slug::random_token(12));
    let s3 = S3::new(
        &var("TOOLSITE_TEST_S3_ENDPOINT"),
        &name,
        "us-east-1",
        &var("TOOLSITE_TEST_S3_ACCESS_KEY_ID"),
        &var("TOOLSITE_TEST_S3_SECRET_ACCESS_KEY"),
        true,
    )
    .unwrap();
    let making = s3.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(making.ensure_bucket())
    })
    .join()
    .unwrap()
    .unwrap();
    s3
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn the_bucket_files_conform_on_postgres() {
    let s3 = test_bucket();
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    // Two runners: one bucket and one database, a volume each.
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let files: Arc<dyn Files> = Arc::new(Bucket::new(Some(s3.clone()), here.path().to_path_buf(), Some(pool.clone())));
    let other: Arc<dyn Files> = Arc::new(Bucket::new(Some(s3.clone()), there.path().to_path_buf(), Some(pool.clone())));
    run(files.clone()).await;
    a_generation_reads_what_it_read(&*files).await;
    cache_names_are_digests(here.path());
    the_cache_keeps_no_old_generation(&*files, here.path()).await;
    pieces_meet_whoever_reads_them(&files, &other).await;
    writers_take_turns(&files, &other).await;
    waiting_on_another_runner_holds_no_place(&files, &pool).await;
    assert!(files.by_generation());

    // Published files are the bucket's: the volume holds a cache and
    // nothing else, and the trash and the pieces live in the bucket too.
    for place in [".trash", "shop", "note.html", ".tmp/inline"] {
        assert!(!here.path().join(place).exists(), "{place} was written to the volume");
    }
    let listing = s3.object_list("", false).await.unwrap();
    assert!(listing.objects.iter().all(|(key, _)| key.starts_with(".toolsite/")), "an object outside .toolsite/");

    // A store with no bucket refuses rather than falling back to the volume.
    let none: Arc<dyn Files> = Arc::new(Bucket::new(None, here.path().to_path_buf(), None));
    assert!(none.put("shop/index.html", "x".into()).await.unwrap_err().contains("bucket"));
    assert!(none.local("shop/index.html", 1).await.is_err());

    drop((files, other));
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}
