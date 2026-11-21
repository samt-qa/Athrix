//! Wire format between [`WorkerPool`](crate::WorkerPool) and the
//! `athrix-extract-worker` process.
//!
//! Every message is a *frame*: a little-endian `u32` payload length, then
//! the payload. The first payload byte is the message tag; integers are
//! little-endian; a string or byte string is a `u32` length and its bytes
//! (strings are UTF-8); an optional string is a `0`/`1` byte and, for `1`,
//! the string. A frame longer than the receiver's bound, a truncated frame,
//! an unknown tag, a bad field or trailing bytes are protocol errors: the
//! worker answers [`Response::ProtocolError`] and exits, the client kills
//! the worker.
//!
//! Version 1:
//!
//! | Direction | Tag | Message | Fields |
//! |---|---|---|---|
//! | worker → client | `0x80` | Hello (first frame) | magic `AXWK`, version `u16`, flags `u32` (bit 0: PDF compiled in), pid `u32` |
//! | client → worker | `0x01` | ExtractPath | id `u64`, limits, path |
//! | client → worker | `0x02` | ExtractBytes | id `u64`, limits, kind `u8`, bytes |
//! | client → worker | `0x03` | Shutdown | — |
//! | client → worker | `0x70` | Debug (test hooks only) | id `u64`, hook `u8`, bytes `u64`, keep `u8` |
//! | worker → client | `0x81` | Extracted | id, heap `u64`, kind, truncated `u8`, encoding?, title?, author?, created?, text |
//! | worker → client | `0x82` | Failed | id, heap `u64`, error code `u8`, io kind `u8`, message |
//! | worker → client | `0x83` | Done (debug hook) | id, heap `u64` |
//! | worker → client | `0x8F` | ProtocolError | message |
//!
//! Limits are six `u64`s in [`Limits`] field order. A path is a tag byte
//! (`1`: UTF-16 code units as `u16`s, Windows; `2`: raw bytes, Unix; `3`:
//! UTF-8, other platforms), a `u32` count and the units. `heap` is the
//! worker's heap in use after answering, used to recycle a worker that
//! grows.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::{ExtractError, Extracted, Kind, Limits, Metadata};

/// Protocol version spoken by this build.
pub(crate) const VERSION: u16 = 1;
/// Magic bytes at the start of the Hello payload.
pub(crate) const MAGIC: &[u8; 4] = b"AXWK";
/// Hello flag: the worker was built with the `pdf` feature.
pub(crate) const FLAG_PDF: u32 = 1;
/// Largest request frame a worker accepts.
pub(crate) const MAX_REQUEST_FRAME: u32 = 1 << 30;

const TAG_EXTRACT_PATH: u8 = 0x01;
const TAG_EXTRACT_BYTES: u8 = 0x02;
const TAG_SHUTDOWN: u8 = 0x03;
const TAG_DEBUG: u8 = 0x70;
const TAG_HELLO: u8 = 0x80;
const TAG_EXTRACTED: u8 = 0x81;
const TAG_FAILED: u8 = 0x82;
const TAG_DONE: u8 = 0x83;
const TAG_PROTOCOL_ERROR: u8 = 0x8F;

const PATH_WIDE: u8 = 1;
const PATH_BYTES: u8 = 2;
const PATH_UTF8: u8 = 3;

/// Offset of the `heap` field in Extracted / Failed / Done frames (length
/// prefix, tag, id).
const HEAP_OFFSET: usize = 4 + 1 + 8;

/// Test-only requests that make the worker misbehave, to exercise the
/// client's recovery paths. The worker accepts them only when started with
/// test hooks enabled.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugHook {
    /// Never answer (sleep forever).
    Hang,
    /// Recurse until the stack overflows.
    Overflow,
    /// Panic outside the extraction guard, killing the worker.
    Panic,
    /// Allocate (and touch) `bytes` bytes; keep them allocated if `keep`.
    Alloc {
        /// Bytes to allocate.
        bytes: u64,
        /// Leak the allocation, to simulate a worker that grows.
        keep: bool,
    },
}

