// SPDX-License-Identifier: AGPL-3.0-only

//! Upload forms (`multipart/form-data`) as the formats' protocols read
//! them: the parts in order, a text field read whole up to a size, a
//! file as a stream of its bytes. A request that is not a form, or a
//! form that cannot be read, is told in the words clients were given
//! before (axum's).

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
pub struct Form(multer::Multipart<'static>);

/// A part of a form, borrowing it: the form gives its next part only
/// once this one is let go of.
pub struct Field<'a> {
    part: multer::Field<'static>,
    form: PhantomData<&'a mut Form>,
}

impl Form {
    /// The form `req` carries, read up to `limit` bytes in all.
    pub fn of(req: Request, limit: u64) -> Result<Form, FormError> {
        let boundary = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| multer::parse_boundary(ct).ok())
            .ok_or(FormError::NotAForm)?;
        let mut seen = 0u64;
        let data = req.into_body().into_data_stream().map(move |chunk| {
            let chunk = chunk?;
            seen += chunk.len() as u64;
            if seen > limit {
                return Err::<Bytes, BoxError>("length limit exceeded".into());
            }
            Ok(chunk)
        });
        Ok(Form(multer::Multipart::new(data, boundary)))
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
    /// it is held, the next part is [`FormError::Held`].
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>, FormError> {
        let part = self.0.next_field().await.map_err(why)?;
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
}
