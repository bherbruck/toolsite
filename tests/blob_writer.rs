//! Files a handler writes in pieces (`blobs.writer-*`), driven through the
//! real router and the test guest. The backend details are unit tests in
//! src/runtime/blobs.rs; this is what a guest can and cannot do with a
//! handle.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{build_router, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

fn server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "test-token"));
    (dir, config)
}

fn publish_handler(config: &Config, app: &str) {
    let dir = config.data_dir.join(app);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), HANDLER).unwrap();
}

async fn get(config: &Arc<Config>, uri: &str) -> (StatusCode, String) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Files under an app's hidden temp directory: writes in progress, or left
/// behind by one that was not cleaned up.
fn temps(dir: &TempDir, app: &str) -> usize {
    std::fs::read_dir(dir.path().join(app).join(".blobs/tmp")).map(|d| d.count()).unwrap_or(0)
}

#[tokio::test]
async fn appended_chunks_form_the_file_and_nothing_shows_before_finish() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let (status, body) = get(&config, "/p/app/api/writer-write?key=out/data.txt&parts=alpha,beta,gamma&peek=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "14:text/plain before-finish:stat=false,listed=0");
    let (status, body) = get(&config, "/p/app/api/blob-get?key=out/data.txt").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "alphabetagamma"));
    assert_eq!(temps(&dir, "app"), 0, "temp left behind");
}

#[tokio::test]
async fn an_aborted_writer_stores_nothing_and_leaves_no_temp() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let (status, body) = get(&config, "/p/app/api/writer-abort?key=gone.txt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("append after: Err("), "an aborted handle took more bytes: {body}");
    let (status, _) = get(&config, "/p/app/api/blob-stat?key=gone.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(temps(&dir, "app"), 0, "temp left behind");
}

#[tokio::test]
async fn a_call_that_traps_with_a_writer_open_leaves_no_file_and_no_temp() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let (status, _) = get(&config, "/p/app/api/writer-trap?key=half.txt").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = get(&config, "/p/app/api/blob-stat?key=half.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(temps(&dir, "app"), 0, "the trapped call's temp outlived it");
}

#[tokio::test]
async fn a_writer_left_open_is_abandoned_when_its_call_returns() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let (status, _) = get(&config, "/p/app/api/writer-leave-open?key=left.txt").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&config, "/p/app/api/blob-stat?key=left.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(temps(&dir, "app"), 0, "the call's temp outlived it");
}

#[tokio::test]
async fn a_handle_means_nothing_to_another_call_or_another_app() {
    let (_dir, config) = server();
    publish_handler(&config, "alpha");
    publish_handler(&config, "beta");
    let (status, handle) = get(&config, "/p/alpha/api/writer-leave-open?key=mine.txt").await;
    assert_eq!(status, StatusCode::OK, "{handle}");
    for app in ["alpha", "beta"] {
        let (status, body) = get(&config, &format!("/p/{app}/api/writer-use?handle={handle}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{app} used alpha's handle from another call: {body}");
        let (status, _) = get(&config, &format!("/p/{app}/api/blob-stat?key=mine.txt")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a file appeared in {app}");
    }
}

#[tokio::test]
async fn the_write_ceiling_stops_a_writer_and_nothing_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::local(dir.path().to_path_buf(), "test-token");
    config.blobs.max_write_bytes = 200 * 1024;
    let config = Arc::new(config);
    publish_handler(&config, "app");
    let (status, body) = get(&config, "/p/app/api/writer-flood?key=flood.bin").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.starts_with("Error::TooLarge(262144) after 196608"), "{body}");
    assert!(body.contains("finish: Err("), "a writer past its ceiling finished: {body}");
    let (status, _) = get(&config, "/p/app/api/blob-stat?key=flood.bin").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(temps(&dir, "app"), 0, "temp left behind");
}

#[tokio::test]
async fn a_call_may_hold_only_a_few_writers_at_once() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let (status, body) = get(&config, "/p/app/api/writer-many").await;
    assert_eq!(status, StatusCode::OK);
    let (opened, error) = body.split_once(':').unwrap();
    assert_eq!(opened, toolsite::runtime::blobs::MAX_OPEN_WRITERS.to_string(), "{body}");
    assert!(error.starts_with("Error::Failed("), "{body}");
    assert_eq!(temps(&dir, "app"), 0, "the open writers outlived the call");
}

