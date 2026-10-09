//! One suite every catalog must pass, run on files always and on Postgres
//! when `TOOLSITE_TEST_DATABASE_URL` names a server
//! (`scripts/test-postgres.sh` starts one). It pins what the rules above
//! rely on: a meta comes back exactly as it went in, field by field; one
//! slug's meta never answers for another's; an edit that fails changes
//! nothing; and of many changes at once, every one lands.

use super::{files::Files, postgres::Postgres, Catalog};
use crate::{
    accounts::store::conformance::{drop_postgres_database, postgres_database},
    content::store::{PageMeta, PathRule, Policy, PortProtocol, PortSocket, ResidentMeta},
    runtime::limits::Asked,
};
use std::{path::Path, sync::Arc};

/// Values that have broken stores before: quotes and SQL, a NUL, Unicode
/// lookalikes, and a long string.
fn hostile() -> Vec<String> {
    vec![
        "'); drop table platform.pages; --".to_string(),
        "nul\u{0}inside".to_string(),
        "\u{2215}etc\u{2215}passwd \u{0430}dmin \u{1F512}".to_string(),
        "x".repeat(64 * 1024),
    ]
}

/// A meta with every field set away from its default.
fn everything() -> PageMeta {
    let mut protocols = std::collections::BTreeMap::new();
    protocols.insert("/live".to_string(), vec!["mqtt".to_string(), "v2.json".to_string()]);
    PageMeta {
        listed: false,
        hidden: true,
        spa: true,
        gate: Some("authenticated".into()),
        allow_http: vec!["api.example.com".into(), hostile()[0].clone()],
        rules: vec![
            PathRule { prefix: "/admin".into(), gate: "restricted".into() },
            PathRule { prefix: "/public".into(), gate: "public".into() },
        ],
        roles: vec!["clerk".into(), hostile()[2].clone()],
        project: Some("ops/yard".into()),
        created_by: Some("user-1".into()),
        queryable: vec!["open_orders".into()],
        policies: vec![Policy {
            table: "orders".into(),
            view: "my_orders".into(),
            where_: "owner = current_user() and note <> 'x'".into(),
            owner: Some("owner".into()),
            write: true,
        }],
        generated: vec!["my_orders".into(), "ts_abc_my_orders".into()],
        access_salt: Some("abc123".into()),
        sockets: vec!["/live".into(), "/feed".into()],
        socket_protocols: protocols,
        ports: vec![
            PortSocket { protocol: PortProtocol::Tcp, port: 1883 },
            PortSocket { protocol: PortProtocol::Udp, port: 5514 },
        ],
        resident: Some(ResidentMeta { memory_mb: Some(64), tick_ms: Some(250) }),
        limits: Some(Asked { request_fuel: Some(u64::MAX), job_seconds: Some(600), query_rows: Some(5), ..Default::default() }),
        label: Some("orders-1a2b3c4d".into()),
    }
}

fn json(meta: &PageMeta) -> serde_json::Value {
    serde_json::to_value(meta).unwrap()
}

async fn set(catalog: &dyn Catalog, slug: &str, meta: PageMeta) -> PageMeta {
    catalog
        .update_meta(slug, Box::new(move |stored| {
            *stored = meta;
            Ok(())
        }))
        .await
        .unwrap()
}

