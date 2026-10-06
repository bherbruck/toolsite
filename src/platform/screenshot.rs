//! A picture of a page as a person would see it, for an agent to look at
//! before it says the page works.
//!
//! The picture is taken on the server's behalf, because only the server can
//! open a gated page as a given account: it mints a one-time preview URL
//! (see `preview.rs`) and hands it to a `Renderer`, which loads it in a real
//! browser and returns PNG bytes. The renderer is a seam. `LocalBrowser`
//! starts a browser in this container; `RemoteBrowser` drives one in another
//! container over the Chrome DevTools Protocol, which Browserless, a
//! chrome-headless-shell container and Playwright's Chromium all speak.
//! Another engine is one more implementation. With none configured, the tool
//! says so and nothing else changes.

use crate::{config::Config, platform::preview};
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    cdp::browser_protocol::{emulation::SetDeviceMetricsOverrideParams, page::CaptureScreenshotFormat},
    handler::Handler,
    page::ScreenshotParams,
};
use futures_util::StreamExt;
use image::{codecs::jpeg::JpegEncoder, GenericImageView, ImageFormat};
use std::{io::Cursor, path::PathBuf, sync::Arc, time::Duration};

/// Names a browser answers to when `TOOLSITE_BROWSER` does not say.
const BROWSER_NAMES: [&str; 6] = [
    "chrome-headless-shell",
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 1600;
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;
/// A full-page capture is as tall as the page, up to this.
pub const FULL_PAGE_HEIGHT: u32 = 4000;
/// What comes back is at most this wide, whatever was rendered.
pub const OUTPUT_WIDTH: u32 = 1280;
/// Past this a PNG becomes a JPEG, so a tool result stays small.
const PNG_BUDGET: usize = 1024 * 1024;
/// The whole render, browser start included.
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the page may keep fetching before the picture is taken anyway.
const SETTLE_LIMIT_MS: u32 = 8000;

/// Something that can load a URL and return a PNG of it.
#[async_trait::async_trait]
pub trait Renderer: Send + Sync {
    /// One line for the boot log: which browser, where.
    fn describe(&self) -> String;
    /// Loads `url`, waits for it to finish loading its data, and returns PNG
    /// bytes at `options.width`.
    async fn render(&self, url: &str, options: &Options) -> Result<Vec<u8>, String>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub width: u32,
    pub full_page: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            full_page: false,
        }
    }
}

impl Options {
    pub fn new(width: Option<u32>, full_page: bool) -> Result<Self, String> {
        let width = width.unwrap_or(DEFAULT_WIDTH);
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) {
            return Err(format!("width must be between {MIN_WIDTH} and {MAX_WIDTH} pixels"));
        }
        Ok(Self { width, full_page })
    }
}