/// A request from the client.
#[derive(Debug)]
pub(crate) enum Request {
    ExtractPath {
        id: u64,
        limits: Limits,
        path: PathBuf,
    },
    ExtractBytes {
        id: u64,
        limits: Limits,
        kind: Kind,
        bytes: Vec<u8>,
    },
    Shutdown,
    Debug {
        id: u64,
        hook: DebugHook,
    },
}

/// A response from the worker.
#[derive(Debug)]
pub(crate) enum Response {
    Hello {
        version: u16,
        flags: u32,
        pid: u32,
    },
    Extracted {
        id: u64,
        heap: u64,
        extracted: Extracted,
    },
    Failed {
        id: u64,
        heap: u64,
        error: ExtractError,
    },
    Done {
        id: u64,
        heap: u64,
    },
    ProtocolError(String),
}

impl Response {
    /// The request id this answers, if it answers one.
    pub(crate) fn id(&self) -> Option<u64> {
        match self {
            Response::Extracted { id, .. }
            | Response::Failed { id, .. }
            | Response::Done { id, .. } => Some(*id),
            Response::Hello { .. } | Response::ProtocolError(_) => None,
        }
    }

    /// Worker heap in use after answering.
    pub(crate) fn heap(&self) -> u64 {
        match self {
            Response::Extracted { heap, .. }
            | Response::Failed { heap, .. }
            | Response::Done { heap, .. } => *heap,
            Response::Hello { .. } | Response::ProtocolError(_) => 0,
        }
    }
}

/// A malformed frame or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolError(pub(crate) String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn perr(msg: impl Into<String>) -> ProtocolError {
    ProtocolError(msg.into())
}

/// Why a frame could not be read.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The stream ended inside a frame.
    Truncated,
    /// The announced length is over the bound.
    TooLong(u32),
    /// Reading failed.
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Truncated => f.write_str("truncated frame"),
            FrameError::TooLong(n) => write!(f, "frame of {n} bytes is over the limit"),
            FrameError::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

/// Reads one frame's payload. `Ok(None)` is a clean end of stream at a
/// frame boundary. The payload buffer grows as data arrives, so a bogus
/// length cannot make the reader allocate it up front.
pub(crate) fn read_frame(r: &mut impl Read, max: u32) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    let len = u32::from_le_bytes(len);
    if len > max {
        return Err(FrameError::TooLong(len));
    }
    let mut payload = Vec::with_capacity((len as usize).min(64 * 1024));
    r.take(u64::from(len))
        .read_to_end(&mut payload)
        .map_err(FrameError::Io)?;
    if payload.len() != len as usize {
        return Err(FrameError::Truncated);
    }
    Ok(Some(payload))
}

/// Builds a frame in place: a length placeholder, then fields.
struct Enc(Vec<u8>);

