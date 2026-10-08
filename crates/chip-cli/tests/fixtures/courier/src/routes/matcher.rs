//! Path-prefix matching on segment boundaries.

/// `prefix` matches `path` when it is equal to it or a whole leading run of its segments:
/// `/reports` matches `/reports` and `/reports/2026` but not `/reportsx`.
pub fn matches(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Specificity of a match: longer prefixes are more specific.
pub fn specificity(prefix: &str) -> usize {
    prefix.trim_end_matches('/').len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_whole_segments_only() {
        assert!(matches("/reports", "/reports"));
        assert!(matches("/reports", "/reports/2026"));
        assert!(!matches("/reports", "/reportsx"));
        assert!(!matches("/reports", "/other"));
    }

    #[test]
    fn trailing_slashes_on_the_prefix_are_ignored() {
        assert!(matches("/reports/", "/reports/a"));
        assert!(matches("/reports/", "/reports"));
    }

    #[test]
    fn root_matches_everything() {
        assert!(matches("/", "/anything"));
        assert!(matches("", "/"));
    }

    #[test]
    fn longer_prefixes_are_more_specific() {
        assert!(specificity("/a/b") > specificity("/a"));
        assert_eq!(specificity("/a/"), specificity("/a"));
    }
}