pub struct Shot {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// What the tool says when there is nothing to render with.
pub fn no_browser_message() -> String {
    "Screenshots are not set up on this server. Set TOOLSITE_BROWSER_URL to a browser \
     sidecar (ws:// or http:// of its DevTools endpoint), or TOOLSITE_BROWSER to a \
     Chromium binary in this container, or build the image with WITH_BROWSER=1."
        .to_string()
}

// --- choosing a renderer -------------------------------------------------

/// Where a local browser is, if anywhere. `TOOLSITE_BROWSER` wins; otherwise
/// the usual names are looked up on `PATH`.
pub fn find_browser() -> Option<PathBuf> {
    if let Ok(named) = std::env::var("TOOLSITE_BROWSER") {
        let named = named.trim();
        if !named.is_empty() {
            let path = PathBuf::from(named);
            return path.is_file().then_some(path);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in BROWSER_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The renderer this deployment asked for: a sidecar named by
/// `TOOLSITE_BROWSER_URL`, else a local browser, else none.
pub fn from_env() -> Result<Option<Arc<dyn Renderer>>, String> {
    if let Ok(url) = std::env::var("TOOLSITE_BROWSER_URL") {
        let url = url.trim();
        if !url.is_empty() {
            return Ok(Some(Arc::new(RemoteBrowser::new(url)?)));
        }
    }
    Ok(find_browser().map(|path| Arc::new(LocalBrowser::new(path)) as Arc<dyn Renderer>))
}

/// The base the one-time preview URL is built on: `TOOLSITE_PREVIEW_BASE`, or
/// `default` (this server's own port). A sidecar needs an address it can
/// reach, such as a private network name.
pub fn preview_base_from_env(default: &str) -> Result<String, String> {
    match std::env::var("TOOLSITE_PREVIEW_BASE") {
        Ok(base) if !base.trim().is_empty() => check_http_base(base.trim()),
        _ => Ok(default.trim_end_matches('/').to_string()),
    }
}

fn check_http_base(base: &str) -> Result<String, String> {
    let uri: axum::http::Uri = base
        .parse()
        .map_err(|_| format!("TOOLSITE_PREVIEW_BASE {base:?} is not a URL"))?;
    match (uri.scheme_str(), uri.host()) {
        (Some("http" | "https"), Some(_)) => Ok(base.trim_end_matches('/').to_string()),
        _ => Err(format!("TOOLSITE_PREVIEW_BASE must be an http:// or https:// URL, not {base:?}")),
    }
}

/// A browser started in this container, one per render. Starting costs
/// about a second; in return nothing stays resident while nobody asks for a
/// picture, and a page that hangs or crashes the browser takes only its own
/// render with it.
pub struct LocalBrowser {
    path: PathBuf,
}

impl LocalBrowser {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl Renderer for LocalBrowser {
    fn describe(&self) -> String {
        format!("local browser at {}", self.path.display())
    }

    async fn render(&self, url: &str, options: &Options) -> Result<Vec<u8>, String> {
        // A profile of its own, so two renders at once do not fight over
        // one, and removed afterwards.
        let profile = std::env::temp_dir().join(format!(
            "toolsite-render-{}",
            crate::content::slug::random_token(12)
        ));
        let config = BrowserConfig::builder()
            .chrome_executable(&self.path)
            .user_data_dir(&profile)
            .no_sandbox()
            .viewport(None)
            .window_size(options.width, DEFAULT_HEIGHT)
            .args([
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--hide-scrollbars",
                "--no-first-run",
                "--disable-extensions",
                "--mute-audio",
            ])
            .build()
            .map_err(|e| format!("could not configure the browser: {e}"))?;
        let path = self.path.display().to_string();
        let result = within_limit(async move {
            let (mut browser, handler) = Browser::launch(config)
                .await
                .map_err(|e| format!("could not start the browser at {path}: {e}"))?;
            let pump = spawn_handler(handler);
            let result = capture(&browser, url, options).await;
            let _ = browser.close().await;
            let _ = browser.wait().await;
            pump.abort();
            result
        })
        .await;
        let _ = tokio::fs::remove_dir_all(&profile).await;
        result
    }
}

/// A browser in another container, reached over the Chrome DevTools
/// Protocol. `http://` is resolved through `/json/version`.
pub struct RemoteBrowser {
    url: String,
}

impl RemoteBrowser {
    pub fn new(url: &str) -> Result<Self, String> {
        let scheme = url.split("://").next().unwrap_or_default();
        if !matches!(scheme, "ws" | "wss" | "http" | "https") || !url.contains("://") {
            return Err(format!(
                "TOOLSITE_BROWSER_URL must be ws://, wss://, http:// or https://, not {url:?}"
            ));
        }
        Ok(Self { url: url.to_string() })
    }
}

#[async_trait::async_trait]
impl Renderer for RemoteBrowser {
    fn describe(&self) -> String {
        format!("browser sidecar at {}", redact(&self.url))
    }

    async fn render(&self, url: &str, options: &Options) -> Result<Vec<u8>, String> {
        let endpoint = self.url.clone();
        within_limit(async move {
            let (browser, handler) = Browser::connect(endpoint.clone())
                .await
                .map_err(|e| format!("could not reach the browser sidecar at {}: {e}", redact(&endpoint)))?;
            let pump = spawn_handler(handler);
            let result = capture(&browser, url, options).await;
            drop(browser);
            pump.abort();
            result
        })
        .await
    }
}

/// A sidecar URL may carry a token in its query (Browserless does); the log
/// shows the address without it.
fn redact(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

fn spawn_handler(mut handler: Handler) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    })
}

async fn within_limit(
    work: impl std::future::Future<Output = Result<Vec<u8>, String>>,
) -> Result<Vec<u8>, String> {
    match tokio::time::timeout(RENDER_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "the browser did not finish within {} seconds",
            RENDER_TIMEOUT.as_secs()
        )),
    }
}

/// Counts requests the page has in flight. Installed before the page's own
/// scripts run, so a fetch made on load is counted from its start.
const TRACK_REQUESTS: &str = r#"(() => {
  if (window.__toolsitePending !== undefined) return;
  window.__toolsitePending = 0;
  const fetchNative = window.fetch;
  if (fetchNative) {
    window.fetch = function (...args) {
      window.__toolsitePending++;
      return fetchNative.apply(this, args).finally(() => { window.__toolsitePending--; });
    };
  }
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (...args) {
    window.__toolsitePending++;
    this.addEventListener('loadend', () => { window.__toolsitePending--; }, { once: true });
    return send.apply(this, args);
  };
})();"#;

