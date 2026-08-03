use crate::slug::valid_asset_path;

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
