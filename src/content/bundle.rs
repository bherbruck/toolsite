use crate::content::slug::valid_asset_path;

/// Zip-bomb guards: a bundle is a built front-end, not an archive dump.
pub(crate) const MAX_BUNDLE_UNPACKED: u64 = 128 * 1024 * 1024;

pub(crate) const MAX_BUNDLE_ENTRIES: usize = 2_000;

/// What to do with one archive entry.
pub(crate) enum EntryVerdict {
    Take(String),
    /// Directories and archive metadata: every tarball has them and nothing is
    /// lost by not writing them.
    Ignore,
    /// Dotfiles and symlinks: harmless to leave out, but reported so an upload
    /// never silently ships less than it claims.
    Skip(&'static str),
    /// Traversal and absolute paths are attacks, not build-output quirks.
    Reject(String),
}

pub(crate) fn classify_entry(entry: &tar::Entry<'_, impl std::io::Read>) -> EntryVerdict {
    let entry_type = entry.header().entry_type();
    if entry_type.is_dir() || entry_type.is_pax_global_extensions() || entry_type.is_gnu_longname()
    {
        return EntryVerdict::Ignore;
    }
    // Links can point anywhere on the host filesystem.
    if !entry_type.is_file() {
        return EntryVerdict::Skip("symlinks and special files");
    }
    let raw = match entry.path() {
        Ok(path) => path.to_string_lossy().replace('\\', "/"),
        Err(e) => return EntryVerdict::Reject(format!("unreadable path in bundle: {e}")),
    };
    let rel = raw.trim_start_matches("./").to_string();
    if rel.is_empty() {
        return EntryVerdict::Ignore;
    }
    if rel.starts_with('/') || rel.split('/').any(|seg| seg == ".." || seg == ".") {
        return EntryVerdict::Reject(format!("unsafe path in bundle: {rel}"));
    }
    if rel.split('/').any(|seg| seg.starts_with('.')) {
        return EntryVerdict::Skip("dotfiles");
    }
    if !valid_asset_path(&rel) {
        return EntryVerdict::Reject(format!(
            "unsupported filename in bundle: {rel} (use letters, numbers, '.', '-', '_')"
        ));
    }
    EntryVerdict::Take(rel)
}

/// Entry paths as they should land on disk, or an error naming the offender.
/// Rejects anything that could escape the destination directory.
pub(crate) fn bundle_entry_paths(body: &[u8]) -> Result<Vec<String>, String> {
    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut paths = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        match classify_entry(&entry) {
            EntryVerdict::Take(rel) => paths.push(rel),
            EntryVerdict::Ignore | EntryVerdict::Skip(_) => continue,
            EntryVerdict::Reject(message) => return Err(message),
        }
    }
    if paths.is_empty() {
        return Err("bundle contains no files".to_string());
    }
    if paths.len() > MAX_BUNDLE_ENTRIES {
        return Err(format!("bundle has more than {MAX_BUNDLE_ENTRIES} files"));
    }
    Ok(paths)
}

/// `tar -czf - dist` wraps everything in `dist/`, while `tar -czf - -C dist .`
/// does not. Strip a single shared top-level directory so both work.
pub(crate) fn bundle_strip_prefix(paths: &[String]) -> Option<String> {
    let first = paths.first()?.split('/').next()?.to_string();
    let all_share = paths
        .iter()
        .all(|p| p.starts_with(&format!("{first}/")));
    let root_has_index = paths.iter().any(|p| p == "index.html");
    (all_share && !root_has_index).then_some(first)
}

#[derive(Debug)]
pub(crate) struct Unpacked {
    pub(crate) files: Vec<String>,
    pub(crate) skipped: Vec<&'static str>,
}

