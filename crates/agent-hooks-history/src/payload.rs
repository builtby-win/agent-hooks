//! Transparent lossless storage codec for captured text.
//!
//! A payload column is one of three SQLite values:
//! - `NULL`: absent;
//! - `TEXT`: the exact UTF-8 string, stored uncompressed (short or
//!   uncompressible payloads);
//! - `BLOB`: one standard zstd frame holding the exact UTF-8 bytes, with its
//!   decoded length and a content checksum embedded in the frame.
//!
//! Nothing here trims, caps, or truncates: an encoded payload always decodes
//! back to the identical string, or decoding fails. Errors carry fixed
//! messages and never echo stored content.

use std::fmt;
use std::io::Read;

use rusqlite::types::{ToSqlOutput, Type, ValueRef};
use rusqlite::ToSql;
use zstd::bulk::Compressor;
use zstd::stream::read::Decoder;
use zstd::zstd_safe;

use crate::HistoryError;

/// Payloads shorter than this many UTF-8 bytes are always stored as `TEXT`.
const COMPRESS_MIN_BYTES: usize = 1024;
const ZSTD_LEVEL: i32 = 1;
/// Upper bound on the up-front buffer reservation while decoding, so a
/// corrupt frame that claims an enormous decoded length cannot force a huge
/// allocation. Larger legitimate payloads simply grow the buffer.
const PREALLOC_LIMIT: usize = 16 << 20;

/// A payload column value ready to bind as a statement parameter.
#[derive(Debug)]
pub(crate) enum EncodedPayload<'a> {
    Absent,
    /// Borrowed original text, stored as SQLite `TEXT`.
    Raw(&'a str),
    /// Owned zstd frame, stored as SQLite `BLOB`.
    Compressed(Vec<u8>),
}

impl ToSql for EncodedPayload<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Self::Absent => ValueRef::Null,
            Self::Raw(text) => ValueRef::Text(text.as_bytes()),
            Self::Compressed(frame) => ValueRef::Blob(frame),
        }))
    }
}

/// Encode an optional payload for storage. Text of at least
/// [`COMPRESS_MIN_BYTES`] bytes is compressed and kept compressed only when
/// the frame is strictly smaller than the text.
pub(crate) fn encode(text: Option<&str>) -> Result<EncodedPayload<'_>, HistoryError> {
    let Some(text) = text else {
        return Ok(EncodedPayload::Absent);
    };
    if text.len() < COMPRESS_MIN_BYTES {
        return Ok(EncodedPayload::Raw(text));
    }
    let frame = compress(text.as_bytes())?;
    Ok(choose(text, frame))
}

fn compress(bytes: &[u8]) -> Result<Vec<u8>, HistoryError> {
    let fail = |_| HistoryError::Payload("compression failed");
    let mut compressor = Compressor::new(ZSTD_LEVEL).map_err(fail)?;
    compressor.include_contentsize(true).map_err(fail)?;
    compressor.include_checksum(true).map_err(fail)?;
    compressor.compress(bytes).map_err(fail)
}

fn choose(text: &str, frame: Vec<u8>) -> EncodedPayload<'_> {
    if frame.len() < text.len() {
        EncodedPayload::Compressed(frame)
    } else {
        EncodedPayload::Raw(text)
    }
}

/// Fixed-message corruption marker; never carries stored content.
#[derive(Debug)]
struct PayloadCorrupt(&'static str);

impl fmt::Display for PayloadCorrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "history payload is corrupt: {}", self.0)
    }
}

impl std::error::Error for PayloadCorrupt {}

fn corrupt(ty: Type, reason: &'static str) -> rusqlite::Error {
    // The column index is not known here; callers know which column failed.
    rusqlite::Error::FromSqlConversionFailure(0, ty, Box::new(PayloadCorrupt(reason)))
}

/// Decode a payload column value written by [`encode`].
pub(crate) fn decode(value: ValueRef<'_>) -> rusqlite::Result<Option<String>> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(|text| Some(text.to_owned()))
            .map_err(|_| corrupt(Type::Text, "stored text is not valid UTF-8")),
        ValueRef::Blob(frame) => decompress(frame).map(Some),
        ValueRef::Integer(_) => Err(corrupt(Type::Integer, "unexpected integer value")),
        ValueRef::Real(_) => Err(corrupt(Type::Real, "unexpected real value")),
    }
}

