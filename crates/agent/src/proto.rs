//! The host ↔ agent wire protocol. **std-only** and hand-rolled: a length-prefixed binary
//! framing with explicit field encoding, so there is no dependency to vendor and nothing whose
//! textual output could drift between versions. *We* own every byte on the wire.
//!
//! This file is the single source of truth for the contract. `rustle-core` includes this
//! exact module (via `#[path]`) for its client-side codec, and the agent binary embeds it too, so
//! both ends share one definition.
//!
//! Frame: `u32` big-endian length, then that many payload bytes.
//! Payload: a `u8` message tag, then tag-specific fields.
//! Primitives: `u32`/`u64` big-endian; `bool` as one byte; `str` as `u32` length + UTF-8 bytes;
//! `vec<T>` as `u32` count + elements.

use std::io::{self, Read, Write};

/// Bumped on any incompatible change to the encodings below. The host refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

// --- framing -------------------------------------------------------------------------------

/// Write one length-prefixed frame and flush.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Read one length-prefixed frame. A clean EOF at a frame boundary surfaces as `UnexpectedEof`.
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

// --- primitive encoders --------------------------------------------------------------------

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(v as u8);
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// A cursor over a frame body, decoding primitives and bounds-checking as it goes. Any short
/// read returns `None`, which callers turn into a protocol error rather than a panic.
struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let slice = self.b.get(self.i..end)?;
        self.i = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_be_bytes(s.try_into().unwrap()))
    }
    fn boolean(&mut self) -> Option<bool> {
        self.u8().map(|b| b != 0)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        // Paths are bytes; lossy keeps us robust to the rare non-UTF-8 name (handled identically
        // on both ends), matching the rest of the codebase.
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

// --- messages ------------------------------------------------------------------------------

/// One regular file in a manifest: path relative to the build root, plus the quick-check keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub rel: String,
    pub size: u64,
    pub mtime: u32,
}

/// One symlink in a manifest: path relative to the build root, plus its verbatim target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkEntry {
    pub rel: String,
    pub target: String,
}

/// Host → agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Version handshake.
    Hello { proto: u32 },
    /// The full local source manifest + sync policy. The agent reconciles its own tree against
    /// this: creates needed dirs, recreates changed symlinks, prunes extraneous entries (when
    /// `prune`), and replies with the files whose contents the host must upload.
    Plan {
        include_hidden: bool,
        prune: bool,
        excludes: Vec<String>,
        files: Vec<FileEntry>,
        links: Vec<LinkEntry>,
    },
    /// Graceful shutdown.
    Bye,
}

/// Agent → host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    HelloOk { agent: u32, proto: u32 },
    /// The reconciliation result: `uploads` are the rel paths whose bytes the host must now send
    /// over SFTP (parents already created by the agent). The counters are for logging.
    Worklist {
        uploads: Vec<String>,
        created_dirs: u32,
        pruned: u32,
        symlinks: u32,
    },
    /// The agent could not satisfy the request (e.g. a filesystem error it can't recover from).
    Error { message: String },
}

const T_HELLO: u8 = 1;
const T_PLAN: u8 = 2;
const T_BYE: u8 = 3;