pub(crate) fn unpack_bundle(body: &[u8], dest: &std::path::Path) -> Result<Unpacked, String> {
    let paths = bundle_entry_paths(body)?;
    let strip = bundle_strip_prefix(&paths);

    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut written = Vec::new();
    let mut skipped: Vec<&'static str> = Vec::new();
    let mut total: u64 = 0;

    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let rel = match classify_entry(&entry) {
            EntryVerdict::Take(rel) => rel,
            EntryVerdict::Ignore => continue,
            EntryVerdict::Skip(reason) => {
                if !skipped.contains(&reason) {
                    skipped.push(reason);
                }
                continue;
            }
            EntryVerdict::Reject(message) => return Err(message),
        };
        let rel = match &strip {
            Some(prefix) => rel
                .strip_prefix(&format!("{prefix}/"))
                .unwrap_or(&rel)
                .to_string(),
            None => rel,
        };
        if rel.is_empty() {
            continue;
        }

        total += entry.header().size().unwrap_or(0);
        if total > MAX_BUNDLE_UNPACKED {
            return Err(format!(
                "bundle exceeds {} MB unpacked",
                MAX_BUNDLE_UNPACKED / 1024 / 1024
            ));
        }

        let out = dest.join(&rel);
        // Belt and braces: the path checks above should make this impossible,
        // but never write outside the destination.
        if !out.starts_with(dest) {
            return Err(format!("path escapes the app directory: {rel}"));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
        written.push(rel);
    }

    written.sort();
    Ok(Unpacked {
        files: written,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes tar headers by hand. The `tar` crate's builder refuses to
    /// emit `..` or absolute paths, which is exactly what these tests need to
    /// forge — a real attacker is not constrained by our tar library either.
    fn raw_entry(path: &str, body: &[u8], type_flag: u8, link: &str) -> Vec<u8> {
        let mut header = [0u8; 512];
        let put = |header: &mut [u8; 512], offset: usize, bytes: &[u8]| {
            header[offset..offset + bytes.len()].copy_from_slice(bytes);
        };
        put(&mut header, 0, path.as_bytes());
        put(&mut header, 100, b"0000644\0");
        put(&mut header, 108, b"0000000\0");
        put(&mut header, 116, b"0000000\0");
        put(&mut header, 124, format!("{:011o}\0", body.len()).as_bytes());
        put(&mut header, 136, b"00000000000\0");
        header[156] = type_flag;
        put(&mut header, 157, link.as_bytes());
        put(&mut header, 257, b"ustar\0");
        put(&mut header, 263, b"00");

        // Checksum is computed with the checksum field itself read as spaces.
        put(&mut header, 148, b"        ");
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    /// `entries` are (path, contents); a path ending in '@' is a symlink to
    /// /etc/passwd.
    fn tarball(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (path, body) in entries {
            match path.strip_suffix('@') {
                Some(link) => tar.extend(raw_entry(link, b"", b'2', "/etc/passwd")),
                None => tar.extend(raw_entry(path, body.as_bytes(), b'0', "")),
            }
        }
        tar.extend(std::iter::repeat_n(0u8, 1024)); // end-of-archive marker

        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_normal_build_output_unpacks_whole() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[
            ("index.html", "<h1>hi</h1>"),
            ("assets/main-4f2a.js", "console.log(1)"),
            ("assets/main-4f2a.css", "body{}"),
        ]);
        let unpacked = unpack_bundle(&body, dir.path()).unwrap();
        assert_eq!(
            unpacked.files,
            ["assets/main-4f2a.css", "assets/main-4f2a.js", "index.html"]
        );
        assert!(unpacked.skipped.is_empty(), "{:?}", unpacked.skipped);
        assert!(dir.path().join("assets/main-4f2a.js").exists());
    }

    #[test]
    fn a_single_wrapping_directory_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[("dist/index.html", "<h1>hi</h1>"), ("dist/app.js", "x")]);
        let unpacked = unpack_bundle(&body, dir.path()).unwrap();
        assert_eq!(unpacked.files, ["app.js", "index.html"]);
        assert!(dir.path().join("index.html").exists());
    }

    #[test]
    fn traversal_aborts_the_whole_upload() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["../escape.html", "a/../../escape.html", "/etc/escape.html"] {
            let body = tarball(&[("index.html", "ok"), (path, "pwned")]);
            let error = unpack_bundle(&body, dir.path()).unwrap_err();
            assert!(error.contains("unsafe path"), "{path:?} gave {error:?}");
        }
        // Nothing from a rejected archive may be left behind outside the dest.
        assert!(!dir.path().parent().unwrap().join("escape.html").exists());
    }

    #[test]
    fn symlinks_are_skipped_and_reported_rather_than_followed() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[("index.html", "ok"), ("passwd.html@", "")]);
        let unpacked = unpack_bundle(&body, dir.path()).unwrap();
        assert_eq!(unpacked.files, ["index.html"]);
        assert!(unpacked.skipped.contains(&"symlinks and special files"));
        assert!(!dir.path().join("passwd.html").exists());
    }

    #[test]
    fn dotfiles_are_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[("index.html", "ok"), (".env", "SECRET=1")]);
        let unpacked = unpack_bundle(&body, dir.path()).unwrap();
        assert_eq!(unpacked.files, ["index.html"]);
        assert!(unpacked.skipped.contains(&"dotfiles"));
        assert!(!dir.path().join(".env").exists());
    }

    #[test]
    fn an_empty_archive_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let error = unpack_bundle(&tarball(&[]), dir.path()).unwrap_err();
        assert!(error.contains("no files"), "got {error:?}");
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        assert!(unpack_bundle(b"not a gzip stream at all", dir.path()).is_err());
    }
}
