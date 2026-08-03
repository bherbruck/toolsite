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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_slugs_reject_anything_that_could_leave_the_data_dir() {
        assert!(valid_slug("page"));
        assert!(valid_slug("my-app/about_page"));
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "a/../b",
            "/leading",
            "trailing/",
            "a//b",
            "with space",
            "with.dot",
            "unicode\u{2044}slash",
            "semi;colon",
        ] {
            assert!(!valid_slug(bad), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn asset_paths_allow_build_output_names_but_not_dotfiles() {
        assert!(valid_asset_path("assets/index-4f2a1c.js"));
        assert!(valid_asset_path("index.html"));
        assert!(valid_asset_path("a/b/c/d.woff2"));
        for bad in ["../secret", "a/../b", ".env", "a/.git/config", "", "a//b"] {
            assert!(!valid_asset_path(bad), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn a_dot_can_never_start_a_segment_so_dot_dot_is_unreachable() {
        // The traversal defence rests on this, so state it directly.
        assert!(!valid_asset_path(".."));
        assert!(!valid_asset_path("ok/.."));
        assert!(valid_asset_path("ok/a..b"));
    }

    #[test]
    fn random_tokens_are_the_requested_length_and_url_safe() {
        let token = random_token(32);
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(random_token(32), random_token(32));
    }
}