impl Enc {
    fn new(tag: u8) -> Enc {
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(&[0; 4]);
        v.push(tag);
        Enc(v)
    }

    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn len(&mut self, n: usize) {
        // Callers keep payloads under `u32::MAX` (frame bound checked by
        // the senders); saturate rather than wrap if one ever is not.
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }

    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
        }
    }

    fn limits(&mut self, l: &Limits) {
        self.u64(l.max_input_bytes);
        self.u64(l.max_output_chars as u64);
        self.u64(l.max_entries as u64);
        self.u64(l.max_entry_bytes);
        self.u64(l.max_total_bytes);
        self.u64(l.max_depth as u64);
    }

    fn path(&mut self, p: &std::path::Path) {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let units: Vec<u16> = p.as_os_str().encode_wide().collect();
            self.u8(PATH_WIDE);
            self.len(units.len());
            for u in units {
                self.u16(u);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.u8(PATH_BYTES);
            self.bytes(p.as_os_str().as_bytes());
        }
        #[cfg(not(any(windows, unix)))]
        {
            self.u8(PATH_UTF8);
            self.str(&p.to_string_lossy());
        }
    }

    /// The finished frame, length prefix filled in.
    fn finish(mut self) -> Vec<u8> {
        let len = u32::try_from(self.0.len() - 4).unwrap_or(u32::MAX);
        self.0[..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

/// Reads fields from a payload.
struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.0.len() < n {
            return Err(perr("payload too short"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }

    fn usize(&mut self) -> Result<usize, ProtocolError> {
        usize::try_from(self.u64()?).map_err(|_| perr("value out of range"))
    }

    fn bool(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(perr(format!("bad boolean {b}"))),
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8], ProtocolError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| perr("string is not UTF-8"))
    }

    fn opt_string(&mut self) -> Result<Option<String>, ProtocolError> {
        if self.bool()? {
            self.string().map(Some)
        } else {
            Ok(None)
        }
    }

    fn limits(&mut self) -> Result<Limits, ProtocolError> {
        Ok(Limits {
            max_input_bytes: self.u64()?,
            max_output_chars: self.usize()?,
            max_entries: self.usize()?,
            max_entry_bytes: self.u64()?,
            max_total_bytes: self.u64()?,
            max_depth: self.usize()?,
        })
    }

    fn kind(&mut self) -> Result<Kind, ProtocolError> {
        let code = self.u8()?;
        kind_from_code(code).ok_or_else(|| perr(format!("unknown kind {code}")))
    }

    fn path(&mut self) -> Result<PathBuf, ProtocolError> {
        let tag = self.u8()?;
        #[cfg(windows)]
        if tag == PATH_WIDE {
            use std::os::windows::ffi::OsStringExt;
            let n = self.u32()? as usize;
            let raw = self.take(n.checked_mul(2).ok_or_else(|| perr("path too long"))?)?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Ok(std::ffi::OsString::from_wide(&units).into());
        }
        #[cfg(unix)]
        if tag == PATH_BYTES {
            use std::os::unix::ffi::OsStringExt;
            return Ok(std::ffi::OsString::from_vec(self.bytes()?.to_vec()).into());
        }
        if tag == PATH_UTF8 {
            return Ok(self.string()?.into());
        }
        // Silence "unused" for the other platforms' tags.
        let _ = (PATH_WIDE, PATH_BYTES);
        Err(perr(format!("path encoding {tag} not supported here")))
    }

    fn end(self) -> Result<(), ProtocolError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(perr("trailing bytes after message"))
        }
    }
}

fn kind_code(kind: Kind) -> u8 {
    match kind {
        Kind::Text => 1,
        Kind::Html => 2,
        Kind::Docx => 3,
        Kind::Xlsx => 4,
        Kind::Pptx => 5,
        Kind::Odt => 6,
        Kind::Ods => 7,
        Kind::Odp => 8,
        Kind::Epub => 9,
        Kind::Eml => 10,
        Kind::Pdf => 11,
    }
}

fn kind_from_code(code: u8) -> Option<Kind> {
    Some(match code {
        1 => Kind::Text,
        2 => Kind::Html,
        3 => Kind::Docx,
        4 => Kind::Xlsx,
        5 => Kind::Pptx,
        6 => Kind::Odt,
        7 => Kind::Ods,
        8 => Kind::Odp,
        9 => Kind::Epub,
        10 => Kind::Eml,
        11 => Kind::Pdf,
        _ => return None,
    })
}

fn io_kind_code(kind: io::ErrorKind) -> u8 {
    match kind {
        io::ErrorKind::NotFound => 1,
        io::ErrorKind::PermissionDenied => 2,
        io::ErrorKind::InvalidData => 3,
        io::ErrorKind::UnexpectedEof => 4,
        io::ErrorKind::InvalidInput => 5,
        _ => 0,
    }
}

fn io_kind_from_code(code: u8) -> io::ErrorKind {
    match code {
        1 => io::ErrorKind::NotFound,
        2 => io::ErrorKind::PermissionDenied,
        3 => io::ErrorKind::InvalidData,
        4 => io::ErrorKind::UnexpectedEof,
        5 => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::Other,
    }
}

