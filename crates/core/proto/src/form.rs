// SPDX-License-Identifier: AGPL-3.0-only

//! Upload forms (`multipart/form-data`) as the formats' protocols read
//! them: the parts in order, a text field read whole up to a size, a
//! file as a stream of its bytes. A request that is not a form, or a
//! form that cannot be read, is told in the words clients were given
//! before (axum's).
//!
//! multer, which reads the forms, holds all it has read while it looks
//! for a form's first delimiter and for the end of a part's headers;
//! those are read up to [`MAX_HEAD`], past it the form is unreadable. A
//! part's bytes are handed on as they come, bounded only by the form's
//! limit.

use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http_body_util::BodyExt;

use crate::body::{BoxError, Request};

/// Said of a request with no form's boundary.
pub const NOT_A_FORM: &str = "Invalid `boundary` for `multipart/form-data` request";
/// Said of a form that cannot be read (cut short, malformed, too large).
pub const UNREADABLE: &str = "Error parsing `multipart/form-data` request";

/// The text fields of upload forms are read up to this size.
pub const MAX_FIELD: usize = 64 << 10;

/// What comes before a form's first delimiter (with it), and each
/// part's headers (with the blank line after them), are read up to this
/// size.
pub const MAX_HEAD: usize = 64 << 10;

/// Said of an upload that cannot be read on (a form's part, or any body
/// being stored), `e` being why.
pub fn reading_failed(e: impl std::fmt::Display) -> String {
    format!("reading the upload: {e}")
}

/// Said of a form's field `name` larger than the `max` bytes it is read
/// up to.
pub fn field_too_large(name: &str, max: impl std::fmt::Display) -> String {
    format!("the form's field {name:?} is larger than {max} bytes")
}

/// Why a form is not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormError {
    NotAForm,
    Unreadable,
    /// The next part was asked for while the body of the one before
    /// ([`Field::into_body`]) was still held: a format's mistake, not
    /// its client's.
    Held,
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FormError::NotAForm => NOT_A_FORM,
            FormError::Unreadable => UNREADABLE,
            FormError::Held => "the form's part before is still being read",
        })
    }
}

impl std::error::Error for FormError {}

/// Why multer did not read on.
fn why(e: multer::Error) -> FormError {
    match e {
        multer::Error::LockFailure => FormError::Held,
        _ => FormError::Unreadable,
    }
}

/// A request's form.
pub struct Form {
    parts: multer::Multipart<'static>,
    /// Found unreadable, and so it stays: multer does not keep its
    /// errors, and asked again would read on from where it stopped (a
    /// part's headers searched for in what follows, held whole).
    unreadable: bool,
}

/// A part of a form, borrowing it: the form gives its next part only
/// once this one is let go of.
pub struct Field<'a> {
    part: multer::Field<'static>,
    form: PhantomData<&'a mut Form>,
}

impl Form {
    /// The form `req` carries, read up to `limit` bytes in all (and its
    /// preamble and each part's headers up to [`MAX_HEAD`]).
    pub fn of(req: Request, limit: u64) -> Result<Form, FormError> {
        let boundary = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| multer::parse_boundary(ct).ok())
            .ok_or(FormError::NotAForm)?;
        let mut seen = 0u64;
        let mut framing = Framing::new(&boundary);
        let data = req.into_body().into_data_stream().map(move |chunk| {
            let chunk = chunk?;
            seen += chunk.len() as u64;
            if seen > limit {
                return Err::<Bytes, BoxError>("length limit exceeded".into());
            }
            framing.feed(&chunk)?;
            Ok(chunk)
        });
        Ok(Form {
            parts: multer::Multipart::new(data, boundary),
            unreadable: false,
        })
    }

    /// The next part, if there is one (what is left of the one before
    /// is skipped). A part borrows its form, so the one before is let
    /// go of first; asking for the next while holding it does not
    /// compile:
    ///
    /// ```compile_fail,E0499
    /// # async fn f(form: &mut pkgpod_proto::form::Form) {
    /// let a = form.next_field().await.unwrap().unwrap();
    /// let b = form.next_field().await;
    /// drop(a);
    /// # }
    /// ```
    ///
    /// ```
    /// # async fn f(form: &mut pkgpod_proto::form::Form) {
    /// let a = form.next_field().await.unwrap().unwrap();
    /// drop(a);
    /// let b = form.next_field().await;
    /// # }
    /// ```
    ///
    /// A part's body ([`Field::into_body`]) does not borrow the form: while
    /// it is held, the next part is [`FormError::Held`]. Once the form is
    /// found unreadable, it is so to every later call.
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>, FormError> {
        if self.unreadable {
            return Err(FormError::Unreadable);
        }
        let part = match self.parts.next_field().await {
            Ok(part) => part,
            Err(e) => {
                let e = why(e);
                self.unreadable = e == FormError::Unreadable;
                return Err(e);
            }
        };
        Ok(part.map(|part| Field {
            part,
            form: PhantomData,
        }))
    }
}

