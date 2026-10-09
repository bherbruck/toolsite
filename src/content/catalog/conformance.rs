//! One suite every catalog must pass, run on files always and on Postgres
//! when `TOOLSITE_TEST_DATABASE_URL` names a server
//! (`scripts/test-postgres.sh` starts one). It pins what the rules above
//! rely on: a meta comes back exactly as it went in, field by field; one
//! slug's meta never answers for another's; an edit that fails changes
//! nothing; and of many changes at once, every one lands. The same for the
//! project tree; a project move's record and its hold, which lets one move
//! run at a time; host labels, which two apps never share however many are
//! assigned at once; and markers.

use super::{files::Files, postgres::Postgres, Catalog, Relocation};
use crate::{
    accounts::store::conformance::{drop_postgres_database, postgres_database},
    content::store::{Folder, PageMeta, PathRule, Policy, PortProtocol, PortSocket, ResidentMeta},
    runtime::limits::Asked,
};
use std::{sync::Arc, time::Duration};

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
    let first = catalog.bump_generation("gen-a").await.unwrap();
    let second = catalog.bump_generation("gen-a").await.unwrap();
    assert!(first > 0 && second > first, "{first} then {second}");
    assert_eq!(catalog.generation("gen-a").await.unwrap(), second);
    assert_eq!(catalog.generation("gen-b").await.unwrap(), 0);
    // Another app's publish moves only its own.
    assert!(catalog.bump_generation("gen-b").await.unwrap() > 0);
    assert_eq!(catalog.generation("gen-a").await.unwrap(), second);
    // A meta change is not a publish.
    set(catalog, "gen-a", PageMeta { spa: true, ..PageMeta::default() }).await;
    assert_eq!(catalog.generation("gen-a").await.unwrap(), second);
}

fn folder(path: &str) -> Folder {
    Folder {
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        created_at: 1_700_000_000,
        locked: false,
        renamed_from: Vec::new(),
        gate: None,
    }
}

async fn add_folder(catalog: &dyn Catalog, made: Folder) {
    catalog
        .update_folders(Box::new(move |folders| {
            folders.push(made);
            folders.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(())
        }))
        .await
        .unwrap();
}

