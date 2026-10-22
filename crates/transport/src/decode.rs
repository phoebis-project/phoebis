//! Content-Encoding decoders behind a uniform trait, selected from the response head.

use std::io::{self, Write};

use crate::response::ResponseHead;
use crate::TransportError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

/// An unrecognized `Content-Encoding` is rejected rather than treated as `Identity`: passing the still-compressed
/// bytes through as plain text would corrupt the relayed response instead of failing loudly. Several
/// `Content-Encoding` fields read as one list (RFC 9110 sections 5.3 and 8.4), refused as a list in one field is.
pub fn encoding(head: &ResponseHead) -> Result<Encoding, TransportError> {
    let fields: Vec<&str> = head
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, value)| value.as_str())
        .collect();
    if fields.is_empty() {
        return Ok(Encoding::Identity);
    }
    let raw = fields.join(", ");
    match raw.trim().to_ascii_lowercase().as_str() {
        "identity" => Ok(Encoding::Identity),
        "gzip" => Ok(Encoding::Gzip),
        "deflate" => Ok(Encoding::Deflate),
        "br" => Ok(Encoding::Brotli),
        "zstd" => Ok(Encoding::Zstd),
        _ => Err(TransportError::Decode(format!(
            "unsupported content-encoding: {raw}"
        ))),
    }
}

/// The request side of `encoding`: the caller's `content-encoding` token, with the same refusal of an unknown
/// one; `None` and `identity` read `Identity`. Only `gzip` is accepted beside it: the relay inflates a request
/// body through the gzip decoder alone, so `deflate`, `br` and `zstd`, which a response may carry, are refused
/// here as an unknown token is.
pub fn request_encoding(value: Option<&str>) -> Result<Encoding, TransportError> {
    let Some(raw) = value else {
        return Ok(Encoding::Identity);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "identity" => Ok(Encoding::Identity),
        "gzip" => Ok(Encoding::Gzip),
        _ => Err(TransportError::Response(format!(
            "unsupported request content-encoding: {raw}"
        ))),
    }
}

/// The buffer every decoder writes into. It refuses the write that would carry it past the budget of the
/// current call, so the library stops with an I/O error and a compressed bomb costs the budget plus the
/// library's own output block, never the whole expansion.
#[derive(Default)]
struct BoundedSink {
    buf: Vec<u8>,
    budget: usize,
    /// Set by the refused write, so the decoder can tell the bound from any other write error the library
    /// reports through the same `io::Error` type.
    tripped: bool,
    /// The most `buf` held at once; the tests read it to check the bound held while decoding.
    #[cfg(test)]
    high_water: usize,
}

impl Write for BoundedSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > self.budget {
            self.tripped = true;
            return Err(io::Error::other("decoded output past the budget"));
        }
        self.buf.extend_from_slice(data);
        #[cfg(test)]
        {
            self.high_water = self.high_water.max(self.buf.len());
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BoundedSink {
    /// Starts a call under its own budget. A refusal must not outlive the call it happened in: a decoder fed
    /// again afterward (post-commit inspection reusing the hold's decoder is the one caller that does) is
    /// judged on that later write alone, not on a trip a previous, already-answered call left behind.
    fn prepare(&mut self, budget: usize) {
        self.buf.clear();
        self.budget = budget;
        self.tripped = false;
        #[cfg(test)]
        {
            self.high_water = 0;
        }
    }

    /// Ends one decoder call: the refused write answers as the bound, any other failure as the payload the
    /// decoder refused, and a clean call hands over what it decoded.
    fn collect(&mut self, budget: usize, wrote: io::Result<()>) -> Result<Vec<u8>, TransportError> {
        if self.tripped {
            return Err(TransportError::TooLarge(budget));
        }
        wrote.map_err(|error| TransportError::Decode(error.to_string()))?;
        Ok(std::mem::take(&mut self.buf))
    }
}

/// Callers must concatenate every `feed_bounded` result with the final `finish_bounded` result to recover the
/// full output: `finish_bounded` returns only whatever tail is still buffered internally. `budget` bounds the
/// output of the one call; a caller accumulating across calls passes what is left of its own bound.
pub trait Decoder: Send {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError>;
    fn finish_bounded(&mut self, budget: usize) -> Result<Vec<u8>, TransportError>;
    /// The most the sink held at once, for the tests that check the bound held while decoding.
    #[cfg(test)]
    fn retained(&self) -> usize;
}

pub fn decoder(e: Encoding) -> Box<dyn Decoder> {
    match e {
        Encoding::Identity => Box::new(Identity),
        Encoding::Gzip => Box::new(GzipDecoder(Some(flate2::write::GzDecoder::new(
            BoundedSink::default(),
        )))),
        Encoding::Deflate => Box::new(DeflateDecoder(Some(flate2::write::ZlibDecoder::new(
            BoundedSink::default(),
        )))),
        Encoding::Brotli => Box::new(BrotliDecoder(Some(brotli::DecompressorWriter::new(
            BoundedSink::default(),
            4096,
        )))),
        // Context creation only fails under allocation failure, which is unrecoverable here regardless.
        Encoding::Zstd => Box::new(ZstdDecoder(Some(
            zstd::stream::write::Decoder::new(BoundedSink::default())
                .expect("zstd decoder context"),
        ))),
    }
}

struct Identity;

impl Decoder for Identity {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError> {
        if input.len() > budget {
            return Err(TransportError::TooLarge(budget));
        }
        Ok(input.to_vec())
    }

    fn finish_bounded(&mut self, _budget: usize) -> Result<Vec<u8>, TransportError> {
        Ok(Vec::new())
    }

    #[cfg(test)]
    fn retained(&self) -> usize {
        0
    }
}

fn already_finished() -> TransportError {
    TransportError::Response("decoder used after finish".into())
}

struct GzipDecoder(Option<flate2::write::GzDecoder<BoundedSink>>);

impl Decoder for GzipDecoder {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        // A write-side decoder keeps what it just decoded until the next write, so without the flush a caller
        // reading the stream as it arrives would see one chunk's bytes only after the following chunk.
        let wrote = d.write_all(input).and_then(|()| d.flush());
        d.get_mut().collect(budget, wrote)
    }

    fn finish_bounded(&mut self, budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.try_finish();
        let out = d.get_mut().collect(budget, wrote)?;
        self.0 = None;
        Ok(out)
    }

    #[cfg(test)]
    fn retained(&self) -> usize {
        self.0.as_ref().map_or(0, |d| d.get_ref().high_water)
    }
}

