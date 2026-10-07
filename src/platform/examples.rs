//! Example apps, served to agents: `GET /examples` lists them and
//! `GET /examples/<name>.tar.gz` hands one over, ready to deploy.
//!
//! The tarballs are packed from `examples/` by build.rs, so what is served
//! is the source in this checkout, and the same source the example tests
//! drive. With `?slug=`, the app's own name is replaced wherever it decides
//! where the app lives: the slug in toolsite.toml, the base path Vite builds
//! for, and the package names. An app built for `/p/kitchen-sink/` and
//! published elsewhere renders blank, so the rename is the server's job,
//! not the reader's.

use crate::config::Config;
use crate::content::slug::valid_slug;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::io::{Read, Write};
use std::sync::Arc;

pub(crate) struct Example {
    pub name: &'static str,
    pub description: &'static str,
    /// A gzipped tar of the example's files, paths relative to its root.
    pub tarball: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/examples.rs"));

/// The names, for a tool description or a test.
pub fn names() -> Vec<&'static str> {
    EXAMPLES.iter().map(|e| e.name).collect()
}

/// A plain list, for `curl`: what each example is and how to start one.
pub async fn list(State(config): State<Arc<Config>>) -> Response {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    let width = EXAMPLES.iter().map(|e| e.name.len()).max().unwrap_or(0);
    let mut text = String::from(
        "# Example apps\n\n\
         Working apps to start from or to copy a part of. Each one deploys as it is.\n\n\
         Start one with the CLI, which names it and sets its base path:\n\n    \
         toolsite init <name> --example <example>\n\n\
         Or download and unpack one. ?slug= renames it the same way:\n\n",
    );
    text.push_str(&format!("    curl -fsS '{base}/examples/<example>.tar.gz?slug=<name>' | tar -xz\n\n"));
    for example in EXAMPLES {
        text.push_str(&format!("{:width$}  {}\n", example.name, example.description));
    }
    text.push_str("\nEach has a README.md that says what it shows and where to look.\n");
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        text,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct DownloadQuery {
    slug: Option<String>,
}

pub async fn download(Path(file): Path<String>, Query(query): Query<DownloadQuery>) -> Response {
    let Some(name) = file.strip_suffix(".tar.gz") else {
        return not_found(&file);
    };
    let Some(example) = EXAMPLES.iter().find(|e| e.name == name) else {
        return not_found(name);
    };
    let slug = query.slug.unwrap_or_else(|| example.name.to_string());
    if !valid_slug(&slug) {
        tracing::warn!(slug = %slug, "example: refused a slug that is not a valid app path");
        return (StatusCode::BAD_REQUEST, "slug must be path segments of letters, digits, '-' or '_'\n").into_response();
    }
    match renamed(example, &slug) {
        Ok(gz) => {
            let top = slug.rsplit('/').next().unwrap_or(&slug).to_string();
            (
                [
                    (header::CONTENT_TYPE, "application/gzip".to_string()),
                    (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{top}.tar.gz\"")),
                ],
                gz,
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(example = example.name, error = %e, "example: could not repack");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not pack the example\n").into_response()
        }
    }
}

fn not_found(name: &str) -> Response {
    let known = names().join(", ");
    (StatusCode::NOT_FOUND, format!("no example '{name}'. There are: {known}\n")).into_response()
}

/// The example under a top directory named for the app, with its name
/// replaced by `slug` in the places that decide where it is served.
pub(crate) fn renamed(example: &Example, slug: &str) -> std::io::Result<Vec<u8>> {
    let leaf = slug.rsplit('/').next().unwrap_or(slug);
    let from = example.name;
    let replacements = [
        (format!("/p/{from}/"), format!("/p/{slug}/")),
        (format!("slug = \"{from}\""), format!("slug = \"{slug}\"")),
        (format!("\"name\": \"{from}\""), format!("\"name\": \"{leaf}\"")),
        (format!("name = \"{from}-handler\""), format!("name = \"{leaf}-handler\"")),
    ];

    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(example.tarball).read_to_end(&mut raw)?;
    let mut archive = tar::Archive::new(raw.as_slice());
    let mut builder = tar::Builder::new(Vec::new());
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let mut body = Vec::new();
        entry.read_to_end(&mut body)?;
        if let Ok(text) = String::from_utf8(body.clone()) {
            let mut text = text;
            for (old, new) in &replacements {
                text = text.replace(old.as_str(), new);
            }
            body = text.into_bytes();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, format!("{leaf}/{path}"), body.as_slice())?;
    }
    let tar = builder.into_inner()?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar)?;
    gz.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(gz: &[u8]) -> Vec<(String, String)> {
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gz).read_to_end(&mut raw).unwrap();
        let mut archive = tar::Archive::new(raw.as_slice());
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let path = e.path().unwrap().to_string_lossy().to_string();
                let mut body = String::new();
                let _ = e.read_to_string(&mut body);
                (path, body)
            })
            .collect()
    }

    fn example(name: &str) -> &'static Example {
        EXAMPLES.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no example {name}"))
    }

    #[test]
    fn every_example_is_embedded_with_a_description() {
        for name in [
            "kitchen-sink",
            "orders",
            "static-report",
            "blob-gallery",
            "inventory-policies",
            "live-board",
            "mqtt-broker",
            "tcp-chat",
            "syslog",
        ] {
            let e = example(name);
            assert!(!e.description.is_empty() && e.description.len() < 300, "{name}: {:?}", e.description);
            assert!(!e.description.contains('\u{2014}'), "{name}: no em-dashes");
        }
    }

    #[test]
    fn dependencies_and_build_output_never_ship() {
        for e in EXAMPLES {
            for (path, _) in files(e.tarball) {
                let parts: Vec<&str> = path.split('/').collect();
                assert!(
                    !parts.iter().any(|p| ["node_modules", "target", "dist"].contains(p)),
                    "{}: {path}",
                    e.name
                );
            }
        }
    }

    #[test]
    fn a_handler_carries_the_contract_as_a_real_file() {
        let wit = include_str!("../../wit/toolsite.wit");
        let shipped = files(example("kitchen-sink").tarball);
        let (_, body) = shipped.iter().find(|(p, _)| p == "handler/wit/toolsite.wit").expect("no wit in the tarball");
        assert_eq!(body, wit);
    }

    #[test]
    fn renaming_moves_the_base_path_and_the_slug_together() {
        let gz = renamed(example("kitchen-sink"), "ops/sink").unwrap();
        let files = files(&gz);
        assert!(files.iter().all(|(p, _)| p.starts_with("sink/")), "one top directory, named for the app");
        let find = |p: &str| files.iter().find(|(path, _)| path == p).map(|(_, b)| b.clone()).unwrap();
        assert!(find("sink/vite.config.ts").contains("base: '/p/ops/sink/'"));
        assert!(find("sink/toolsite.toml").contains("slug = \"ops/sink\""));
        assert!(find("sink/package.json").contains("\"name\": \"sink\""));
        assert!(find("sink/handler/Cargo.toml").contains("name = \"sink-handler\""));
        assert!(
            !files.iter().any(|(_, body)| body.contains("/p/kitchen-sink/")),
            "the old base path survived somewhere"
        );
    }
}