/// Every field of a project comes back as it went in, the tree reads in
/// path order, a rename is a new path and the old one gone, and an edit
/// that fails changes nothing.
async fn the_tree_round_trips(catalog: &dyn Catalog) {
    assert_eq!(catalog.folders().await.unwrap(), Vec::<Folder>::new());
    let full = Folder {
        locked: true,
        renamed_from: vec!["old".into(), hostile()[0].clone(), hostile()[2].clone()],
        gate: Some("restricted".into()),
        name: hostile()[1].replace('\0', ""),
        ..folder("ops")
    };
    add_folder(catalog, full.clone()).await;
    add_folder(catalog, folder("ops/yard")).await;
    add_folder(catalog, folder("Zeta")).await;
    add_folder(catalog, folder("ops_x")).await;
    let tree = catalog.folders().await.unwrap();
    let paths: Vec<&str> = tree.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["Zeta", "ops", "ops/yard", "ops_x"], "not in the order the file keeps");
    assert_eq!(tree[1], full);
    assert_eq!(catalog.folders_blocking().unwrap(), tree);

    let refused = catalog
        .update_folders(Box::new(|folders| {
            folders.clear();
            Err("refused".into())
        }))
        .await;
    assert_eq!(refused.unwrap_err(), "refused");
    assert_eq!(catalog.folders().await.unwrap(), tree);

    // A move: one path goes, another comes, everything else is as it was.
    let moved = catalog
        .update_folders(Box::new(|folders| {
            for f in folders.iter_mut() {
                if f.path == "ops/yard" {
                    f.path = "ops/dock".into();
                    f.renamed_from.push("ops/yard".into());
                }
            }
            folders.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(())
        }))
        .await
        .unwrap();
    assert_eq!(catalog.folders().await.unwrap(), moved);
    assert!(moved.iter().any(|f| f.path == "ops/dock" && f.renamed_from == vec!["ops/yard".to_string()]));
    assert!(!moved.iter().any(|f| f.path == "ops/yard"));
    catalog.update_folders(Box::new(|folders| {
        folders.retain(|f| f.path == "ops");
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(catalog.folders().await.unwrap(), vec![full]);
    catalog.update_folders(Box::new(|folders| {
        folders.clear();
        Ok(())
    }))
    .await
    .unwrap();
}

/// Many projects created at once, each its own: all of them are there
/// after. Read the tree, add one and write it back as three steps would
/// keep only some.
async fn concurrent_tree_changes_all_land(catalog: Arc<dyn Catalog>) {
    const WRITERS: usize = 24;
    let start = Arc::new(tokio::sync::Barrier::new(WRITERS));
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let (catalog, start) = (catalog.clone(), start.clone());
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            add_folder(&*catalog, folder(&format!("p{writer:02}"))).await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let paths: Vec<String> = catalog.folders().await.unwrap().into_iter().map(|f| f.path).collect();
    let wanted: Vec<String> = (0..WRITERS).map(|w| format!("p{w:02}")).collect();
    assert_eq!(paths, wanted, "{} of {WRITERS} concurrent projects were lost", WRITERS - paths.len());
}

async fn a_move_is_recorded_until_it_ends(catalog: Arc<dyn Catalog>) {
    assert_eq!(catalog.relocation().await.unwrap(), None);
    catalog.begin_relocation("ops", "ops2").await.unwrap();
    let wanted = Relocation { from: "ops".into(), to: "ops2".into() };
    assert_eq!(catalog.relocation().await.unwrap(), Some(wanted.clone()));
    let reading = catalog.clone();
    let blocking = tokio::task::spawn_blocking(move || reading.relocation_blocking()).await.unwrap().unwrap();
    assert_eq!(blocking, Some(wanted));
    // One record at a time: a new one replaces it.
    catalog.begin_relocation(&hostile()[0], "b/c").await.unwrap();
    assert_eq!(catalog.relocation().await.unwrap(), Some(Relocation { from: hostile()[0].clone(), to: "b/c".into() }));
    catalog.end_relocation().await.unwrap();
    assert_eq!(catalog.relocation().await.unwrap(), None);
    catalog.end_relocation().await.unwrap();
}

/// While one move holds, the next waits; released, or dropped as a task
/// that fails would drop it, the next goes in.
async fn one_move_at_a_time(catalog: Arc<dyn Catalog>) {
    let first = catalog.hold_relocations().await.unwrap();
    let waiting = catalog.clone();
    let mut second = tokio::spawn(async move { waiting.hold_relocations().await.unwrap() });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut second).await.is_err(),
        "a second move started while the first held"
    );
    first.release().await;
    let second = tokio::time::timeout(Duration::from_secs(10), second).await.expect("the second move never started").unwrap();
    let waiting = catalog.clone();
    let mut third = tokio::spawn(async move { waiting.hold_relocations().await.unwrap() });
    assert!(tokio::time::timeout(Duration::from_millis(300), &mut third).await.is_err());
    drop(second);
    let third = tokio::time::timeout(Duration::from_secs(10), third).await.expect("a dropped hold kept the next move out").unwrap();
    third.release().await;
}

/// The first label of `wanted` that nobody holds yet.
fn first_free(wanted: Vec<String>) -> super::LabelChoice<'static> {
    Box::new(move |issued| wanted.iter().find(|label| !issued.contains_key(*label)).cloned().expect("a free label"))
}

/// Many apps asking for a label at once, each wanting the first free one
/// of the same list: every app gets one, and no two the same. Without the
/// hold, each would read the same list and choose the same label.
async fn concurrent_labels_are_never_shared(catalog: Arc<dyn Catalog>) {
    const APPS: usize = 16;
    let wanted: Vec<String> = (0..APPS).map(|n| format!("shared-{n:02}")).collect();
    let start = Arc::new(std::sync::Barrier::new(APPS));
    let mut tasks = Vec::new();
    for n in 0..APPS {
        let (catalog, start, wanted) = (catalog.clone(), start.clone(), wanted.clone());
        tasks.push(tokio::task::spawn_blocking(move || {
            start.wait();
            let app = format!("app{n:02}");
            let label = catalog.assign_label_blocking(&app, true, first_free(wanted));
            (app, label)
        }));
    }
    let mut given = std::collections::BTreeMap::new();
    for task in tasks {
        let (app, label) = task.await.unwrap();
        let label = label.unwrap_or_else(|why| panic!("{app} got no label: {why}"));
        if let Some(other) = given.insert(label.clone(), app.clone()) {
            panic!("{label} was given to both {other} and {app}");
        }
    }
    assert_eq!(given.len(), APPS);
    for (label, app) in &given {
        assert_eq!(catalog.label_owner_blocking(label).unwrap().as_deref(), Some(app.as_str()));
    }
}