impl Field<'_> {
    pub fn name(&self) -> Option<&str> {
        self.part.name()
    }

    pub fn file_name(&self) -> Option<&str> {
        self.part.file_name()
    }

    /// The part's headers (its Content-Type, Content-Transfer-Encoding
    /// and the like).
    pub fn headers(&self) -> &http::HeaderMap {
        self.part.headers()
    }

    /// The part read whole, refused above `max` bytes (a part is
    /// otherwise bounded only by the form's limit, gigabytes).
    pub async fn capped(mut self, max: usize) -> Result<Vec<u8>, String> {
        let name = self.name().unwrap_or("").to_string();
        let mut out = Vec::new();
        while let Some(chunk) = self
            .part
            .chunk()
            .await
            .map_err(|e| reading_failed(why(e)))?
        {
            if out.len() + chunk.len() > max {
                return Err(field_too_large(&name, max));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

impl Field<'_> {
    /// The part's bytes as a body (to be stored); a form that cannot be
    /// read ends it in an error. The body is let go of (read to its end,
    /// or dropped) before the form's next part is asked for.
    pub fn into_body(self) -> crate::body::Body {
        crate::body::from_stream(self.part.map(|c| c.map_err(why)))
    }
}

impl Stream for Field<'_> {
    type Item = Result<Bytes, FormError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.part)
            .poll_next(cx)
            .map(|c| c.map(|c| c.map_err(why)))
    }
}

/// Where a form's bytes are in its framing, followed on their way to
/// multer as multer 3.1 reads them: the first `--` and boundary, found
/// anywhere; after each delimiter `--` (the form ends: what follows is
/// not read), or spaces and tabs then CRLF; a part's headers, up to the
/// blank line (CRLF CRLF); its bytes, up to the next delimiter (CRLF,
/// `--` and the boundary). What multer refuses there is refused at once,
/// and so is a form whose preamble or a part's headers run past
/// MAX_HEAD; once refused, all that follows is.
struct Framing {
    at: At,
    /// Bytes of the preamble, or of the headers, so far.
    run: usize,
    first: Seek,
    delimiter: Seek,
    blank: Seek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Preamble,
    /// Just after a delimiter.
    Delimiter,
    /// A `-` after a delimiter.
    Dash,
    /// Spaces or tabs after a delimiter.
    Padding,
    /// A CR after a delimiter.
    Cr,
    Headers,
    Data,
    Closed,
    Refused,
}

impl Framing {
    fn new(boundary: &str) -> Framing {
        Framing {
            at: At::Preamble,
            run: 0,
            first: Seek::new(format!("--{boundary}").into_bytes()),
            delimiter: Seek::new(format!("\r\n--{boundary}").into_bytes()),
            blank: Seek::new(b"\r\n\r\n".to_vec()),
        }
    }

    /// Follows the form's next bytes: an error if the form is refused.
    fn feed(&mut self, mut b: &[u8]) -> Result<(), BoxError> {
        while !b.is_empty() {
            let (seek, then, held) = match self.at {
                At::Preamble => (&mut self.first, At::Delimiter, true),
                At::Headers => (&mut self.blank, At::Data, true),
                At::Data => (&mut self.delimiter, At::Delimiter, false),
                At::Closed | At::Refused => break,
                at => {
                    self.at = match (at, b[0]) {
                        (At::Delimiter | At::Padding, b' ' | b'\t') => At::Padding,
                        (At::Delimiter, b'-') => At::Dash,
                        (At::Delimiter | At::Padding, b'\r') => At::Cr,
                        (At::Dash, b'-') => At::Closed,
                        (At::Cr, b'\n') => At::Headers,
                        _ => At::Refused,
                    };
                    b = &b[1..];
                    continue;
                }
            };
            match seek.find(b) {
                Some(end) if !held || self.run + end <= MAX_HEAD => {
                    self.at = then;
                    self.run = 0;
                    b = &b[end..];
                }
                None if !held => break,
                None if self.run + b.len() < MAX_HEAD => {
                    self.run += b.len();
                    break;
                }
                _ => self.at = At::Refused,
            }
        }
        if self.at == At::Refused {
            return Err("not a form as multer reads one, or its headers too large".into());
        }
        Ok(())
    }
}

