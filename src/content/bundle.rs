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
/// The ceilings are counted here too, as the entries go by, so a bundle
/// over one is refused before anything is written, and a bomb is given up
/// on at the ceiling rather than inflated to its end.
pub(crate) fn bundle_entry_paths(body: &[u8]) -> Result<Vec<String>, String> {
    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut paths = Vec::new();
    let mut total: u64 = 0;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        match classify_entry(&entry) {
            EntryVerdict::Take(rel) => {
                total = total.saturating_add(entry.header().size().unwrap_or(0));
                paths.push(rel);
            }
            EntryVerdict::Ignore | EntryVerdict::Skip(_) => continue,
            EntryVerdict::Reject(message) => return Err(message),
        }
        if paths.len() > MAX_BUNDLE_ENTRIES {
            return Err(format!("bundle has more than {MAX_BUNDLE_ENTRIES} files"));
        }
        if total > MAX_BUNDLE_UNPACKED {
            return Err(format!("bundle exceeds {} MB unpacked", MAX_BUNDLE_UNPACKED / 1024 / 1024));
        }
    }
    if paths.is_empty() {
        return Err("bundle contains no files".to_string());
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

/// The .sql files in a gzipped tar, by name, without writing anything to
/// disk. Same entry rules as a bundle, so a migration cannot be a symlink or
/// a path that climbs out.
pub(crate) fn read_sql_files(body: &[u8]) -> Result<Vec<(String, String)>, String> {
    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut files = Vec::new();

    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let rel = match classify_entry(&entry) {
            EntryVerdict::Take(rel) => rel,
            EntryVerdict::Ignore | EntryVerdict::Skip(_) => continue,
            EntryVerdict::Reject(message) => return Err(message),
        };
        if !rel.ends_with(".sql") {
            continue;
        }
        let mut sql = String::new();
        std::io::Read::read_to_string(&mut entry, &mut sql).map_err(|e| e.to_string())?;
        // The directory a file sat in says nothing; its name orders it.
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        files.push((name, sql));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Every regular file in a gzipped tar, in memory, under the same traversal
/// rules a bundle gets. For handing a stored source archive on somewhere
/// else, file by file. Capped, because an archive is attacker-shaped input;
/// what a build or a version control tool leaves behind is not the project.
pub(crate) fn read_all_files(
    body: &[u8],
    max_files: usize,
    max_bytes: usize,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let rel = match classify_entry(&entry) {
            EntryVerdict::Take(rel) => rel,
            EntryVerdict::Ignore | EntryVerdict::Skip(_) => continue,
            EntryVerdict::Reject(message) => return Err(message),
        };
        let first = rel.split('/').next().unwrap_or("");
        if matches!(first, "node_modules" | "target" | "dist") || rel.contains("/node_modules/") {
            continue;
        }
        // Read no more than the budget left, plus one byte to notice going
        // over: a small archive can claim a huge entry, and reading it whole
        // first would hold all of it in memory before the check.
        let mut bytes = Vec::new();
        let budget = (max_bytes.saturating_sub(total) as u64).saturating_add(1);
        std::io::Read::read_to_end(&mut std::io::Read::take(&mut entry, budget), &mut bytes).map_err(|e| e.to_string())?;
        total += bytes.len();
        if files.len() >= max_files || total > max_bytes {
            return Err(format!(
                "archive is larger than {max_files} files or {} MB",
                max_bytes / 1024 / 1024
            ));
        }
        files.push((rel, bytes));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

#[derive(Debug)]
pub struct Unpacked {
    pub files: Vec<String>,
    pub skipped: Vec<&'static str>,
}

/// `slug` is what `dest` serves as: the app, or a page inside one.
pub(crate) fn unpack_bundle(body: &[u8], dest: &std::path::Path, slug: &str) -> Result<Unpacked, String> {
    unpack_into(body, slug, &mut |rel, entry, _size| {
        let out = dest.join(rel);
        // Belt and braces: the path checks should make this impossible,
        // but never write outside the destination.
        if !out.starts_with(dest) {
            return Err(format!("path escapes the app directory: {rel}"));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(entry, &mut file).map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// Where one checked entry of a bundle goes: its path inside the app, its
/// bytes and their length. The `Files` store gives one per backend; the
/// checks above it are the same for all of them.
pub(crate) type Sink<'a> = dyn FnMut(&str, &mut dyn std::io::Read, u64) -> Result<(), String> + 'a;

/// Every entry of a bundle that passes the traversal defence, handed to
/// `sink` in the archive's order. Every path is checked before the first is
/// handed over, as the key it will be stored at, so an archive with one
/// path a store cannot keep writes nothing at all: a refusal half way would
/// leave the app serving half of the new bundle over the old.
pub(crate) fn unpack_into(body: &[u8], slug: &str, sink: &mut Sink<'_>) -> Result<Unpacked, String> {
    let paths = bundle_entry_paths(body)?;
    let strip = bundle_strip_prefix(&paths);
    for path in &paths {
        let rel = match &strip {
            Some(prefix) => path.strip_prefix(&format!("{prefix}/")).unwrap_or(path),
            None => path,
        };
        if !rel.is_empty() && !crate::content::files::valid_key(&format!("{slug}/{rel}")) {
            return Err(format!("unsupported filename in bundle: {rel} (too long to store)"));
        }
    }

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
        if crate::content::store::platform_file(&format!("{slug}/{rel}")) {
            let reason = "files the platform keeps (sidecars such as index.meta, handler.wasm, data.db)";
            if !skipped.contains(&reason) {
                skipped.push(reason);
            }
            continue;
        }

        let size = entry.header().size().unwrap_or(0);
        total += size;
        if total > MAX_BUNDLE_UNPACKED {
            return Err(format!(
                "bundle exceeds {} MB unpacked",
                MAX_BUNDLE_UNPACKED / 1024 / 1024
            ));
        }
        // Checked again here, after the strip, by the rule every store
        // keys on: whatever a sink does with it, this path is a key.
        if !valid_asset_path(&rel) {
            return Err(format!("unsupported filename in bundle: {rel}"));
        }
        sink(&rel, &mut entry, size)?;
        written.push(rel);
    }

    written.sort();
    Ok(Unpacked {
        files: written,
        skipped,
    })
}

/// Archives forged by hand, for the traversal tests here and for the same
/// tests against every `Files` store.
#[cfg(test)]
pub(crate) mod forged {
    use std::io::Write;

    /// Writes tar headers by hand. The `tar` crate's builder refuses to
    /// emit `..` or absolute paths, which is exactly what these tests need to
    /// forge — a real attacker is not constrained by our tar library either.
    pub(crate) fn raw_entry(path: &str, body: &[u8], type_flag: u8, link: &str) -> Vec<u8> {
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
    pub(crate) fn tarball(entries: &[(&str, &str)]) -> Vec<u8> {
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
}

#[cfg(test)]
mod tests {
    use super::forged::tarball;
    use super::*;

    #[test]
    fn a_normal_build_output_unpacks_whole() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[
            ("index.html", "<h1>hi</h1>"),
            ("assets/main-4f2a.js", "console.log(1)"),
            ("assets/main-4f2a.css", "body{}"),
        ]);
        let unpacked = unpack_bundle(&body, dir.path(), "app").unwrap();
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
        let unpacked = unpack_bundle(&body, dir.path(), "app").unwrap();
        assert_eq!(unpacked.files, ["app.js", "index.html"]);
        assert!(dir.path().join("index.html").exists());
    }

    #[test]
    fn traversal_aborts_the_whole_upload() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["../escape.html", "a/../../escape.html", "/etc/escape.html"] {
            let body = tarball(&[("index.html", "ok"), (path, "pwned")]);
            let error = unpack_bundle(&body, dir.path(), "app").unwrap_err();
            assert!(error.contains("unsafe path"), "{path:?} gave {error:?}");
        }
        // Nothing from a rejected archive may be left behind outside the dest.
        assert!(!dir.path().parent().unwrap().join("escape.html").exists());
    }

    #[test]
    fn symlinks_are_skipped_and_reported_rather_than_followed() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[("index.html", "ok"), ("passwd.html@", "")]);
        let unpacked = unpack_bundle(&body, dir.path(), "app").unwrap();
        assert_eq!(unpacked.files, ["index.html"]);
        assert!(unpacked.skipped.contains(&"symlinks and special files"));
        assert!(!dir.path().join("passwd.html").exists());
    }

    #[test]
    fn dotfiles_are_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let body = tarball(&[("index.html", "ok"), (".env", "SECRET=1")]);
        let unpacked = unpack_bundle(&body, dir.path(), "app").unwrap();
        assert_eq!(unpacked.files, ["index.html"]);
        assert!(unpacked.skipped.contains(&"dotfiles"));
        assert!(!dir.path().join(".env").exists());
    }

    #[test]
    fn an_empty_archive_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let error = unpack_bundle(&tarball(&[]), dir.path(), "app").unwrap_err();
        assert!(error.contains("no files"), "got {error:?}");
    }

    /// A gzipped tar of `index.html` first, then `then`: `(path, size)`
    /// entries of zeros, made with the `tar` crate since nothing here needs
    /// forging.
    fn index_then(then: &[(String, u64)]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, "index.html", &b"v2"[..]).unwrap();
            for (path, size) in then {
                let mut header = tar::Header::new_gnu();
                header.set_size(*size);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, std::io::Read::take(std::io::repeat(0), *size)).unwrap();
            }
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn a_bundle_over_a_ceiling_hands_the_store_nothing_at_all() {
        // Over the unpacked ceiling, in one file after the index; over the
        // count of files; and with a name no store could keep. Each is
        // refused before the index reaches the store, where a refusal part
        // way would leave half of the new bundle served over the old.
        let too_big = index_then(&[("big.bin".to_string(), MAX_BUNDLE_UNPACKED)]);
        let too_many = index_then(&(0..MAX_BUNDLE_ENTRIES).map(|n| (format!("f{n}.txt"), 1)).collect::<Vec<_>>());
        let too_long = index_then(&[(format!("{}.js", "x".repeat(300)), 1)]);
        for (body, said) in [(too_big, "unpacked"), (too_many, "more than"), (too_long, "too long")] {
            let mut handed = Vec::new();
            let error = unpack_into(&body, "app", &mut |rel, _, _| {
                handed.push(rel.to_string());
                Ok(())
            })
            .unwrap_err();
            assert!(error.contains(said), "{error}");
            assert!(handed.is_empty(), "the store was handed {handed:?} before the refusal");
        }
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        assert!(unpack_bundle(b"not a gzip stream at all", dir.path(), "app").is_err());
    }
}