const R_HELLO_OK: u8 = 1;
const R_WORKLIST: u8 = 2;
const R_ERROR: u8 = 3;

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Hello { proto } => {
                out.push(T_HELLO);
                put_u32(&mut out, *proto);
            }
            Request::Plan {
                include_hidden,
                prune,
                excludes,
                files,
                links,
            } => {
                out.push(T_PLAN);
                put_bool(&mut out, *include_hidden);
                put_bool(&mut out, *prune);
                put_u32(&mut out, excludes.len() as u32);
                for e in excludes {
                    put_str(&mut out, e);
                }
                put_u32(&mut out, files.len() as u32);
                for f in files {
                    put_str(&mut out, &f.rel);
                    put_u64(&mut out, f.size);
                    put_u32(&mut out, f.mtime);
                }
                put_u32(&mut out, links.len() as u32);
                for l in links {
                    put_str(&mut out, &l.rel);
                    put_str(&mut out, &l.target);
                }
            }
            Request::Bye => out.push(T_BYE),
        }
        out
    }

    pub fn decode(body: &[u8]) -> Option<Request> {
        let mut r = Reader::new(body);
        match r.u8()? {
            T_HELLO => Some(Request::Hello { proto: r.u32()? }),
            T_PLAN => {
                let include_hidden = r.boolean()?;
                let prune = r.boolean()?;
                let n = r.u32()? as usize;
                let mut excludes = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    excludes.push(r.string()?);
                }
                let n = r.u32()? as usize;
                let mut files = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    files.push(FileEntry {
                        rel: r.string()?,
                        size: r.u64()?,
                        mtime: r.u32()?,
                    });
                }
                let n = r.u32()? as usize;
                let mut links = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    links.push(LinkEntry {
                        rel: r.string()?,
                        target: r.string()?,
                    });
                }
                Some(Request::Plan {
                    include_hidden,
                    prune,
                    excludes,
                    files,
                    links,
                })
            }
            T_BYE => Some(Request::Bye),
            _ => None,
        }
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::HelloOk { agent, proto } => {
                out.push(R_HELLO_OK);
                put_u32(&mut out, *agent);
                put_u32(&mut out, *proto);
            }
            Response::Worklist {
                uploads,
                created_dirs,
                pruned,
                symlinks,
            } => {
                out.push(R_WORKLIST);
                put_u32(&mut out, uploads.len() as u32);
                for u in uploads {
                    put_str(&mut out, u);
                }
                put_u32(&mut out, *created_dirs);
                put_u32(&mut out, *pruned);
                put_u32(&mut out, *symlinks);
            }
            Response::Error { message } => {
                out.push(R_ERROR);
                put_str(&mut out, message);
            }
        }
        out
    }

    pub fn decode(body: &[u8]) -> Option<Response> {
        let mut r = Reader::new(body);
        match r.u8()? {
            R_HELLO_OK => Some(Response::HelloOk {
                agent: r.u32()?,
                proto: r.u32()?,
            }),
            R_WORKLIST => {
                let n = r.u32()? as usize;
                let mut uploads = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    uploads.push(r.string()?);
                }
                Some(Response::Worklist {
                    uploads,
                    created_dirs: r.u32()?,
                    pruned: r.u32()?,
                    symlinks: r.u32()?,
                })
            }
            R_ERROR => Some(Response::Error {
                message: r.string()?,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_plan_round_trips() {
        let req = Request::Plan {
            include_hidden: false,
            prune: true,
            excludes: vec!["target".into(), ".*".into()],
            files: vec![
                FileEntry { rel: "src/main.rs".into(), size: 123, mtime: 1_700_000_000 },
                FileEntry { rel: "Cargo.toml".into(), size: 45, mtime: 1_700_000_001 },
            ],
            links: vec![LinkEntry { rel: "link.rs".into(), target: "src/main.rs".into() }],
        };
        assert_eq!(Request::decode(&req.encode()), Some(req));
    }

    #[test]
    fn response_worklist_round_trips() {
        let resp = Response::Worklist {
            uploads: vec!["a".into(), "b/c.rs".into()],
            created_dirs: 3,
            pruned: 1,
            symlinks: 2,
        };
        assert_eq!(Response::decode(&resp.encode()), Some(resp));
    }

    #[test]
    fn truncated_frame_decodes_to_none_not_panic() {
        let full = Request::Hello { proto: 7 }.encode();
        assert!(Request::decode(&full[..full.len() - 2]).is_none());
        assert!(Request::decode(&[]).is_none());
    }

    #[test]
    fn frame_write_read_is_symmetric() {
        let body = Request::Bye.encode();
        let mut buf = Vec::new();
        write_frame(&mut buf, &body).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), body);
    }
}
