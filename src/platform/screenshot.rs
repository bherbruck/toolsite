//! A picture of a page as a person would see it, for an agent to look at
//! before it says the page works.
//!
//! The render happens here, on the server, because this is the only place
//! that can open a gated page as a given account and sees the real data. A
//! headless browser is run as a subprocess against a one-time preview URL
//! (see `preview.rs`), the PNG it writes is read back, scaled to a size a
//! tool result can carry, and handed over. No browser on the machine means
//! the tool says so and nothing else changes.

use crate::{config::Config, platform::preview};
use image::{codecs::jpeg::JpegEncoder, GenericImageView, ImageFormat};
use std::{io::Cursor, path::PathBuf, time::Duration};
use tokio::process::Command;

/// Names a browser answers to when `TOOLSITE_BROWSER` does not say.
const BROWSER_NAMES: [&str; 5] = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"];
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 1600;
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;
/// A full-page capture is a tall viewport; this is as tall as it gets.
pub const FULL_PAGE_HEIGHT: u32 = 4000;
/// What comes back is at most this wide, whatever was rendered.
pub const OUTPUT_WIDTH: u32 = 1280;
/// Past this a PNG becomes a JPEG, so a tool result stays small.
const PNG_BUDGET: usize = 1024 * 1024;
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the browser is, if anywhere. `TOOLSITE_BROWSER` wins; otherwise
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

    fn height(&self) -> u32 {
        if self.full_page {
            FULL_PAGE_HEIGHT
        } else {
            DEFAULT_HEIGHT
        }
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
    "Screenshots need a browser on the server. Set TOOLSITE_BROWSER to a Chromium \
     binary, or build the image with WITH_BROWSER=1 (the default Dockerfile does)."
        .to_string()
}

/// Renders `/p/<app><path>` as `user_id` (or as nobody) and returns an image.
pub async fn render(
    config: &Config,
    app: &str,
    path: &str,
    user_id: Option<&str>,
    options: Options,
) -> Result<Shot, String> {
    let Some(browser) = config.browser.clone() else {
        return Err(no_browser_message());
    };
    let token = preview::issue(config, app, path, user_id)?;
    let port = local_port(config)?;
    let dir = config.data_dir.join(".tmp").join("shots");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("could not prepare a place for the screenshot: {e}"))?;
    let output = dir.join(format!("{}.png", crate::content::slug::random_token(16)));
    let url = format!("http://127.0.0.1:{port}/preview/{token}");

    let mut command = Command::new(&browser);
    command
        .arg("--headless=new")
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--hide-scrollbars")
        .arg("--no-first-run")
        .arg("--disable-extensions")
        .arg(format!("--window-size={},{}", options.width, options.height()))
        .arg("--virtual-time-budget=5000")
        .arg(format!("--screenshot={}", output.display()))
        .arg(&url)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let run = tokio::time::timeout(RENDER_TIMEOUT, command.output()).await;
    let result = match run {
        Err(_) => Err(format!(
            "the browser did not finish within {} seconds",
            RENDER_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => Err(format!("could not start the browser at {}: {e}", browser.display())),
        Ok(Ok(out)) => {
            if output.is_file() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let line = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
                Err(format!("the browser produced no image ({}): {line}", out.status))
            }
        }
    };
    let bytes = match result {
        Ok(()) => tokio::fs::read(&output).await.map_err(|e| format!("could not read the screenshot: {e}")),
        Err(why) => Err(why),
    };
    let _ = tokio::fs::remove_file(&output).await;
    let bytes = bytes?;
    tokio::task::spawn_blocking(move || fit(&bytes))
        .await
        .map_err(|_| "the image step failed".to_string())?
}

/// The port this server listens on, from the local base URL.
fn local_port(config: &Config) -> Result<u16, String> {
    let base = config.local_base.trim_end_matches('/');
    let after_host = base.rsplit(':').next().unwrap_or_default();
    after_host
        .parse::<u16>()
        .map_err(|_| format!("cannot tell the local port from {base:?}"))
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
        assert_eq!(Options::new(Some(1600), true).unwrap().height(), FULL_PAGE_HEIGHT);
        assert_eq!(Options::new(Some(320), false).unwrap().height(), DEFAULT_HEIGHT);
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
    fn the_local_port_comes_from_the_base_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::local(dir.path().to_path_buf(), "t");
        assert_eq!(local_port(&config).unwrap(), 8080);
        config.local_base = "http://localhost:18800/".to_string();
        assert_eq!(local_port(&config).unwrap(), 18800);
    }

    #[test]
    fn without_a_browser_the_message_names_the_knobs() {
        let message = no_browser_message();
        assert!(message.contains("TOOLSITE_BROWSER"));
        assert!(message.contains("WITH_BROWSER"));
    }
}