/// Encodes a request as a complete frame.
pub(crate) fn encode_request(req: &Request) -> Vec<u8> {
    match req {
        Request::ExtractPath { id, limits, path } => {
            let mut e = Enc::new(TAG_EXTRACT_PATH);
            e.u64(*id);
            e.limits(limits);
            e.path(path);
            e.finish()
        }
        Request::ExtractBytes {
            id,
            limits,
            kind,
            bytes,
        } => encode_extract_bytes(*id, limits, *kind, bytes),
        Request::Shutdown => Enc::new(TAG_SHUTDOWN).finish(),
        Request::Debug { id, hook } => {
            let mut e = Enc::new(TAG_DEBUG);
            e.u64(*id);
            let (code, bytes, keep) = match *hook {
                DebugHook::Hang => (1, 0, false),
                DebugHook::Overflow => (2, 0, false),
                DebugHook::Panic => (3, 0, false),
                DebugHook::Alloc { bytes, keep } => (4, bytes, keep),
            };
            e.u8(code);
            e.u64(bytes);
            e.u8(u8::from(keep));
            e.finish()
        }
    }
}

/// Encodes an ExtractBytes request without first copying `bytes` into a
/// [`Request`].
pub(crate) fn encode_extract_bytes(id: u64, limits: &Limits, kind: Kind, bytes: &[u8]) -> Vec<u8> {
    let mut e = Enc::new(TAG_EXTRACT_BYTES);
    e.0.reserve(bytes.len() + 64);
    e.u64(id);
    e.limits(limits);
    e.u8(kind_code(kind));
    e.bytes(bytes);
    e.finish()
}

/// Decodes a request payload.
pub(crate) fn decode_request(payload: &[u8]) -> Result<Request, ProtocolError> {
    let mut d = Dec(payload);
    let req = match d.u8()? {
        TAG_EXTRACT_PATH => Request::ExtractPath {
            id: d.u64()?,
            limits: d.limits()?,
            path: d.path()?,
        },
        TAG_EXTRACT_BYTES => Request::ExtractBytes {
            id: d.u64()?,
            limits: d.limits()?,
            kind: d.kind()?,
            bytes: d.bytes()?.to_vec(),
        },
        TAG_SHUTDOWN => Request::Shutdown,
        TAG_DEBUG => {
            let id = d.u64()?;
            let code = d.u8()?;
            let bytes = d.u64()?;
            let keep = d.bool()?;
            let hook = match code {
                1 => DebugHook::Hang,
                2 => DebugHook::Overflow,
                3 => DebugHook::Panic,
                4 => DebugHook::Alloc { bytes, keep },
                c => return Err(perr(format!("unknown debug hook {c}"))),
            };
            Request::Debug { id, hook }
        }
        tag => return Err(perr(format!("unknown request tag {tag:#04x}"))),
    };
    d.end()?;
    Ok(req)
}

/// Encodes a response as a complete frame.
pub(crate) fn encode_response(resp: &Response) -> Vec<u8> {
    match resp {
        Response::Hello {
            version,
            flags,
            pid,
        } => {
            let mut e = Enc::new(TAG_HELLO);
            e.0.extend_from_slice(MAGIC);
            e.u16(*version);
            e.u32(*flags);
            e.u32(*pid);
            e.finish()
        }
        Response::Extracted {
            id,
            heap,
            extracted: x,
        } => {
            let mut e = Enc::new(TAG_EXTRACTED);
            e.0.reserve(x.text.len() + 64);
            e.u64(*id);
            e.u64(*heap);
            e.u8(kind_code(x.kind));
            e.u8(u8::from(x.truncated));
            e.opt_str(x.encoding);
            e.opt_str(x.metadata.title.as_deref());
            e.opt_str(x.metadata.author.as_deref());
            e.opt_str(x.metadata.created.as_deref());
            e.str(&x.text);
            e.finish()
        }
        Response::Failed { id, heap, error } => {
            let mut e = Enc::new(TAG_FAILED);
            e.u64(*id);
            e.u64(*heap);
            let (code, io_kind, message) = match error {
                ExtractError::Unsupported(m) => (1, 0, m.clone()),
                ExtractError::TooLarge(m) => (2, 0, m.clone()),
                ExtractError::Corrupt(m) => (3, 0, m.clone()),
                ExtractError::Encrypted => (4, 0, String::new()),
                ExtractError::Io(err) => (5, io_kind_code(err.kind()), err.to_string()),
                // Client-side outcomes; a worker never produces them.
                other => (3, 0, other.to_string()),
            };
            e.u8(code);
            e.u8(io_kind);
            e.str(&message);
            e.finish()
        }
        Response::Done { id, heap } => {
            let mut e = Enc::new(TAG_DONE);
            e.u64(*id);
            e.u64(*heap);
            e.finish()
        }
        Response::ProtocolError(m) => {
            let mut e = Enc::new(TAG_PROTOCOL_ERROR);
            e.str(m);
            e.finish()
        }
    }
}

