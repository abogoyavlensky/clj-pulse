//! The shared name matcher: one tiered scoring rule for every place that
//! filters symbols by a typed query (workspace/symbol, completion).

/// Match tiers: exact (0) > prefix (1) > substring (2) > subsequence (3).
/// `None` means no match. An empty query matches everything.
pub fn match_score(name: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(3);
    }
    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else if is_subsequence(query, name) {
        Some(3)
    } else {
        None
    }
}

pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_score_tiers() {
        assert_eq!(match_score("add", "add"), Some(0));
        assert_eq!(match_score("add-and-double", "add"), Some(1));
        assert_eq!(match_score("re-add", "add"), Some(2));
        assert_eq!(match_score("a-d-d", "add"), Some(3));
        assert_eq!(match_score("multiply", "add"), None);
    }

    #[test]
    fn test_match_score_empty_query_matches_all() {
        assert_eq!(match_score("anything", ""), Some(3));
    }

    #[test]
    fn test_is_subsequence() {
        assert!(is_subsequence("aad", "add-and-double"));
        assert!(!is_subsequence("xyz", "add"));
    }
}
