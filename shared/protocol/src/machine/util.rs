pub(crate) fn opt_str(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}