async fn labels_are_issued_once(catalog: Arc<dyn Catalog>) {
    let catalog = catalog.clone();
    tokio::task::spawn_blocking(move || {
        assert_eq!(catalog.label_owner_blocking("shop").unwrap(), None);
        // Not recorded: the app does not exist yet.
        let label = catalog.assign_label_blocking("shop", false, Box::new(|_| "shop".to_string())).unwrap();
        assert_eq!(label, "shop");
        assert_eq!(catalog.label_owner_blocking("shop").unwrap(), None);
        // Recorded, and asking again keeps it.
        catalog.assign_label_blocking("shop", true, Box::new(|_| "shop".to_string())).unwrap();
        assert_eq!(catalog.label_owner_blocking("shop").unwrap().as_deref(), Some("shop"));
        catalog
            .assign_label_blocking("shop", true, Box::new(|issued| {
                assert_eq!(issued.get("shop").map(String::as_str), Some("shop"));
                "shop".to_string()
            }))
            .unwrap();
        // Never to another app, even if a choice asks for it.
        let refused = catalog.assign_label_blocking("Shop", true, Box::new(|_| "shop".to_string()));
        assert!(refused.is_err(), "a label issued to one app was issued to another");
        assert_eq!(catalog.label_owner_blocking("shop").unwrap().as_deref(), Some("shop"));
        // Labels and apps are compared exactly.
        assert_eq!(catalog.label_owner_blocking("SHOP").unwrap(), None);
    })
    .await
    .unwrap();
}

async fn markers_round_trip(catalog: &dyn Catalog) {
    assert_eq!(catalog.flag("grants-adopted").await.unwrap(), None);
    catalog.set_flag("grants-adopted", "3\n").await.unwrap();
    assert_eq!(catalog.flag("grants-adopted").await.unwrap().as_deref(), Some("3\n"));
    catalog.set_flag("grants-adopted", "0\n").await.unwrap();
    assert_eq!(catalog.flag("grants-adopted").await.unwrap().as_deref(), Some("0\n"));
    assert_eq!(catalog.flag("other-marker").await.unwrap(), None);
}

