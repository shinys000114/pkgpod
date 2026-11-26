// SPDX-License-Identifier: AGPL-3.0-only

//! Bodies of requests and replies: bytes as they come, from any source
//! (a buffer, a file, a connection), boxed into one type.

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};

/// An error of a body: whatever its source's is.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Uploads are limited to this size.
pub const MAX_UPLOAD: u64 = 8 << 30;
/// A body.
pub type Body = UnsyncBoxBody<Bytes, BoxError>;
pub type Request = http::Request<Body>;
pub type Response = http::Response<Body>;

/// No bytes.
pub fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|e| match e {}).boxed_unsync()
}

/// These bytes, their length known.
pub fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|e| match e {}).boxed_unsync()
}

/// The bytes of `body`, at most `limit` of them: past that an error, as
/// for an error of the body itself.
pub async fn to_bytes(body: Body, limit: usize) -> Result<Bytes, BoxError> {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let mut out = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > limit {
            return Err("length limit exceeded".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.freeze())
}

/// Any body of bytes, boxed (its size hint kept).
pub fn boxed<B>(b: B) -> Body
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    b.map_err(Into::into).boxed_unsync()
}

/// The chunks `s` gives, as a body (its length not known); an error of
/// `s` ends it in that error.
pub fn from_stream<S, D, E>(s: S) -> Body
where
    S: futures_util::Stream<Item = Result<D, E>> + Send + 'static,
    D: Into<Bytes>,
    E: Into<BoxError>,
{
    use futures_util::StreamExt;
    let frames = s.map(|c| c.map(|d| http_body::Frame::data(d.into())));
    boxed(http_body_util::StreamBody::new(frames))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;

    #[test]
    fn bytes_to_a_limit_and_sizes_kept() {
        let rt = |f| futures_util::FutureExt::now_or_never(f).expect("ready");
        assert_eq!(rt(to_bytes(full("abcd"), 4)).unwrap(), "abcd");
        assert!(rt(to_bytes(full("abcde"), 4)).is_err());
        assert_eq!(rt(to_bytes(empty(), 0)).unwrap(), "");
        // Whose length is known says so (a reply then has a
        // Content-Length, not chunks).
        assert_eq!(full("abcd").size_hint().exact(), Some(4));
        assert_eq!(empty().size_hint().exact(), Some(0));
        assert_eq!(boxed(full("ab")).size_hint().exact(), Some(2));
    }

    #[test]
    fn a_stream_s_chunks_and_its_error() {
        let rt = |f| futures_util::FutureExt::now_or_never(f).expect("ready");
        let chunks =
            |c: Vec<Result<&'static str, &'static str>>| from_stream(futures_util::stream::iter(c));
        let b = chunks(vec![Ok("ab"), Ok(""), Ok("cd")]);
        assert_eq!(b.size_hint().exact(), None);
        assert_eq!(rt(to_bytes(b, 4)).unwrap(), "abcd");
        let b = chunks(vec![Ok("ab"), Err("cut short"), Ok("cd")]);
        assert_eq!(rt(to_bytes(b, 10)).unwrap_err().to_string(), "cut short");
        assert_eq!(rt(to_bytes(chunks(vec![]), 0)).unwrap(), "");
    }
}