/// Overwrites the `heap` field of an encoded Extracted / Failed / Done
/// frame, so the worker can measure its heap after building the answer.
pub(crate) fn patch_heap(frame: &mut [u8], heap: u64) {
    if let Some(field) = frame.get_mut(HEAP_OFFSET..HEAP_OFFSET + 8) {
        field.copy_from_slice(&heap.to_le_bytes());
    }
}

/// Decodes a response payload.
pub(crate) fn decode_response(payload: &[u8]) -> Result<Response, ProtocolError> {
    let mut d = Dec(payload);
    let resp = match d.u8()? {
        TAG_HELLO => {
            if d.take(4)? != MAGIC {
                return Err(perr("bad magic in hello"));
            }
            Response::Hello {
                version: d.u16()?,
                flags: d.u32()?,
                pid: d.u32()?,
            }
        }
        TAG_EXTRACTED => {
            let id = d.u64()?;
            let heap = d.u64()?;
            let kind = d.kind()?;
            let truncated = d.bool()?;
            let encoding = d.opt_string()?.and_then(|name| {
                encoding_rs::Encoding::for_label(name.as_bytes()).map(encoding_rs::Encoding::name)
            });
            let metadata = Metadata {
                title: d.opt_string()?,
                author: d.opt_string()?,
                created: d.opt_string()?,
            };
            let text = d.string()?;
            Response::Extracted {
                id,
                heap,
                extracted: Extracted {
                    text,
                    kind,
                    truncated,
                    metadata,
                    encoding,
                },
            }
        }
        TAG_FAILED => {
            let id = d.u64()?;
            let heap = d.u64()?;
            let code = d.u8()?;
            let io_kind = d.u8()?;
            let message = d.string()?;
            let error = match code {
                1 => ExtractError::Unsupported(message),
                2 => ExtractError::TooLarge(message),
                3 => ExtractError::Corrupt(message),
                4 => ExtractError::Encrypted,
                5 => ExtractError::Io(io::Error::new(io_kind_from_code(io_kind), message)),
                c => return Err(perr(format!("unknown error code {c}"))),
            };
            Response::Failed { id, heap, error }
        }
        TAG_DONE => Response::Done {
            id: d.u64()?,
            heap: d.u64()?,
        },
        TAG_PROTOCOL_ERROR => Response::ProtocolError(d.string()?),
        tag => return Err(perr(format!("unknown response tag {tag:#04x}"))),
    };
    d.end()?;
    Ok(resp)
}

