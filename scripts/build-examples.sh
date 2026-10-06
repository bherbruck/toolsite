#!/bin/sh
# Rebuilds the example fixtures in tests/fixtures/examples/ from examples/.
#
# For each example: its handler as <name>.wasm, its published web root as
# <name>-dist.tar.gz, and <name>.hash, a digest of its source. tests/examples.rs
# recomputes the digest and fails when the source changed after this ran, so
# a stale fixture cannot pass for the current code.
#
# Run it after any change under examples/ or to wit/toolsite.wit. Needs cargo
# with the wasm32-wasip2 target, and npm. Name examples to rebuild only
# those: scripts/build-examples.sh live-board orders
set -eu

cd "$(dirname "$0")/.."
out=tests/fixtures/examples
mkdir -p "$out"
rustup target add wasm32-wasip2 >/dev/null 2>&1 || true

# The digest tests/examples.rs computes: one "<sha256>  <path>" line per file,
# sorted by path, then the sha256 of those lines. node_modules, target and
# dist are skipped; symlinks are followed, so the WIT a handler links is in it.
source_hash() {
    (
        cd "$1"
        find -L . \( -name node_modules -o -name target -o -name dist \) -prune -o -type f -print \
            | sed 's|^\./||' | LC_ALL=C sort \
            | while IFS= read -r f; do
                printf '%s  %s\n' "$(sha256sum < "$f" | cut -d' ' -f1)" "$f"
            done \
            | sha256sum | cut -d' ' -f1
    )
}

if [ $# -gt 0 ]; then
    dirs=$(for name in "$@"; do echo "examples/$name/"; done)
else
    dirs=$(ls -d examples/*/)
fi

for dir in $dirs; do
    name=$(basename "$dir")
    [ -f "$dir/toolsite.toml" ] || continue
    echo "== $name"

    if [ -f "$dir/handler/Cargo.toml" ]; then
        cargo build --quiet --release --target wasm32-wasip2 --manifest-path "$dir/handler/Cargo.toml"
        cp "$dir/handler/target/wasm32-wasip2/release/$(echo "$name" | tr - _)_handler.wasm" "$out/$name.wasm"
    fi

    if [ -f "$dir/package.json" ]; then
        (cd "$dir" && npm ci --no-audit --no-fund --silent && npm run --silent build)
        root="$dir/dist"
    else
        root="$dir/public"
    fi
    # Fixed order, owner and times, so an unchanged build gives the same bytes.
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='@0' -C "$root" -cf - . | gzip -n -9 > "$out/$name-dist.tar.gz"

    source_hash "$dir" > "$out/$name.hash"
done

echo "fixtures in $out"
