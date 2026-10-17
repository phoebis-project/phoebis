use serde_json::Value;

/// The strict parse, else a parse of a reading copy with every lone surrogate escape mapped to U+FFFD.
pub fn parse_body(body: &[u8]) -> Option<Value> {
    // The client cuts a string mid-pair and sends it, so a body no strict decoder reads still carries the facts the
    // router reads.
    serde_json::from_slice(body).ok().or_else(|| {
        let copy = phoebis_cch::lone_surrogates_mapped(body)?;
        serde_json::from_slice(&copy).ok()
    })
}