#[tokio::test]
async fn a_writer_key_cannot_escape_the_apps_files() {
    let (dir, config) = server();
    std::fs::create_dir_all(dir.path().join("victim")).unwrap();
    publish_handler(&config, "app");
    for key in ["../victim/pwned", "..%2Fvictim%2Fpwned", "%2Fetc%2Fpasswd", ".hidden", "a%2F.b", "a%2F%2Fb", ""] {
        let (status, body) = get(&config, &format!("/p/app/api/writer-write?key={key}&parts=x")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key:?} was accepted: {body}");
    }
    assert!(!dir.path().join("victim/pwned").exists());
    assert_eq!(temps(&dir, "app"), 0);
}

/// Where writes in progress live: under `.blobs/tmp/`, which no key, no
/// listing and no route can name. A guest that knows the layout still
/// cannot serve, read or list a half-written file, its own or another's.
#[tokio::test]
async fn a_write_in_progress_is_out_of_reach_of_every_route_and_key() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let tmp = dir.path().join("app/.blobs/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("inflight.part"), "HALF WRITTEN").unwrap();
    std::fs::create_dir_all(dir.path().join("app/.blobs/data")).unwrap();
    std::fs::write(dir.path().join("app/.blobs/data/kept.txt"), "kept").unwrap();

    for uri in [
        "/p/app/.blobs/tmp/inflight.part",
        "/p/app/.blobs/data/kept.txt",
        "/p/app/%2eblobs/tmp/inflight.part",
        "/p/app/%2Eblobs%2Ftmp%2Finflight.part",
        "/p/app/x/../.blobs/tmp/inflight.part",
    ] {
        let (status, body) = get(&config, uri).await;
        assert!(!body.contains("HALF WRITTEN") && body != "kept", "{uri} served it: {status}");
    }
    for key in ["../tmp/inflight.part", "tmp/../../tmp/inflight.part", ".blobs/tmp/inflight.part", "../../.blobs/tmp/inflight.part"] {
        for route in ["blob-serve", "blob-get", "blob-stat"] {
            let (status, body) = get(&config, &format!("/p/app/api/{route}?key={key}")).await;
            assert!(!body.contains("HALF WRITTEN"), "{route} {key}: {status} {body}");
        }
        let (status, body) = get(&config, &format!("/p/app/api/writer-write?key={key}&parts=over")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key} was written: {body}");
    }
    let (_, listed) = get(&config, "/p/app/api/blob-list?prefix=").await;
    assert!(!listed.contains("inflight"), "{listed}");
    assert_eq!(std::fs::read_to_string(tmp.join("inflight.part")).unwrap(), "HALF WRITTEN");
}

/// A process that died mid-write leaves its temp file. The next writer of
/// that app sweeps one a day old, and leaves a fresh one, which may be a
/// write still going on.
#[tokio::test]
async fn temp_files_a_crash_left_are_swept_and_a_live_one_is_not() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    let tmp = dir.path().join("app/.blobs/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("crashed.part"), "x".repeat(1000)).unwrap();
    std::fs::write(tmp.join("live.part"), "y").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
    std::fs::File::options().write(true).open(tmp.join("crashed.part")).unwrap().set_modified(old).unwrap();

    let (status, body) = get(&config, "/p/app/api/writer-write?key=new.txt&parts=a").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!tmp.join("crashed.part").exists(), "a day-old temp was kept");
    assert!(tmp.join("live.part").exists(), "a fresh temp was swept");
}

/// Many small appends count toward the ceiling as one large one would.
#[tokio::test]
async fn the_write_ceiling_counts_every_small_append() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        blobs: toolsite::runtime::blobs::Blobs { max_write_bytes: 10, ..toolsite::runtime::blobs::Blobs::local(0) },
        ..Config::local(dir.path().to_path_buf(), "test-token")
    });
    publish_handler(&config, "app");
    let parts = ["x"; 11].join(",");
    let (status, body) = get(&config, &format!("/p/app/api/writer-write?key=small.txt&parts={parts}")).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let (status, _) = get(&config, "/p/app/api/blob-get?key=small.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(temps(&dir, "app"), 0);
}