/// Writes a complete frame and flushes.
pub(crate) fn write_frame(w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    w.write_all(frame)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(frame: &[u8]) -> &[u8] {
        let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - 4);
        &frame[4..]
    }

    #[test]
    fn requests_round_trip() {
        let limits = Limits {
            max_depth: 3,
            ..Limits::default()
        };
        let path = PathBuf::from("مجلد/ملف تقرير.pdf");
        let frame = encode_request(&Request::ExtractPath {
            id: 7,
            limits: limits.clone(),
            path: path.clone(),
        });
        match decode_request(payload(&frame)).unwrap() {
            Request::ExtractPath {
                id,
                limits: l,
                path: p,
            } => {
                assert_eq!((id, l, p), (7, limits.clone(), path));
            }
            other => panic!("{other:?}"),
        }
        let frame = encode_extract_bytes(8, &limits, Kind::Docx, b"PK\x03\x04");
        match decode_request(payload(&frame)).unwrap() {
            Request::ExtractBytes {
                id, kind, bytes, ..
            } => {
                assert_eq!(
                    (id, kind, bytes.as_slice()),
                    (8, Kind::Docx, &b"PK\x03\x04"[..])
                );
            }
            other => panic!("{other:?}"),
        }
        let hook = DebugHook::Alloc {
            bytes: 1 << 20,
            keep: true,
        };
        let frame = encode_request(&Request::Debug { id: 9, hook });
        assert!(matches!(
            decode_request(payload(&frame)).unwrap(),
            Request::Debug { id: 9, hook: h } if h == hook
        ));
    }

    #[test]
    fn responses_round_trip() {
        let extracted = Extracted {
            text: "سلام\nHello".into(),
            kind: Kind::Pdf,
            truncated: true,
            metadata: Metadata {
                title: Some("عنوان".into()),
                author: None,
                created: Some("D:2024".into()),
            },
            encoding: Some("windows-1256"),
        };
        let mut frame = encode_response(&Response::Extracted {
            id: 3,
            heap: 0,
            extracted: extracted.clone(),
        });
        patch_heap(&mut frame, 4096);
        match decode_response(payload(&frame)).unwrap() {
            Response::Extracted {
                id,
                heap,
                extracted: x,
            } => {
                assert_eq!((id, heap, x), (3, 4096, extracted));
            }
            other => panic!("{other:?}"),
        }
        let frame = encode_response(&Response::Failed {
            id: 4,
            heap: 1,
            error: ExtractError::Io(io::Error::new(io::ErrorKind::NotFound, "gone")),
        });
        match decode_response(payload(&frame)).unwrap() {
            Response::Failed {
                error: ExtractError::Io(e),
                ..
            } => assert_eq!(
                (e.kind(), e.to_string()),
                (io::ErrorKind::NotFound, "gone".into())
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        assert!(decode_request(&[]).is_err());
        assert!(decode_request(&[0x55]).is_err());
        // Truncated ExtractPath.
        assert!(decode_request(&[TAG_EXTRACT_PATH, 1, 2, 3]).is_err());
        // Trailing bytes.
        let mut frame = encode_request(&Request::Shutdown);
        frame.push(0);
        assert!(decode_request(&frame[4..]).is_err());
        // Bad kind code.
        let mut frame = encode_extract_bytes(1, &Limits::default(), Kind::Text, b"x");
        let kind_at = 4 + 1 + 8 + 6 * 8;
        frame[kind_at] = 200;
        assert!(decode_request(&frame[4..]).is_err());
        // A byte string longer than the payload.
        let mut frame = encode_extract_bytes(1, &Limits::default(), Kind::Text, b"x");
        frame[kind_at + 1..kind_at + 5].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_request(&frame[4..]).is_err());
        // Bad hello magic, invalid UTF-8 text.
        assert!(
            decode_response(&[
                TAG_HELLO, b'X', b'X', b'X', b'X', 1, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ])
            .is_err()
        );
        let mut bad = vec![TAG_PROTOCOL_ERROR, 2, 0, 0, 0];
        bad.extend_from_slice(&[0xFF, 0xFE]);
        assert!(decode_response(&bad).is_err());
    }

    #[test]
    fn frames_are_bounded() {
        let frame = encode_request(&Request::Shutdown);
        let mut r = &frame[..];
        assert_eq!(read_frame(&mut r, 16).unwrap().unwrap(), vec![TAG_SHUTDOWN]);
        assert!(read_frame(&mut r, 16).unwrap().is_none());
        // Over the bound: rejected before reading (or allocating) the body.
        let mut r = &u32::MAX.to_le_bytes()[..];
        assert!(matches!(
            read_frame(&mut r, 1 << 20),
            Err(FrameError::TooLong(_))
        ));
        // Truncated header and body.
        let mut r = &[1u8, 0][..];
        assert!(matches!(read_frame(&mut r, 16), Err(FrameError::Truncated)));
        let mut r = &[5u8, 0, 0, 0, 1][..];
        assert!(matches!(read_frame(&mut r, 16), Err(FrameError::Truncated)));
    }
}