fn decompress(frame: &[u8]) -> rusqlite::Result<String> {
    let bad = |reason| corrupt(Type::Blob, reason);
    let declared = match zstd_safe::get_frame_content_size(frame) {
        Ok(Some(n)) => n,
        _ => return Err(bad("compressed frame has no valid decoded length")),
    };
    match zstd_safe::find_frame_compressed_size(frame) {
        Ok(len) if len == frame.len() => {}
        _ => return Err(bad("compressed frame is truncated or has trailing data")),
    }
    let declared_len =
        usize::try_from(declared).map_err(|_| bad("decoded length is not addressable"))?;

    let decoder = Decoder::with_buffer(frame)
        .map_err(|_| bad("compressed frame is invalid"))?
        .single_frame();
    let mut out = Vec::with_capacity(declared_len.min(PREALLOC_LIMIT));
    // One byte past the declared length detects a frame that decodes to more
    // than it declares.
    decoder
        .take(declared.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|_| bad("compressed frame failed to decode"))?;
    if out.len() != declared_len {
        return Err(bad("decoded length does not match the frame header"));
    }
    String::from_utf8(out).map_err(|_| bad("decoded payload is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    const SECRET: &str = "SECRET-CAPTURED-TEXT-0451";

    fn table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v BLOB)").unwrap();
        conn
    }

    /// Store `payload`, returning (sqlite storage type, decoded value).
    fn roundtrip(payload: &EncodedPayload<'_>) -> (String, rusqlite::Result<Option<String>>) {
        let conn = table();
        conn.execute("INSERT INTO t(v) VALUES (?1)", [payload])
            .unwrap();
        let ty: String = conn
            .query_row("SELECT typeof(v) FROM t", [], |r| r.get(0))
            .unwrap();
        let decoded = conn.query_row("SELECT v FROM t", [], |r| decode(r.get_ref(0)?));
        (ty, decoded)
    }

    fn stored(text: Option<&str>) -> (String, Option<String>) {
        let (ty, decoded) = roundtrip(&encode(text).unwrap());
        (ty, decoded.unwrap())
    }

    fn compressed_blob(text: &str) -> Vec<u8> {
        match encode(Some(text)).unwrap() {
            EncodedPayload::Compressed(frame) => frame,
            other => panic!("expected compressed payload, got {other:?}"),
        }
    }

    fn decode_blob(blob: &[u8]) -> rusqlite::Result<Option<String>> {
        decode(ValueRef::Blob(blob))
    }

    fn assert_sanitized(err: &rusqlite::Error) {
        assert!(
            matches!(err, rusqlite::Error::FromSqlConversionFailure(..)),
            "unexpected error type"
        );
        for rendered in [err.to_string(), format!("{err:?}")] {
            assert!(!rendered.contains(SECRET), "error leaked content");
        }
    }

    fn big_unicode() -> String {
        format!("{SECRET} héllo 世界 🚀 \u{0} \"quoted\" \\ \n\t").repeat(2000)
    }

    #[test]
    fn absent_and_short_payloads_roundtrip_as_exact_text() {
        assert_eq!(stored(None), ("null".into(), None));
        for text in ["", "x", "héllo 世界 🚀", "nul\0inside", &"a".repeat(1023)] {
            assert_eq!(stored(Some(text)), ("text".into(), Some(text.to_owned())));
        }
    }

    #[test]
    fn large_unicode_phase_text_compresses_and_roundtrips() {
        let text = big_unicode();
        let frame = compressed_blob(&text);
        assert!(frame.len() < text.len());
        let (ty, back) = stored(Some(&text));
        assert_eq!(ty, "blob");
        assert_eq!(back.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn raw_payload_borrows_original_bytes() {
        let text = String::from("short");
        let EncodedPayload::Raw(raw) = encode(Some(&text)).unwrap() else {
            panic!("expected raw payload");
        };
        assert!(std::ptr::eq(raw.as_ptr(), text.as_ptr()));
    }

    #[test]
    fn truncated_frames_error_without_leaking() {
        let frame = compressed_blob(&big_unicode());
        for cut in [0, 1, 4, frame.len() / 2, frame.len() - 1] {
            let err = decode_blob(&frame[..cut]).unwrap_err();
            assert_sanitized(&err);
        }
    }

    #[test]
    fn corrupt_frames_error_without_leaking() {
        let frame = compressed_blob(&big_unicode());

        let mut bad_magic = frame.clone();
        bad_magic[0] ^= 0xff;
        assert_sanitized(&decode_blob(&bad_magic).unwrap_err());

        // The last four bytes are the content checksum.
        let mut bad_checksum = frame.clone();
        *bad_checksum.last_mut().unwrap() ^= 0x01;
        assert_sanitized(&decode_blob(&bad_checksum).unwrap_err());

        let mut trailing = frame.clone();
        trailing.extend_from_slice(b"junk");
        assert_sanitized(&decode_blob(&trailing).unwrap_err());

        let mut second_frame = frame.clone();
        second_frame.extend_from_slice(&frame);
        assert_sanitized(&decode_blob(&second_frame).unwrap_err());

        assert_sanitized(&decode_blob(SECRET.as_bytes()).unwrap_err());
        assert_sanitized(&decode_blob(&[]).unwrap_err());
    }

    #[test]
    fn frame_without_declared_length_is_rejected() {
        let mut compressor = Compressor::new(ZSTD_LEVEL).unwrap();
        compressor.include_contentsize(false).unwrap();
        let frame = compressor.compress(big_unicode().as_bytes()).unwrap();
        assert_sanitized(&decode_blob(&frame).unwrap_err());
    }

    #[test]
    fn invalid_utf8_is_an_error_not_lossy() {
        let mut bytes = SECRET.as_bytes().repeat(100);
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        let frame = zstd::bulk::compress(&bytes, ZSTD_LEVEL).unwrap();
        assert_sanitized(&decode_blob(&frame).unwrap_err());

        let mut text = SECRET.as_bytes().to_vec();
        text.push(0xff);
        assert_sanitized(&decode(ValueRef::Text(&text)).unwrap_err());
    }

    #[test]
    fn invalid_utf8_text_cell_in_sqlite_is_an_error() {
        let conn = table();
        conn.execute("INSERT INTO t(v) VALUES (CAST(x'c328' AS TEXT))", [])
            .unwrap();
        let err = conn
            .query_row("SELECT v FROM t", [], |r| decode(r.get_ref(0)?))
            .unwrap_err();
        assert_sanitized(&err);
    }

    #[test]
    fn non_text_value_types_are_rejected() {
        assert_sanitized(&decode(ValueRef::Integer(7)).unwrap_err());
        assert_sanitized(&decode(ValueRef::Real(1.5)).unwrap_err());
    }
}
