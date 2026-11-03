use serde_json::Value;

/// The strict parse, else a parse of a reading copy with every lone surrogate escape mapped to U+FFFD.
pub fn parse_body(body: &[u8]) -> Option<Value> {
    // The client cuts a string mid-pair and sends it, so a body no strict decoder reads still carries the facts the
    // router reads.
    phoebis_cch::decode_tolerant(body)
}
