use std::time::{SystemTime, UNIX_EPOCH};

/// Get current timestamp in milliseconds since UNIX epoch.
#[inline]
pub fn timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Get current timestamp in seconds since UNIX epoch.
#[inline]
pub fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Split a comma-separated string into a vector of trimmed, non-empty strings.
pub fn split_csv(input: Option<String>) -> Vec<String> {
    input
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_millis() {
        let ts = timestamp_millis();
        assert!(ts > 0);
    }

    #[test]
    fn test_split_csv_basic() {
        let result = split_csv(Some("a,b,c".to_string()));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_csv_with_whitespace() {
        let result = split_csv(Some(" a , b , c ".to_string()));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_csv_empty() {
        let result = split_csv(None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_split_csv_filters_empty() {
        let result = split_csv(Some("a,,b,  ,c".to_string()));
        assert_eq!(result, vec!["a", "b", "c"]);
    }
}
