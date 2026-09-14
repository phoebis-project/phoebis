//! Fake upstream server for testing.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_links() {
        assert!(!env!("CARGO_PKG_NAME").is_empty());
    }
}
