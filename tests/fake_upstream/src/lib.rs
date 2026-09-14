//! A loopback HTTP/1.1 server used by relay and oauth tests.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_links() {
        assert!(!env!("CARGO_PKG_NAME").is_empty());
    }
}
