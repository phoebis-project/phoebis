//! Content-Encoding decoders behind a uniform trait, selected from the response head.

use std::io::Write;

use crate::response::ResponseHead;
use crate::TransportError;

pub enum Encoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

/// An unrecognized `Content-Encoding` is rejected rather than treated as `Identity`: passing the still-compressed
/// bytes through as plain text would corrupt the relayed response instead of failing loudly.
pub fn encoding(head: &ResponseHead) -> Result<Encoding, TransportError> {
    let Some(raw) = head.get("content-encoding") else {
        return Ok(Encoding::Identity);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "identity" => Ok(Encoding::Identity),
        "gzip" => Ok(Encoding::Gzip),
        "deflate" => Ok(Encoding::Deflate),
        "br" => Ok(Encoding::Brotli),
        "zstd" => Ok(Encoding::Zstd),
        _ => Err(TransportError::Response(format!(
            "unsupported content-encoding: {raw}"
        ))),
    }
}

/// Callers must concatenate every `feed` result with the final `finish` result to recover the full output:
/// `finish` returns only whatever tail is still buffered internally, not the whole decoded stream.
pub trait Decoder: Send {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError>;
    fn finish(&mut self) -> Result<Vec<u8>, TransportError>;
}

pub fn decoder(e: Encoding) -> Box<dyn Decoder> {
    match e {
        Encoding::Identity => Box::new(Identity),
        Encoding::Gzip => Box::new(GzipDecoder(Some(flate2::write::GzDecoder::new(Vec::new())))),
        Encoding::Deflate => Box::new(DeflateDecoder(Some(flate2::write::ZlibDecoder::new(
            Vec::new(),
        )))),
        Encoding::Brotli => Box::new(BrotliDecoder(Some(brotli::DecompressorWriter::new(
            Vec::new(),
            4096,
        )))),
        // Context creation only fails under allocation failure, which is unrecoverable here regardless.
        Encoding::Zstd => Box::new(ZstdDecoder(Some(
            zstd::stream::write::Decoder::new(Vec::new()).expect("zstd decoder context"),
        ))),
    }
}

struct Identity;

impl Decoder for Identity {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        Ok(input.to_vec())
    }

    fn finish(&mut self) -> Result<Vec<u8>, TransportError> {
        Ok(Vec::new())
    }
}

fn already_finished() -> TransportError {
    TransportError::Response("decoder used after finish".into())
}

struct GzipDecoder(Option<flate2::write::GzDecoder<Vec<u8>>>);

impl Decoder for GzipDecoder {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.write_all(input)?;
        Ok(std::mem::take(d.get_mut()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, TransportError> {
        let d = self.0.take().ok_or_else(already_finished)?;
        Ok(d.finish()?)
    }
}

struct DeflateDecoder(Option<flate2::write::ZlibDecoder<Vec<u8>>>);

impl Decoder for DeflateDecoder {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.write_all(input)?;
        Ok(std::mem::take(d.get_mut()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, TransportError> {
        let d = self.0.take().ok_or_else(already_finished)?;
        Ok(d.finish()?)
    }
}

struct BrotliDecoder(Option<brotli::DecompressorWriter<Vec<u8>>>);

impl Decoder for BrotliDecoder {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.write_all(input)?;
        Ok(std::mem::take(d.get_mut()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut d = self.0.take().ok_or_else(already_finished)?;
        d.close()?;
        d.into_inner()
            .map_err(|_| TransportError::Response("brotli stream did not finish cleanly".into()))
    }
}

struct ZstdDecoder(Option<zstd::stream::write::Decoder<'static, Vec<u8>>>);

impl Decoder for ZstdDecoder {
    fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.write_all(input)?;
        Ok(std::mem::take(d.get_mut()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut d = self.0.take().ok_or_else(already_finished)?;
        d.flush()?;
        Ok(d.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(headers: Vec<(&str, &str)>) -> ResponseHead {
        ResponseHead {
            status: 200,
            headers: headers
                .into_iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn absent_header_is_identity() {
        assert!(matches!(encoding(&head(vec![])), Ok(Encoding::Identity)));
    }

    #[test]
    fn uppercase_token_is_recognized() {
        assert!(matches!(
            encoding(&head(vec![("Content-Encoding", "GZIP")])),
            Ok(Encoding::Gzip)
        ));
    }

    #[test]
    fn zstd_is_recognized() {
        assert!(matches!(
            encoding(&head(vec![("Content-Encoding", "zstd")])),
            Ok(Encoding::Zstd)
        ));
    }

    #[test]
    fn unknown_token_is_rejected() {
        assert!(encoding(&head(vec![("Content-Encoding", "gzip, br")])).is_err());
    }

    /// A few KB of repetitive-but-not-trivial text, long enough to span many 7-byte feed pieces.
    fn sample_bytes() -> Vec<u8> {
        let mut out = String::new();
        for i in 0..120 {
            out.push_str(&format!(
                "line {i}: the quick brown fox jumps over the lazy dog\n"
            ));
        }
        out.into_bytes()
    }

    /// Feeds compressed bytes through `decoder(e)` in small pieces, exercising the concatenation contract the
    /// `Decoder` trait documents: every `feed` result plus the final `finish` result must equal the input.
    fn round_trip(e: Encoding, compressed: &[u8]) -> Vec<u8> {
        let mut d = decoder(e);
        let mut out = Vec::new();
        for chunk in compressed.chunks(7) {
            out.extend(d.feed(chunk).unwrap());
        }
        out.extend(d.finish().unwrap());
        out
    }

    #[test]
    fn gzip_round_trips_through_small_feeds() {
        let sample = sample_bytes();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&sample).unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(round_trip(Encoding::Gzip, &compressed), sample);
    }

    #[test]
    fn deflate_round_trips_through_small_feeds() {
        let sample = sample_bytes();
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&sample).unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(round_trip(Encoding::Deflate, &compressed), sample);
    }

    #[test]
    fn brotli_round_trips_through_small_feeds() {
        let sample = sample_bytes();
        let mut enc = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        enc.write_all(&sample).unwrap();
        enc.flush().unwrap();
        let compressed = enc.into_inner();
        assert_eq!(round_trip(Encoding::Brotli, &compressed), sample);
    }

    #[test]
    fn zstd_round_trips_through_small_feeds() {
        let sample = sample_bytes();
        let compressed = zstd::stream::encode_all(&sample[..], 0).unwrap();
        assert_eq!(round_trip(Encoding::Zstd, &compressed), sample);
    }
}