async fn run(catalog: Arc<dyn Catalog>) {
    every_field_round_trips(&*catalog).await;
    slugs_are_apart(&*catalog).await;
    old_words_come_back_current(&*catalog).await;
    a_failed_edit_changes_nothing(&*catalog).await;
    concurrent_changes_all_land(catalog.clone(), "busy").await;
    notes_round_trip(&*catalog).await;
    generations_count_per_app(&*catalog).await;
    the_tree_round_trips(&*catalog).await;
    concurrent_tree_changes_all_land(catalog.clone()).await;
    a_move_is_recorded_until_it_ends(catalog.clone()).await;
    one_move_at_a_time(catalog.clone()).await;
    concurrent_labels_are_never_shared(catalog.clone()).await;
    labels_are_issued_once(catalog.clone()).await;
    markers_round_trip(&*catalog).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_files_catalog_conforms() {
    let dir = tempfile::tempdir().unwrap();
    run(Arc::new(Files::new(dir.path().to_path_buf()))).await;
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

/// The tree, a move's record, the issued labels and a marker are the same
/// files in the same shape as before this store, and every write is a
/// rename: nothing but the files themselves is left in `.site/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_files_catalog_writes_the_same_site_files_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join(".site");
    let catalog = Arc::new(Files::new(dir.path().to_path_buf()));
    let locked = Folder { locked: true, gate: Some("restricted".into()), renamed_from: vec!["old".into()], ..folder("ops") };
    add_folder(&*catalog, locked.clone()).await;
    add_folder(&*catalog, folder("ops/yard")).await;
    let tree = vec![locked, folder("ops/yard")];
    assert_eq!(std::fs::read_to_string(site.join("projects.json")).unwrap(), serde_json::to_string_pretty(&tree).unwrap());
    // A tree written before, with a field this build does not know, reads.
    std::fs::write(site.join("projects.json"), r#"[{"path":"a","name":"a","created_at":1,"someday":true}]"#).unwrap();
    assert_eq!(catalog.folders().await.unwrap(), vec![Folder { created_at: 1, ..folder("a") }]);

    catalog.begin_relocation("a", "b").await.unwrap();
    assert_eq!(std::fs::read_to_string(site.join("relocating.json")).unwrap(), r#"{"from":"a","to":"b"}"#);
    catalog.end_relocation().await.unwrap();
    assert!(!site.join("relocating.json").exists());
    std::fs::write(site.join("relocating.json"), "{torn").unwrap();
    assert!(catalog.relocation().await.is_err(), "a torn record read as no move at all");
    std::fs::remove_file(site.join("relocating.json")).unwrap();

    let labels = catalog.clone();
    tokio::task::spawn_blocking(move || {
        labels.assign_label_blocking("Shop", true, Box::new(|_| "shop-1a2b3c4d".to_string())).unwrap();
    })
    .await
    .unwrap();
    let mut issued = std::collections::BTreeMap::new();
    issued.insert("shop-1a2b3c4d".to_string(), "Shop".to_string());
    assert_eq!(std::fs::read_to_string(site.join("labels.json")).unwrap(), serde_json::to_string_pretty(&issued).unwrap());

    catalog.set_flag("grants-adopted", "2\n").await.unwrap();
    assert_eq!(std::fs::read_to_string(site.join("grants-adopted")).unwrap(), "2\n");
    assert!(catalog.set_flag("../escape", "x").await.is_err());

    let mut left: Vec<String> =
        std::fs::read_dir(&site).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec!["grants-adopted", "labels.json", "projects.json"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_postgres_catalog_conforms() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(Postgres::new(pool.clone(), dir.path().to_path_buf()));
    run(catalog.clone()).await;

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

    // Another runner's move: the lock taken on a session of its own. A move
    // here waits for it, whatever this process's own queue says.
    let other = pool.get().await.unwrap();
    other.execute("select pg_advisory_lock($1::int4, 0)", &[&crate::state::pg::LOCK_RELOCATION]).await.unwrap();
    let waiting = catalog.clone();
    let mut here = tokio::spawn(async move { waiting.hold_relocations().await.unwrap() });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut here).await.is_err(),
        "a move started while another runner's held"
    );
    other.execute("select pg_advisory_unlock($1::int4, 0)", &[&crate::state::pg::LOCK_RELOCATION]).await.unwrap();
    let here = tokio::time::timeout(Duration::from_secs(10), here).await.expect("the move never started").unwrap();
    // And the other way round: while this hold is on, the other runner's
    // try fails.
    let taken: bool = other
        .query_one("select pg_try_advisory_lock($1::int4, 0)", &[&crate::state::pg::LOCK_RELOCATION])
        .await
        .unwrap()
        .get(0);
    assert!(!taken, "another runner took the move lock while a move here held it");
    here.release().await;
    let taken: bool = other
        .query_one("select pg_try_advisory_lock($1::int4, 0)", &[&crate::state::pg::LOCK_RELOCATION])
        .await
        .unwrap()
        .get(0);
    assert!(taken, "a released hold kept the lock");
    drop(other);
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

    let published = catalog.bump_generation("shop").await.unwrap();

    let retiring = catalog.clone();
    let retired = tokio::task::spawn_blocking(move || retiring.retire_blocking("shop", 77)).await.unwrap().unwrap();
    let slugs: Vec<&str> = retired.pages.iter().map(|(slug, _, _)| slug.as_str()).collect();
    assert_eq!(slugs, vec!["shop", "shop/back"]);
    assert!(retired.pages[0].1.as_deref().unwrap().contains("first"));
    assert_eq!(retired.pages[0].2.as_deref(), Some("old notes"));

    assert_eq!(json(&catalog.meta("shop").await.unwrap()), json(&PageMeta::default()));
    // The next app at the name counts on from past the old one's
    // generation, so no runner's cache of the old app is ever reached.
    assert_eq!(catalog.generation("shop").await.unwrap(), 0);
    assert!(catalog.bump_generation("shop").await.unwrap() > published, "a removed app's generation was handed out again");
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