async fn every_field_round_trips(catalog: &dyn Catalog) {
    assert_eq!(json(&catalog.meta("nobody").await.unwrap()), json(&PageMeta::default()));
    let wanted = everything();
    let stored = set(catalog, "orders", wanted.clone()).await;
    assert_eq!(json(&stored), json(&wanted));
    assert_eq!(json(&catalog.meta("orders").await.unwrap()), json(&wanted));
    assert_eq!(json(&catalog.meta_blocking("orders").unwrap()), json(&wanted));

    // Back to the defaults: nothing of the old meta lingers.
    let stored = set(catalog, "orders", PageMeta::default()).await;
    assert_eq!(json(&stored), json(&PageMeta::default()));
    assert_eq!(json(&catalog.meta("orders").await.unwrap()), json(&PageMeta::default()));

    // Every hostile value, in a field that is free text.
    for (n, value) in hostile().into_iter().enumerate() {
        let slug = format!("hostile-{n}");
        let meta = PageMeta { project: Some(value.clone()), roles: vec![value.clone()], ..PageMeta::default() };
        set(catalog, &slug, meta).await;
        let back = catalog.meta(&slug).await.unwrap();
        assert_eq!(back.project.as_deref(), Some(value.as_str()));
        assert_eq!(back.roles, vec![value]);
    }
}

async fn slugs_are_apart(catalog: &dyn Catalog) {
    set(catalog, "shop", PageMeta { gate: Some("public".into()), ..PageMeta::default() }).await;
    set(catalog, "shop/back", PageMeta { hidden: true, ..PageMeta::default() }).await;
    set(catalog, "shop_", PageMeta { gate: Some("restricted".into()), ..PageMeta::default() }).await;
    assert_eq!(catalog.meta("shop").await.unwrap().gate.as_deref(), Some("public"));
    assert!(!catalog.meta("shop").await.unwrap().hidden);
    assert!(catalog.meta("shop/back").await.unwrap().hidden);
    assert_eq!(catalog.meta("shop_").await.unwrap().gate.as_deref(), Some("restricted"));
    assert_eq!(catalog.meta("Shop").await.unwrap().gate, None, "slugs are case-sensitive");
}

async fn old_words_come_back_current(catalog: &dyn Catalog) {
    let meta = PageMeta {
        gate: Some("granted".into()),
        rules: vec![PathRule { prefix: "/a".into(), gate: "signed-in".into() }],
        ..PageMeta::default()
    };
    let stored = set(catalog, "legacy", meta).await;
    assert_eq!(stored.gate.as_deref(), Some("restricted"));
    assert_eq!(stored.rules[0].gate, "authenticated");
    assert_eq!(catalog.meta("legacy").await.unwrap().gate.as_deref(), Some("restricted"));
}

async fn a_failed_edit_changes_nothing(catalog: &dyn Catalog) {
    set(catalog, "kept", PageMeta { gate: Some("restricted".into()), ..PageMeta::default() }).await;
    let refused = catalog
        .update_meta("kept", Box::new(|meta| {
            meta.gate = Some("public".into());
            meta.hidden = true;
            Err("refused".into())
        }))
        .await;
    assert_eq!(refused.unwrap_err(), "refused");
    let meta = catalog.meta("kept").await.unwrap();
    assert_eq!(meta.gate.as_deref(), Some("restricted"));
    assert!(!meta.hidden);
}

