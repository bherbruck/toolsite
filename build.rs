//! Packs each app under `examples/` into a tarball the server embeds, so
//! `/examples/<name>.tar.gz` serves the source as it is in this checkout.
//!
//! Dependencies and build output stay out: `node_modules`, `target` and
//! `dist` are skipped at any depth. A symlink is followed, which is how each
//! handler's `wit/` points at the one contract in `wit/` and still arrives
//! as a real file in the tarball.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const SKIP: [&str; 3] = ["node_modules", "target", "dist"];

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let root = Path::new("examples");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=examples");

    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().join("toolsite.toml").is_file())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();

    let mut generated = String::from("pub(crate) const EXAMPLES: &[Example] = &[\n");
    for name in &names {
        let dir = root.join(name);
        let mut files = Vec::new();
        collect(&dir, Path::new(""), &mut files);
        files.sort();

        let mut builder = tar::Builder::new(Vec::new());
        for relative in &files {
            let body = std::fs::read(dir.join(relative)).expect("read an example file");
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            let path = format!("{}", relative.display());
            builder.append_data(&mut header, path, body.as_slice()).expect("append to the tarball");
        }
        let tar = builder.into_inner().expect("finish the tarball");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&tar).expect("compress the tarball");
        let gz = gz.finish().expect("finish compressing");
        std::fs::write(out.join(format!("example-{name}.tar.gz")), gz).expect("write the tarball");

        let description = description(&dir.join("README.md"));
        writeln!(
            generated,
            "    Example {{ name: {name:?}, description: {description:?}, tarball: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/example-{name}.tar.gz\")) }},"
        )
        .unwrap();
    }
    generated.push_str("];\n");
    std::fs::write(out.join("examples.rs"), generated).expect("write examples.rs");
}

/// Every file under `dir`, as paths relative to the example's root.
fn collect(base: &Path, relative: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(base.join(relative)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if SKIP.contains(&name) {
            continue;
        }
        let path = relative.join(name);
        // metadata, not symlink_metadata: a link is followed to what it names.
        let Ok(meta) = std::fs::metadata(base.join(&path)) else { continue };
        if meta.is_dir() {
            collect(base, &path, files);
        } else if meta.is_file() {
            files.push(path);
        }
    }
}

/// The first sentence of the README's first paragraph after its heading.
fn description(readme: &Path) -> String {
    let text = std::fs::read_to_string(readme).unwrap_or_default();
    let paragraph: Vec<&str> = text
        .lines()
        .skip_while(|l| l.starts_with('#') || l.trim().is_empty())
        .take_while(|l| !l.trim().is_empty())
        .map(str::trim)
        .collect();
    let joined = paragraph.join(" ");
    match joined.find(". ") {
        Some(end) => joined[..=end].to_string(),
        None => joined,
    }
}