struct DeflateDecoder(Option<flate2::write::ZlibDecoder<BoundedSink>>);

impl Decoder for DeflateDecoder {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.write_all(input).and_then(|()| d.flush());
        d.get_mut().collect(budget, wrote)
    }

    fn finish_bounded(&mut self, budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.try_finish();
        let out = d.get_mut().collect(budget, wrote)?;
        self.0 = None;
        Ok(out)
    }

    #[cfg(test)]
    fn retained(&self) -> usize {
        self.0.as_ref().map_or(0, |d| d.get_ref().high_water)
    }
}

struct BrotliDecoder(Option<brotli::DecompressorWriter<BoundedSink>>);

impl Decoder for BrotliDecoder {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.write_all(input).and_then(|()| d.flush());
        d.get_mut().collect(budget, wrote)
    }

    fn finish_bounded(&mut self, budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.close();
        let out = d.get_mut().collect(budget, wrote)?;
        self.0 = None;
        Ok(out)
    }

    #[cfg(test)]
    fn retained(&self) -> usize {
        self.0.as_ref().map_or(0, |d| d.get_ref().high_water)
    }
}

struct ZstdDecoder(Option<zstd::stream::write::Decoder<'static, BoundedSink>>);

impl Decoder for ZstdDecoder {
    fn feed_bounded(&mut self, input: &[u8], budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.write_all(input).and_then(|()| d.flush());
        d.get_mut().collect(budget, wrote)
    }

    fn finish_bounded(&mut self, budget: usize) -> Result<Vec<u8>, TransportError> {
        let d = self.0.as_mut().ok_or_else(already_finished)?;
        d.get_mut().prepare(budget);
        let wrote = d.flush();
        let out = d.get_mut().collect(budget, wrote)?;
        self.0 = None;
        Ok(out)
    }

    #[cfg(test)]
    fn retained(&self) -> usize {
        self.0.as_ref().map_or(0, |d| d.get_ref().high_water)
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
        assert!(matches!(
            encoding(&head(vec![("Content-Encoding", "gzip, br")])),
            Err(TransportError::Decode(_))
        ));
    }

    #[test]
    fn two_content_encoding_fields_read_as_one_list_and_are_rejected() {
        assert!(matches!(
            encoding(&head(vec![
                ("Content-Encoding", "gzip"),
                ("Content-Encoding", "gzip")
            ])),
            Err(TransportError::Decode(_))
        ));
    }

    #[test]
    fn a_request_with_no_coding_or_identity_reads_identity() {
        assert_eq!(request_encoding(None).unwrap(), Encoding::Identity);
        assert_eq!(
            request_encoding(Some("Identity")).unwrap(),
            Encoding::Identity
        );
    }

    #[test]
    fn a_gzip_request_coding_reads_gzip_in_any_case() {
        assert_eq!(request_encoding(Some("gzip")).unwrap(), Encoding::Gzip);
        assert_eq!(request_encoding(Some(" GZIP ")).unwrap(), Encoding::Gzip);
    }

    /// The response side decodes these, but a request body is inflated through the gzip decoder alone.
    #[test]
    fn every_request_coding_but_gzip_and_identity_is_refused() {
        for token in [
            "br", "compress", "deflate", "gzip, br", "x-gzip", "zstd", "",
        ] {
            assert!(request_encoding(Some(token)).is_err(), "{token:?}");
        }
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
    /// `Decoder` trait documents: every `feed_bounded` result plus the final `finish_bounded` result must equal
    /// the input.
    fn round_trip(e: Encoding, compressed: &[u8]) -> Vec<u8> {
        let mut d = decoder(e);
        let mut out = Vec::new();
        for chunk in compressed.chunks(7) {
            out.extend(d.feed_bounded(chunk, 1 << 20).unwrap());
        }
        out.extend(d.finish_bounded(1 << 20).unwrap());
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

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    /// One compressed frame per coding, each decoding to 8 MiB of zeros: small enough on the wire to fit a
    /// single `feed_bounded` call, large enough decoded to prove the budget bites before the caller would.
    fn bombs() -> Vec<(Encoding, Vec<u8>)> {
        let zeros = vec![0u8; 8 << 20];
        let mut br = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        br.write_all(&zeros).unwrap();
        br.flush().unwrap();
        let brotli = br.into_inner();
        let zstd = zstd::stream::encode_all(&zeros[..], 19).unwrap();
        vec![
            (Encoding::Gzip, gzip(&zeros)),
            (Encoding::Brotli, brotli),
            (Encoding::Zstd, zstd),
        ]
    }

    #[test]
    fn a_gzip_payload_that_does_not_inflate_is_refused_as_undecodable() {
        let mut corrupt = gzip(&sample_bytes());
        // The first byte past the ten-byte header opens a final block of the reserved type 3, which no inflater
        // reads.
        corrupt[10] = 0xff;
        let mut d = decoder(Encoding::Gzip);
        let error = d
            .feed_bounded(&corrupt, 1 << 20)
            .and_then(|_| d.finish_bounded(1 << 20))
            .unwrap_err();
        assert!(matches!(error, TransportError::Decode(_)), "{error:?}");
    }

    #[test]
    fn a_feed_past_the_budget_is_refused_and_the_sink_never_held_more_than_the_budget() {
        for (coding, frame) in bombs() {
            let mut d = decoder(coding);
            let error = d.feed_bounded(&frame, 64 * 1024).unwrap_err();
            assert!(matches!(error, TransportError::TooLarge(65536)), "{error}");
            assert!(d.retained() <= 64 * 1024 + 64 * 1024, "{}", d.retained());
        }
    }

    #[test]
    fn a_write_after_a_refusal_is_judged_on_its_own_size() {
        let mut sink = BoundedSink {
            budget: 4,
            ..Default::default()
        };
        assert!(sink.write(b"12345").is_err(), "past the budget of 4");
        assert!(matches!(
            sink.collect(4, Ok(())),
            Err(TransportError::TooLarge(4))
        ));
        // A caller that feeds the sink again (post-commit inspection reusing the hold's decoder is the one
        // that does) prepares it under its own budget first, so the earlier trip does not answer for it.
        sink.prepare(10);
        sink.write_all(b"ok").unwrap();
        assert_eq!(sink.collect(10, Ok(())).unwrap(), b"ok");
    }

    #[test]
    fn a_finish_that_writes_past_the_budget_is_refused_at_the_bound() {
        // Every library hands its output over during `feed_bounded`, whose flush drains it, so the one way to
        // reach the sink at finish is the writer's own undumped block: the frame is written without a flush,
        // the sink is emptied by hand, and the tail the writer still holds lands at `try_finish`. The payload
        // is 5_000 bytes past a multiple of the writer's block, so that tail is never empty.
        let mut inner = flate2::write::GzDecoder::new(BoundedSink {
            budget: usize::MAX,
            ..Default::default()
        });
        inner
            .write_all(&gzip(&vec![0u8; (8 << 20) + 5_000]))
            .unwrap();
        *inner.get_mut() = BoundedSink::default();
        let mut d = GzipDecoder(Some(inner));
        let error = d.finish_bounded(1024).unwrap_err();
        assert!(matches!(error, TransportError::TooLarge(1024)), "{error}");
        assert!(d.retained() <= 1024, "{}", d.retained());
    }

    #[test]
    fn a_frame_within_the_budget_decodes_whole_across_feed_and_finish() {
        for (coding, frame) in bombs() {
            let mut d = decoder(coding);
            let mut out = d.feed_bounded(&frame, 16 << 20).unwrap();
            out.extend(d.finish_bounded((16 << 20) - out.len()).unwrap());
            assert_eq!(out.len(), 8 << 20);
        }
    }
}