/// Many writers at once, each adding its own entry, async and blocking
/// mixed: every entry is there after. Without the hold, a writer's read
/// misses the entries written between it and its write, and they are lost.
async fn concurrent_changes_all_land(catalog: Arc<dyn Catalog>, slug: &str) {
    const WRITERS: usize = 24;
    const EACH: usize = 5;
    let start = Arc::new(tokio::sync::Barrier::new(WRITERS));
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let (catalog, start, slug) = (catalog.clone(), start.clone(), slug.to_string());
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            for n in 0..EACH {
                let entry = format!("host-{writer}-{n}");
                if writer % 2 == 0 {
                    catalog
                        .update_meta(&slug, Box::new(move |meta| {
                            meta.allow_http.push(entry);
                            Ok(())
                        }))
                        .await
                        .unwrap();
                } else {
                    let (catalog, slug) = (catalog.clone(), slug.clone());
                    tokio::task::spawn_blocking(move || {
                        catalog
                            .update_meta_blocking(&slug, Box::new(move |meta| {
                                meta.allow_http.push(entry);
                                Ok(())
                            }))
                            .unwrap()
                    })
                    .await
                    .unwrap();
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let mut landed = catalog.meta(slug).await.unwrap().allow_http;
    landed.sort();
    let mut wanted: Vec<String> = (0..WRITERS).flat_map(|w| (0..EACH).map(move |n| format!("host-{w}-{n}"))).collect();
    wanted.sort();
    assert_eq!(landed.len(), wanted.len(), "{} of {} concurrent changes were lost", wanted.len() - landed.len(), wanted.len());
    assert_eq!(landed, wanted);
}

async fn notes_round_trip(catalog: &dyn Catalog) {
    assert_eq!(catalog.notes("quiet").await.unwrap(), None);
    catalog.set_notes("quiet", "schema: orders(id, note)").await.unwrap();
    assert_eq!(catalog.notes("quiet").await.unwrap().as_deref(), Some("schema: orders(id, note)"));
    catalog.set_notes("quiet", "replaced").await.unwrap();
    assert_eq!(catalog.notes("quiet").await.unwrap().as_deref(), Some("replaced"));
    // Notes and meta share a slug without touching each other.
    set(catalog, "quiet", PageMeta { hidden: true, ..PageMeta::default() }).await;
    assert_eq!(catalog.notes("quiet").await.unwrap().as_deref(), Some("replaced"));
    catalog.set_notes("quiet", "again").await.unwrap();
    assert!(catalog.meta("quiet").await.unwrap().hidden);
    for value in hostile().into_iter().filter(|v| !v.contains('\0')) {
        catalog.set_notes("loud", &value).await.unwrap();
        assert_eq!(catalog.notes("loud").await.unwrap(), Some(value));
    }
}

async fn generations_count_per_app(catalog: &dyn Catalog) {
    assert_eq!(catalog.generation("gen-a").await.unwrap(), 0);
    assert_eq!(catalog.bump_generation("gen-a").await.unwrap(), 1);
    assert_eq!(catalog.bump_generation("gen-a").await.unwrap(), 2);
    assert_eq!(catalog.generation("gen-a").await.unwrap(), 2);
    assert_eq!(catalog.generation("gen-b").await.unwrap(), 0);
    // A meta change is not a publish.
    set(catalog, "gen-a", PageMeta { spa: true, ..PageMeta::default() }).await;
    assert_eq!(catalog.generation("gen-a").await.unwrap(), 2);
}

async fn published_files_are_listed(catalog: &dyn Catalog, data_dir: &Path) {
    std::fs::write(data_dir.join("note.html"), "<title>Note</title>").unwrap();
    std::fs::create_dir_all(data_dir.join("board/inner")).unwrap();
    std::fs::write(data_dir.join("board/index.html"), "x").unwrap();
    std::fs::write(data_dir.join("board/inner/page.html"), "x").unwrap();
    std::fs::create_dir_all(data_dir.join(".trash/1-old")).unwrap();
    std::fs::write(data_dir.join(".trash/1-old/slug.html"), "x").unwrap();
    let mut slugs = catalog.slugs().await.unwrap();
    slugs.sort();
    assert_eq!(slugs, vec!["board", "note"]);
    assert_eq!(catalog.apps().await.unwrap(), vec!["board", "note"]);
}

async fn run(catalog: Arc<dyn Catalog>, data_dir: &Path) {
    every_field_round_trips(&*catalog).await;
    slugs_are_apart(&*catalog).await;
    old_words_come_back_current(&*catalog).await;
    a_failed_edit_changes_nothing(&*catalog).await;
    concurrent_changes_all_land(catalog.clone(), "busy").await;
    notes_round_trip(&*catalog).await;
    generations_count_per_app(&*catalog).await;
    published_files_are_listed(&*catalog, data_dir).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_files_catalog_conforms() {
    let dir = tempfile::tempdir().unwrap();
    run(Arc::new(Files::new(dir.path().to_path_buf())), dir.path()).await;
}

/// The sidecar holds exactly the meta's serde text, so a file written
/// before this store is read the same and one written by it is unchanged
/// in shape. The write is a rename, so nothing else is left beside it.
#[tokio::test]
async fn the_files_catalog_writes_the_same_sidecar_as_before() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("page.html"), "x").unwrap();
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/index.html"), "x").unwrap();
    let catalog = Files::new(dir.path().to_path_buf());
    let meta = set(&catalog, "page", everything()).await;
    set(&catalog, "app", everything()).await;
    let text = serde_json::to_string(&meta).unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("page.meta")).unwrap(), text);
    assert_eq!(std::fs::read_to_string(dir.path().join("app/index.meta")).unwrap(), text);
    let mut left: Vec<String> =
        std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec!["app", "page.html", "page.meta"]);

    // A sidecar from before, with the old word and a field this build does
    // not know, reads as it always did.
    std::fs::write(dir.path().join("old.meta"), r#"{"gate":"granted","hidden":true,"someday":1}"#).unwrap();
    let old = catalog.meta("old").await.unwrap();
    assert_eq!(old.gate.as_deref(), Some("restricted"));
    assert!(old.hidden);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_postgres_catalog_conforms() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(Postgres::new(pool.clone(), dir.path().to_path_buf()));
    run(catalog.clone(), dir.path()).await;

    // The row holds exactly the meta's serde text, as the sidecar does.
    let meta = set(&*catalog, "exact", everything()).await;
    let client = pool.get().await.unwrap();
    let text: String = client
        .query_one("select meta::text from platform.pages where slug = 'exact'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(text, serde_json::to_string(&meta).unwrap());

    // A row that is not a meta is an error, never the open default.
    client
        .execute("insert into platform.pages (slug, meta, created_at, updated_at) values ('torn', '[1]', 0, 0)", &[])
        .await
        .unwrap();
    assert!(catalog.meta("torn").await.is_err());
    assert!(catalog.update_meta("torn", Box::new(|_| Ok(()))).await.is_err());
    drop(client);
    drop(catalog);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}

/// A removal takes the app's rows and every page's under it out of
/// `pages`, keeps them in `removed_pages`, and leaves a neighbour whose
/// name only starts the same alone: an app published again at the slug
/// starts with nothing of the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_removal_retires_the_rows_and_keeps_them_on_postgres() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog: Arc<dyn Catalog> = Arc::new(Postgres::new(pool.clone(), dir.path().to_path_buf()));
    set(&*catalog, "shop", PageMeta { created_by: Some("first".into()), hidden: true, ..PageMeta::default() }).await;
    set(&*catalog, "shop/back", PageMeta { gate: Some("restricted".into()), ..PageMeta::default() }).await;
    catalog.set_notes("shop", "old notes").await.unwrap();
    set(&*catalog, "shop_x", PageMeta { gate: Some("public".into()), ..PageMeta::default() }).await;
    set(&*catalog, "shopx", PageMeta { gate: Some("public".into()), ..PageMeta::default() }).await;

    let retiring = catalog.clone();
    let retired = tokio::task::spawn_blocking(move || retiring.retire_blocking("shop", 77)).await.unwrap().unwrap();
    let slugs: Vec<&str> = retired.pages.iter().map(|(slug, _, _)| slug.as_str()).collect();
    assert_eq!(slugs, vec!["shop", "shop/back"]);
    assert!(retired.pages[0].1.as_deref().unwrap().contains("first"));
    assert_eq!(retired.pages[0].2.as_deref(), Some("old notes"));

    assert_eq!(json(&catalog.meta("shop").await.unwrap()), json(&PageMeta::default()));
    assert_eq!(catalog.notes("shop").await.unwrap(), None);
    assert_eq!(catalog.meta("shop_x").await.unwrap().gate.as_deref(), Some("public"));
    assert_eq!(catalog.meta("shopx").await.unwrap().gate.as_deref(), Some("public"));
    let kept: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("select count(*) from platform.removed_pages where removed_at = 77", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 2);
    drop(catalog);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}
