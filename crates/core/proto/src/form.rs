// SPDX-License-Identifier: AGPL-3.0-only

//! Upload forms (`multipart/form-data`) as the formats' protocols read
//! them: the parts in order, a text field read whole up to a size, a
//! file as a stream of its bytes. A request that is not a form, or a
//! form that cannot be read, is told in the words clients were given
//! before (axum's).

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

/// Why a form is not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormError {
    NotAForm,
    Unreadable,
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FormError::NotAForm => NOT_A_FORM,
            FormError::Unreadable => UNREADABLE,
        })
    }
}

impl std::error::Error for FormError {}

/// A request's form.
pub struct Form(multer::Multipart<'static>);

/// A part of a form.
pub struct Field(multer::Field<'static>);

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

    /// The next part, if there is one; the one before is let go of
    /// first (dropped), else the form is unreadable.
    pub async fn next_field(&mut self) -> Result<Option<Field>, FormError> {
        match self.0.next_field().await {
            Ok(f) => Ok(f.map(Field)),
            Err(_) => Err(FormError::Unreadable),
        }
    }
}

impl Field {
    pub fn name(&self) -> Option<&str> {
        self.0.name()
    }

    pub fn file_name(&self) -> Option<&str> {
        self.0.file_name()
    }

    /// The part read whole, refused above `max` bytes (a part is
    /// otherwise bounded only by the form's limit, gigabytes).
    pub async fn capped(mut self, max: usize) -> Result<Vec<u8>, String> {
        let name = self.name().unwrap_or("").to_string();
        let mut out = Vec::new();
        while let Some(chunk) = self
            .0
            .chunk()
            .await
            .map_err(|_| format!("reading the upload: {}", FormError::Unreadable))?
        {
            if out.len() + chunk.len() > max {
                return Err(format!(
                    "the form's field {name:?} is larger than {max} bytes"
                ));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

impl Stream for Field {
    type Item = Result<Bytes, FormError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|c| c.map(|c| c.map_err(|_| FormError::Unreadable)))
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
        // A part is let go of before the next is asked for.
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
}
