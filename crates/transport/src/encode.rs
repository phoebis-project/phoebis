//! The request-body gzip encoder, the sibling of the response decoders in `decode`.

use std::io::Write;

use flate2::write::GzEncoder;
use flate2::Compression;

/// The request-body gzip encoder, the sibling of `decode::decoder(Encoding::Gzip)`: the whole body in one call,
/// since the relay holds a request body in memory and edits it before it leaves.
pub fn gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(input)
        .expect("writing into a Vec cannot fail");
    encoder.finish().expect("writing into a Vec cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{decoder, Encoding};

    #[test]
    fn the_encoders_output_inflates_back_to_its_input_through_the_gzip_decoder() {
        let input: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        let encoded = gzip(&input);
        assert_ne!(encoded, input);
        let mut d = decoder(Encoding::Gzip);
        let mut out = d.feed_bounded(&encoded, input.len()).unwrap();
        out.extend(d.finish_bounded(input.len() - out.len()).unwrap());
        assert_eq!(out, input);
    }

    #[test]
    fn an_empty_input_encodes_to_a_valid_empty_gzip_stream() {
        let encoded = gzip(&[]);
        assert_eq!(&encoded[..2], &[0x1f, 0x8b], "a gzip member's magic bytes");
        let mut d = decoder(Encoding::Gzip);
        let mut out = d.feed_bounded(&encoded, 0).unwrap();
        out.extend(d.finish_bounded(0).unwrap());
        assert!(out.is_empty());
    }
}
