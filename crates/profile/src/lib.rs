//! Pinned 2.1.269 constants and fixtures.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_links() {
        assert!(!env!("CARGO_PKG_NAME").is_empty());
    }
}