/// Waits for the load event, then for half a second with no request in
/// flight (or the settle limit), then two frames so the last paint lands.
/// Returns the page's height.
fn settle_script() -> String {
    format!(
        r#"async () => {{
  const deadline = Date.now() + {SETTLE_LIMIT_MS};
  if (document.readyState !== 'complete') {{
    await new Promise((resolve) => {{
      addEventListener('load', resolve, {{ once: true }});
      setTimeout(resolve, {SETTLE_LIMIT_MS});
    }});
  }}
  let quiet = 0;
  while (Date.now() < deadline && quiet < 5) {{
    await new Promise((resolve) => setTimeout(resolve, 100));
    quiet = (window.__toolsitePending || 0) > 0 ? 0 : quiet + 1;
  }}
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  const root = document.documentElement;
  return Math.ceil(Math.max(root ? root.scrollHeight : 0, document.body ? document.body.scrollHeight : 0));
}}"#
    )
}

fn viewport(width: u32, height: u32) -> SetDeviceMetricsOverrideParams {
    SetDeviceMetricsOverrideParams::new(width as i64, height as i64, 1.0, false)
}

/// What both renderers do once they have a browser: open a page at the
/// requested size, load the URL, wait for it to settle, take the picture.
async fn capture(browser: &Browser, url: &str, options: &Options) -> Result<Vec<u8>, String> {
    let failed = |what: &str, e: chromiumoxide::error::CdpError| format!("the browser could not {what}: {e}");
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| failed("open a page", e))?;
    let result = async {
        page.execute(viewport(options.width, DEFAULT_HEIGHT))
            .await
            .map_err(|e| failed("set the page size", e))?;
        page.evaluate_on_new_document(TRACK_REQUESTS)
            .await
            .map_err(|e| failed("prepare the page", e))?;
        page.goto(url).await.map_err(|e| failed("load the page", e))?;
        let settled = page
            .evaluate_function(settle_script())
            .await
            .map_err(|e| failed("wait for the page", e))?;
        let height = settled.into_value::<f64>().unwrap_or(DEFAULT_HEIGHT as f64);
        if options.full_page {
            let tall = (height.max(1.0) as u32).min(FULL_PAGE_HEIGHT);
            page.execute(viewport(options.width, tall))
                .await
                .map_err(|e| failed("set the page size", e))?;
            let _ = page
                .evaluate_function("async () => { await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))); return true; }")
                .await;
        }
        page.screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(false)
                .build(),
        )
        .await
        .map_err(|e| failed("take the picture", e))
    }
    .await;
    let _ = page.close().await;
    result
}

// --- the entry point -----------------------------------------------------

/// Renders `/p/<app><path>` as `user_id` (or as nobody) and returns an image.
pub async fn render(
    config: &Config,
    app: &str,
    path: &str,
    user_id: Option<&str>,
    options: Options,
) -> Result<Shot, String> {
    let Some(renderer) = config.renderer.clone() else {
        return Err(no_browser_message());
    };
    let token = preview::issue(config, app, path, user_id)?;
    let url = preview_url(config, &token);
    let png = renderer.render(&url, &options).await?;
    tokio::task::spawn_blocking(move || fit(&png))
        .await
        .map_err(|_| "the image step failed".to_string())?
}

/// The one-time URL a renderer opens, on the base it can reach.
pub fn preview_url(config: &Config, token: &str) -> String {
    format!("{}/preview/{token}", config.preview_base.trim_end_matches('/'))
}