/// A search for a pattern in bytes that come in chunks: where it first
/// ends, a match begun in one chunk carried into the next (KMP, so that
/// a pattern that overlaps itself is not missed).
struct Seek {
    pat: Vec<u8>,
    /// For each i, the length of the longest prefix of `pat` that also
    /// ends `pat[..=i]` (shorter than it): what is still matched when the
    /// byte after does not match.
    back: Vec<usize>,
    /// How much of `pat` the bytes so far end with.
    got: usize,
}

impl Seek {
    fn new(pat: Vec<u8>) -> Seek {
        let mut back = vec![0; pat.len()];
        let mut k = 0;
        for i in 1..pat.len() {
            while k > 0 && pat[i] != pat[k] {
                k = back[k - 1];
            }
            if pat[i] == pat[k] {
                k += 1;
            }
            back[i] = k;
        }
        Seek { pat, back, got: 0 }
    }

    /// Where in `b` the pattern ends, if it does (the search then starts
    /// over).
    fn find(&mut self, b: &[u8]) -> Option<usize> {
        let mut i = 0;
        while i < b.len() {
            if self.got == 0 {
                // Nothing begun: on to the pattern's first byte.
                i += b[i..].iter().position(|&c| c == self.pat[0])?;
            }
            let c = b[i];
            while self.got > 0 && self.pat[self.got] != c {
                self.got = self.back[self.got - 1];
            }
            if self.pat[self.got] == c {
                self.got += 1;
            }
            i += 1;
            if self.got == self.pat.len() {
                self.got = 0;
                return Some(i);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(ct: Option<&str>, body: &'static [u8]) -> Request {
        let mut b = http::Request::post("/");
        if let Some(ct) = ct {
            b = b.header(http::header::CONTENT_TYPE, ct);
        }
        b.body(crate::body::full(body)).unwrap()
    }

    #[tokio::test]
    async fn parts_in_order_and_refusals_as_before() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"f.bin\"\r\n\r\n0123456789\r\n--b--\r\n";
        let mut f = Form::of(req(Some("multipart/form-data; boundary=b"), body), 1 << 20).unwrap();
        let a = f.next_field().await.unwrap().unwrap();
        assert_eq!(a.name(), Some("a"));
        assert_eq!(a.capped(2).await.unwrap(), b"xy");
        let mut file = f.next_field().await.unwrap().unwrap();
        assert_eq!(file.file_name(), Some("f.bin"));
        let mut got = Vec::new();
        while let Some(c) = file.next().await {
            got.extend_from_slice(&c.unwrap());
        }
        assert_eq!(got, b"0123456789");
        // A part is let go of before the next is asked for (holding it
        // does not compile).
        drop(file);
        assert!(f.next_field().await.unwrap().is_none());

        for ct in [None, Some("text/plain"), Some("multipart/form-data")] {
            assert_eq!(Form::of(req(ct, b""), 10).err(), Some(FormError::NotAForm));
        }
        let mut f = Form::of(req(Some("multipart/form-data; boundary=b"), body), 1 << 20).unwrap();
        let a = f.next_field().await.unwrap().unwrap();
        assert_eq!(
            a.capped(1).await.unwrap_err(),
            "the form's field \"a\" is larger than 1 bytes"
        );
        // The server says these too, of the forms it reads with axum's
        // Multipart.
        assert_eq!(
            field_too_large("file", 7u64),
            "the form's field \"file\" is larger than 7 bytes"
        );
        assert_eq!(
            reading_failed(FormError::Unreadable),
            "reading the upload: Error parsing `multipart/form-data` request"
        );
        // Over the form's limit: unreadable, as a body cut short is.
        let mut f = Form::of(req(Some("multipart/form-data; boundary=b"), body), 40).unwrap();
        let mut err = None;
        loop {
            match f.next_field().await {
                Ok(Some(mut p)) => {
                    while let Some(c) = p.next().await {
                        if let Err(e) = c {
                            err = Some(e);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert_eq!(err.map(|e| e.to_string()).as_deref(), Some(UNREADABLE));
    }

    #[tokio::test]
    async fn a_part_s_headers() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"c.tgz\"\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\neHk=\r\n--b--\r\n";
        let mut f = Form::of(req(Some("multipart/form-data; boundary=b"), body), 1 << 20).unwrap();
        let p = f.next_field().await.unwrap().unwrap();
        let h = p.headers();
        assert_eq!(h.len(), 3);
        assert_eq!(h["content-transfer-encoding"], "base64");
        assert_eq!(h["content-type"], "application/octet-stream");
        assert_eq!(
            crate::body::to_bytes(p.into_body(), 10).await.unwrap(),
            "eHk="
        );
    }

    #[tokio::test]
    async fn a_part_s_body_held_holds_back_the_next() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n--b\r\nContent-Disposition: form-data; name=\"c\"\r\n\r\nz\r\n--b--\r\n";
        let mut f = Form::of(req(Some("multipart/form-data; boundary=b"), body), 1 << 20).unwrap();
        let held = f.next_field().await.unwrap().unwrap().into_body();
        assert_eq!(f.next_field().await.err(), Some(FormError::Held));
        assert_eq!(
            FormError::Held.to_string(),
            "the form's part before is still being read"
        );
        // Let go of, the form goes on.
        assert_eq!(crate::body::to_bytes(held, 10).await.unwrap(), "xy");
        let c = f.next_field().await.unwrap().unwrap();
        assert_eq!(c.name(), Some("c"));
        assert_eq!(c.capped(1).await.unwrap(), b"z");
        assert!(f.next_field().await.unwrap().is_none());
    }

    // ---- the framing: what multer would hold whole is bounded ----

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::body::Body;

    /// curl's kind of boundary: dashes that overlap themselves.
    const CURL: &str = "------------------------d74496d66958873e";
    /// Chunks of the long bodies below.
    const CHUNK: usize = 1 << 10;

    /// A form of `parts` (name, its other header lines, its bytes).
    fn form(boundary: &str, parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, more, data) in parts {
            let head = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n{more}\r\n"
            );
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    /// Forms multer reads (with their boundaries), framed every way it
    /// takes: a preamble and an epilogue, padding after delimiters, empty
    /// parts, no line end after the last delimiter, bytes in parts that
    /// begin a delimiter or a blank line and do not end one.
    fn valid() -> Vec<(&'static str, Vec<u8>)> {
        let simple = form(
            "b",
            &[
                ("a", "", b"xy"),
                (
                    "f",
                    "Content-Type: application/octet-stream\r\n",
                    b"0123456789",
                ),
            ],
        );
        let dashes = [
            &b"\r\n"[..],
            &[b'-'; 30],
            b"x\r\n--",
            &CURL.as_bytes()[..10],
            b"\r\n\r\nx--",
            CURL.as_bytes(),
            b"\r\n-",
        ]
        .concat();
        let mut around = b"a preamble: --x, -b and \r\n\r\n\r\n".to_vec();
        around.extend_from_slice(&simple);
        around.extend_from_slice(b"after: --b\r\nnot a part\r\n\r\n--b--\r\n");
        // Dashes that run on into the first delimiter.
        let run_on = [&b"x-----"[..], &form(CURL, &[("a", "", b"xy")])].concat();
        let padded = b"--b \t \r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n--b\t\r\nContent-Disposition: form-data; name=\"c\"\r\n\r\n\r\n--b--".to_vec();
        vec![
            ("b", simple),
            (CURL, form(CURL, &[("a", "", &dashes), ("e", "", b"")])),
            (CURL, run_on),
            ("b", around),
            ("b", padded),
            (
                "b",
                form("b", &[("a", "", b"\r\r\n\r\n\r\r\n--x\r\n-\r\n--")]),
            ),
        ]
    }

    /// Forms multer refuses, and whether their framing is refused
    /// already (or only their headers or their end are wrong).
    fn invalid() -> Vec<(&'static [u8], bool)> {
        vec![
            (b"--b\r\nnot a header\r\n\r\nx\r\n--b--\r\n", false),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy",
                false,
            ),
            (b"no boundary here", false),
            (b"--bx\r\n\r\n", true),
            (b"--b-x", true),
            (b"--b\rx", true),
            (b"--b \t x\r\n", true),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n--bz\r\n",
                true,
            ),
        ]
    }

    /// How many parts multer alone reads in `body`, if it reads all of
    /// it.
    async fn multer_reads(boundary: &str, body: &[u8]) -> Option<usize> {
        let body = Bytes::copy_from_slice(body);
        let data = futures_util::stream::once(async { Ok::<_, std::convert::Infallible>(body) });
        let mut m = multer::Multipart::new(data, boundary);
        let mut n = 0;
        loop {
            match m.next_field().await {
                Ok(Some(mut f)) => {
                    n += 1;
                    while f.chunk().await.ok()?.is_some() {}
                }
                Ok(None) => return Some(n),
                Err(_) => return None,
            }
        }
    }

    /// How many parts' headers the framing of `body` finds, fed a byte at
    /// a time.
    fn headers_found(boundary: &str, body: &[u8]) -> usize {
        let mut f = Framing::new(boundary);
        let mut n = 0;
        for c in body.chunks(1) {
            let before = f.at;
            let _ = f.feed(c);
            n += usize::from(f.at == At::Headers && before != At::Headers);
        }
        n
    }

    /// Whether the framing of `body`, fed in `chunks`, is refused, and
    /// where it is at the end.
    fn framed<'a>(boundary: &str, chunks: impl Iterator<Item = &'a [u8]>) -> (bool, At) {
        let mut f = Framing::new(boundary);
        let mut refused = false;
        for c in chunks {
            refused |= f.feed(c).is_err();
        }
        (refused, f.at)
    }

    /// A search finds where its pattern first ends however the bytes
    /// are cut, as a search of them whole does, a pattern that overlaps
    /// itself too.
    #[test]
    fn seek_finds_the_first_end_in_any_chunks() {
        let curl = format!("--{CURL}");
        for pat in [curl.as_bytes(), b"\r\n\r\n", b"abab", b"aab"] {
            // Its beginnings run into each other and into it.
            let mut hays = Vec::new();
            for i in 0..pat.len() {
                for j in [0, 1, pat.len() / 2, pat.len() - 1] {
                    hays.push([&pat[..i], &pat[..j], pat, b"x"].concat());
                    hays.push([&pat[..i], &pat[j..], &pat[..j], b"x"].concat());
                }
            }
            for hay in hays {
                let first = hay
                    .windows(pat.len())
                    .position(|w| w == pat)
                    .map(|i| i + pat.len());
                assert_eq!(Seek::new(pat.to_vec()).find(&hay), first, "{hay:?}");
                for cut in 0..=hay.len() {
                    let (a, b) = hay.split_at(cut);
                    let mut s = Seek::new(pat.to_vec());
                    let got = s.find(a).or_else(|| s.find(b).map(|end| cut + end));
                    assert_eq!(got, first, "{hay:?} cut at {cut}");
                }
                let mut s = Seek::new(pat.to_vec());
                let got = (0..hay.len()).find(|&i| s.find(&hay[i..=i]).is_some());
                assert_eq!(got.map(|i| i + 1), first, "{hay:?} a byte at a time");
            }
        }
    }

    /// The framing of a form is read alike however its bytes are cut
    /// into chunks; it never refuses what multer reads, and refuses only
    /// what multer does.
    #[tokio::test]
    async fn framing_follows_multer_in_any_chunks() {
        let mut all: Vec<(&str, Vec<u8>, Option<bool>)> = Vec::new();
        for (b, body) in valid() {
            all.push((b, body, None));
        }
        for (body, refused) in invalid() {
            all.push(("b", body.to_vec(), Some(refused)));
        }
        for (boundary, body, refused) in all {
            let whole = framed(boundary, std::iter::once(&body[..]));
            let reads = multer_reads(boundary, &body).await;
            match refused {
                None => {
                    assert!(reads.is_some_and(|n| n > 0), "{body:?}");
                    assert_eq!(whole, (false, At::Closed), "{body:?}");
                    // Each part where multer finds it.
                    assert_eq!(Some(headers_found(boundary, &body)), reads, "{body:?}");
                }
                Some(refused) => {
                    assert_eq!(reads, None, "{body:?}");
                    assert_eq!(whole.0, refused, "{body:?}");
                }
            }
            assert_eq!(framed(boundary, body.chunks(1)), whole, "{body:?}");
            for cut in 0..=body.len() {
                let (a, b) = body.split_at(cut);
                assert_eq!(
                    framed(boundary, [a, b].into_iter()),
                    whole,
                    "{body:?} at {cut}"
                );
            }
        }
    }

    /// A body of `chunks`, each after a yield, so that multer reads them
    /// as they come rather than all at once; `pulled` counts the bytes
    /// taken of it.
    fn trickle<I>(chunks: I, pulled: Arc<AtomicUsize>) -> Body
    where
        I: Iterator<Item = Vec<u8>> + Send + 'static,
    {
        let s = futures_util::stream::unfold((chunks, pulled), |(mut it, pulled)| async move {
            tokio::task::yield_now().await;
            let c = it.next()?;
            pulled.fetch_add(c.len(), Ordering::Relaxed);
            Some((Ok::<_, BoxError>(c), (it, pulled)))
        });
        crate::body::from_stream(s)
    }

    /// `body` in chunks of `n` bytes.
    fn cut(body: &[u8], n: usize) -> Body {
        let chunks: Vec<Vec<u8>> = body.chunks(n).map(<[u8]>::to_vec).collect();
        trickle(chunks.into_iter(), Arc::default())
    }

    fn post(boundary: &str, body: Body) -> Request {
        http::Request::post("/")
            .header(
                http::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .unwrap()
    }

    /// The parts of a form (name, bytes), read as the formats read them.
    async fn parts(boundary: &str, body: Body) -> Result<Vec<(String, Vec<u8>)>, FormError> {
        let mut f = Form::of(post(boundary, body), 1 << 30)?;
        let mut out = Vec::new();
        while let Some(mut p) = f.next_field().await? {
            let name = p.name().unwrap_or("").to_string();
            let mut data = Vec::new();
            while let Some(c) = p.next().await {
                data.extend_from_slice(&c?);
            }
            out.push((name, data));
        }
        Ok(out)
    }

    #[tokio::test]
    async fn forms_read_alike_in_any_chunks() {
        for (boundary, body) in valid() {
            let whole = parts(boundary, crate::body::full(body.clone()))
                .await
                .unwrap();
            assert!(!whole.is_empty());
            // A byte at a time, as multer sees them too.
            assert_eq!(parts(boundary, cut(&body, 1)).await.unwrap(), whole);
            // Every delimiter and blank line cut across two chunks.
            for at in 1..body.len() {
                let (a, b) = body.split_at(at);
                let two = trickle([a.to_vec(), b.to_vec()].into_iter(), Arc::default());
                assert_eq!(parts(boundary, two).await.unwrap(), whole, "at {at}");
            }
        }
    }

    /// A body of `head`, then `fill` bytes up to `len` in all, in chunks
    /// of CHUNK; `pulled` counts the bytes taken of it.
    fn long(head: &[u8], fill: u8, len: usize, pulled: Arc<AtomicUsize>) -> Body {
        let rest = std::iter::repeat_n(vec![fill; CHUNK], (len - head.len()) / CHUNK);
        trickle(std::iter::once(head.to_vec()).chain(rest), pulled)
    }

    /// A body with no delimiter, or a part whose headers do not end, is
    /// refused once MAX_HEAD of it is read, not held whole. (The form's
    /// limit, 4 MiB here, is far above that.)
    #[tokio::test]
    async fn no_delimiter_or_headers_without_end_refused_early() {
        let part = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n";
        let cases: [(&[u8], u8, usize, usize); 4] = [
            // 1 MiB with no boundary.
            (b"", b'x', 1 << 20, 0),
            // A header line that goes on (for a GiB).
            (
                b"--b\r\nContent-Disposition: form-data; name=\"a\"; x=\"",
                b'a',
                1 << 30,
                0,
            ),
            // Headers looked for after the delimiter's line end, not at it.
            (b"--b\r\n\r\n", b'a', 1 << 30, 0),
            // The second part's.
            (&[&part[..], b"--b\r\nX-A: "].concat(), b'a', 1 << 30, 1),
        ];
        for (head, fill, len, before) in cases {
            let pulled = Arc::new(AtomicUsize::new(0));
            let body = long(head, fill, len, pulled.clone());
            let mut f = Form::of(post("b", body), 4 << 20).unwrap();
            for _ in 0..before {
                let p = f.next_field().await.unwrap().unwrap();
                assert_eq!(p.capped(2).await.unwrap(), b"xy");
            }
            assert_eq!(f.next_field().await.err(), Some(FormError::Unreadable));
            let n = pulled.load(Ordering::Relaxed);
            assert!(n <= head.len() + MAX_HEAD + CHUNK, "{n} of {len} read");
        }
    }

    /// A form found unreadable is so when asked again, without reading
    /// on: multer, asked again, would look for a part's headers in what
    /// follows, and hold it all.
    #[tokio::test]
    async fn an_unreadable_form_stays_so() {
        let heads: [&[u8]; 2] = [
            // Headers multer cannot read, then no blank line.
            b"--b\r\nnot a header\r\n\r\n",
            // A delimiter followed by neither "--" nor a line end; then
            // a line end and no blank line.
            b"--bxx\r\nX-A: ",
        ];
        for head in heads {
            let pulled = Arc::new(AtomicUsize::new(0));
            let body = long(head, b'a', 1 << 30, pulled.clone());
            let mut f = Form::of(post("b", body), 4 << 20).unwrap();
            for _ in 0..3 {
                assert_eq!(f.next_field().await.err(), Some(FormError::Unreadable));
            }
            let n = pulled.load(Ordering::Relaxed);
            assert!(n <= head.len() + CHUNK, "{n} read after {head:?}");
        }
    }

    /// The preamble and a part's headers may be MAX_HEAD bytes, not one
    /// more; a part's bytes are not bounded by it.
    #[tokio::test]
    async fn the_edges_of_max_head() {
        let rest = b"\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxy\r\n--b--\r\n";
        let head = |pad: usize| {
            let mut h = b"Content-Disposition: form-data; name=\"a\"\r\nX-Pad: ".to_vec();
            let fill = MAX_HEAD - h.len() - 4 + pad;
            h.extend(std::iter::repeat_n(b'p', fill));
            h.extend_from_slice(b"\r\n\r\n");
            h
        };
        let big = vec![b'd'; 1 << 20];
        let forms = [
            // The first delimiter ends MAX_HEAD bytes in.
            ([&vec![b'x'; MAX_HEAD - 3][..], b"--b", rest].concat(), true),
            (
                [&vec![b'x'; MAX_HEAD - 2][..], b"--b", rest].concat(),
                false,
            ),
            // The header block with its blank line is MAX_HEAD bytes.
            (
                [&b"--b\r\n"[..], &head(0), b"xy\r\n--b--\r\n"].concat(),
                true,
            ),
            (
                [&b"--b\r\n"[..], &head(1), b"xy\r\n--b--\r\n"].concat(),
                false,
            ),
            // A part of 1 MiB.
            (form("b", &[("a", "", &big)]), true),
        ];
        for (body, read) in forms {
            for n in [body.len(), CHUNK, 7] {
                let got = parts("b", cut(&body, n)).await;
                if read {
                    let got = got.unwrap();
                    assert_eq!(got.len(), 1);
                    assert!(got[0].1 == b"xy" || got[0].1 == big);
                } else {
                    assert_eq!(got.err(), Some(FormError::Unreadable), "in chunks of {n}");
                }
            }
        }
    }

    /// The form's limit crossed in a file part stored as it comes: the
    /// part's body ends in the unreadable form's error, in the words the
    /// server stores a body with.
    #[tokio::test]
    async fn the_form_s_limit_inside_a_file_part() {
        let file = |n: usize| form("b", &[("content", "", &vec![b'f'; n])]);
        let limit = file(10 << 10).len();
        for (body, whole) in [(file(10 << 10), true), (file((10 << 10) + 1), false)] {
            assert_eq!(body.len(), limit + usize::from(!whole));
            let mut f = Form::of(post("b", cut(&body, CHUNK)), limit as u64).unwrap();
            let p = f.next_field().await.unwrap().unwrap();
            let mut stored = p.into_body().into_data_stream();
            let (mut n, mut err) = (0, None);
            while let Some(c) = stored.next().await {
                match c {
                    Ok(c) => n += c.len(),
                    Err(e) => {
                        err = Some(reading_failed(e));
                        break;
                    }
                }
            }
            drop(stored);
            if whole {
                assert_eq!((n, err), (10 << 10, None));
                assert!(f.next_field().await.unwrap().is_none());
            } else {
                assert!(n < 10 << 10);
                assert_eq!(
                    err.as_deref(),
                    Some("reading the upload: Error parsing `multipart/form-data` request")
                );
            }
        }
    }
}
