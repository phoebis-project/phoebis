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

/// An unrecognized `Content-Encoding` maps to `Identity`: the bytes pass through unchanged, and any mismatch
/// between the declared and actual encoding then surfaces as a parse error wherever the body is consumed.
pub fn encoding(head: &ResponseHead) -> Encoding {
    match head
        .get("content-encoding")
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("gzip") => Encoding::Gzip,
        Some("deflate") => Encoding::Deflate,
        Some("br") => Encoding::Brotli,
        Some("zstd") => Encoding::Zstd,
        _ => Encoding::Identity,
    }
}

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
