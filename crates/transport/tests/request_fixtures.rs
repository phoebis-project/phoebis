use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use phoebis_profile::fixtures::{control_head, inference_head, ControlFixture, FixtureKind};
use phoebis_transport::request::{
    parse_head, serialize_head, write_request, RawRequest, WriteProgress,
};

fn roundtrip(raw: &[u8]) {
    let (line, headers) = parse_head(raw).unwrap();
    let (method, rest) = line.split_once(' ').unwrap();
    let target = rest.trim_end_matches(" HTTP/1.1").to_string();
    let req = RawRequest {
        method: Box::leak(method.to_string().into_boxed_str()),
        target,
        headers,
        body: Bytes::new(),
    };
    let head_len = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(serialize_head(&req), &raw[..head_len]);
}

#[test]
fn inference_heads_serialize_byte_identical() {
    for k in [
        FixtureKind::OauthNonFirstParty,
        FixtureKind::OauthFirstParty,
        FixtureKind::ApiKey,
        FixtureKind::Hello,
    ] {
        roundtrip(inference_head(k));
    }
}

#[test]
fn control_heads_serialize_byte_identical() {
    for k in [
        ControlFixture::Token,
        ControlFixture::Profile,
        ControlFixture::Roles,
        ControlFixture::Usage,
    ] {
        roundtrip(control_head(k));
    }
}

/// Takes every byte offered up to `limit`, then fails; `fail_flush` fails the flush instead of a write.
struct Budget {
    limit: usize,
    taken: Vec<u8>,
    fail_flush: bool,
}

impl Budget {
    fn new(limit: usize, fail_flush: bool) -> Budget {
        Budget {
            limit,
            taken: Vec::new(),
            fail_flush,
        }
    }
}

impl tokio::io::AsyncWrite for Budget {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.taken.len() >= self.limit {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "budget",
            )));
        }
        let n = buf.len().min(self.limit - self.taken.len());
        self.taken.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.fail_flush {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "flush",
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn every_write_failure_reports_the_progress_it_reached() {
    let req = RawRequest {
        method: "POST",
        target: "/v1/messages".into(),
        headers: vec![("Host".into(), "api.anthropic.com".into())],
        body: Bytes::from_static(b"0123456789"),
    };
    let head = serialize_head(&req).len();
    let cases = [
        (0, false, WriteProgress::NothingWritten),
        (head - 1, false, WriteProgress::NothingWritten),
        (head, false, WriteProgress::HeadWritten),
        (head + 3, false, WriteProgress::HeadWritten),
        (head + req.body.len(), true, WriteProgress::BodyWritten),
    ];
    for (limit, fail_flush, want) in cases {
        let mut w = Budget::new(limit, fail_flush);
        let err = write_request(&mut w, &req)
            .await
            .expect_err("the writer fails");
        assert_eq!(err.progress, want, "limit {limit}");
        if want == WriteProgress::BodyWritten {
            let mut wire = serialize_head(&req);
            wire.extend_from_slice(&req.body);
            assert_eq!(w.taken, wire, "the head and then the body, every byte");
        }
    }
    let mut w = Budget::new(usize::MAX, false);
    write_request(&mut w, &req)
        .await
        .expect("a healthy writer succeeds");
}