/// Scales a rendered PNG to at most `OUTPUT_WIDTH` wide and encodes it: PNG
/// when that fits the budget, otherwise JPEG.
pub fn fit(png: &[u8]) -> Result<Shot, String> {
    let decoded = image::load_from_memory_with_format(png, ImageFormat::Png)
        .map_err(|e| format!("the browser's image could not be read: {e}"))?;
    let (w, h) = decoded.dimensions();
    let scaled = if w > OUTPUT_WIDTH {
        let new_h = ((h as u64 * OUTPUT_WIDTH as u64) / w as u64).max(1) as u32;
        decoded.resize_exact(OUTPUT_WIDTH, new_h, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let (width, height) = scaled.dimensions();
    let mut out = Cursor::new(Vec::new());
    scaled
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|e| format!("could not encode the screenshot: {e}"))?;
    if out.get_ref().len() <= PNG_BUDGET {
        return Ok(Shot {
            bytes: out.into_inner(),
            media_type: "image/png",
            width,
            height,
        });
    }
    let mut jpeg = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&scaled.to_rgb8())
        .map_err(|e| format!("could not encode the screenshot: {e}"))?;
    Ok(Shot {
        bytes: jpeg.into_inner(),
        media_type: "image/jpeg",
        width,
        height,
    })
}

/// For tests and the CLI: the extension a media type is saved under.
pub fn extension_for(media_type: &str) -> &'static str {
    if media_type == "image/jpeg" {
        "jpg"
    } else {
        "png"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn png_of(width: u32, height: u32, noisy: bool) -> Vec<u8> {
        let img = ImageBuffer::from_fn(width, height, |x, y| {
            if noisy {
                Rgba([(x * 7 % 251) as u8, (y * 13 % 241) as u8, ((x ^ y) % 199) as u8, 255])
            } else {
                Rgba([200, 30, 30, 255])
            }
        });
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn options_keep_the_width_within_bounds() {
        assert_eq!(Options::new(None, false).unwrap().width, DEFAULT_WIDTH);
        assert!(Options::new(Some(319), false).is_err());
        assert!(Options::new(Some(1601), false).is_err());
        assert!(Options::new(Some(1600), true).unwrap().full_page);
        assert_eq!(Options::new(Some(320), false).unwrap().width, 320);
    }

    #[test]
    fn a_wide_render_is_scaled_down_and_a_small_one_left_alone() {
        let big = png_of(1600, 1000, false);
        let shot = fit(&big).unwrap();
        assert_eq!((shot.width, shot.height), (1280, 800));
        assert_eq!(shot.media_type, "image/png");
        let small = png_of(640, 400, false);
        let shot = fit(&small).unwrap();
        assert_eq!((shot.width, shot.height), (640, 400));
    }

    #[test]
    fn a_png_that_will_not_fit_the_budget_becomes_a_jpeg() {
        // Noise does not compress; a 1280x4000 noisy PNG is far over 1 MB.
        let noisy = png_of(1280, 4000, true);
        let shot = fit(&noisy).unwrap();
        assert_eq!(shot.media_type, "image/jpeg");
        assert!(shot.bytes.len() < noisy.len());
        assert_eq!(extension_for(shot.media_type), "jpg");
    }

    #[test]
    fn a_sidecar_url_must_speak_devtools_and_a_preview_base_must_be_http() {
        assert!(RemoteBrowser::new("ws://browser:3000").is_ok());
        assert!(RemoteBrowser::new("http://browser.railway.internal:9222").is_ok());
        assert!(RemoteBrowser::new("wss://chrome.example.com?token=abc").is_ok());
        assert!(RemoteBrowser::new("browser:3000").is_err());
        assert!(RemoteBrowser::new("ftp://browser").is_err());
        assert_eq!(redact("wss://chrome.example.com?token=secret"), "wss://chrome.example.com");
        assert_eq!(check_http_base("http://toolsite:8080/").unwrap(), "http://toolsite:8080");
        assert!(check_http_base("toolsite:8080").is_err());
        assert!(check_http_base("ws://toolsite:8080").is_err());
    }

    #[test]
    fn the_preview_url_is_built_on_the_preview_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::local(dir.path().to_path_buf(), "t");
        assert_eq!(preview_url(&config, "abc"), "http://127.0.0.1:8080/preview/abc");
        config.preview_base = "http://toolsite.railway.internal:8080/".to_string();
        assert_eq!(preview_url(&config, "abc"), "http://toolsite.railway.internal:8080/preview/abc");
    }

    #[test]
    fn without_a_browser_the_message_names_the_knobs() {
        let message = no_browser_message();
        assert!(message.contains("TOOLSITE_BROWSER_URL"));
        assert!(message.contains("TOOLSITE_BROWSER "));
        assert!(message.contains("WITH_BROWSER"));
    }
}
