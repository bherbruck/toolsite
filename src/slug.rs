use rand::RngExt;

pub(crate) fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub(crate) fn random_slug() -> String {
    random_token(8)
}

pub(crate) fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.split('/').all(valid_segment)
}

/// Looser than `valid_slug` because built bundles ship names like
/// `main.4f2a1c.js`. Dots are allowed inside a segment but a segment may not
/// start with one, which rules out `..` and dotfiles in a single stroke.
pub(crate) fn valid_asset_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
