//! The worker side: the request loop run by the `athrix-extract-worker`
//! binary.

use std::ffi::OsString;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use super::protocol::{self, DebugHook, FLAG_PDF, MAX_REQUEST_FRAME, Request, Response, VERSION};
use super::{DEFAULT_STACK_BYTES, TEST_HOOKS_ENV};
use crate::ExtractError;

/// Exit code after a protocol error.
const EXIT_PROTOCOL: u8 = 2;
/// Exit code when the worker cannot start (bad arguments, memory cap).
const EXIT_STARTUP: u8 = 3;
/// Exit code when the client's pipe is gone.
const EXIT_PIPE: u8 = 4;
/// Exit code after a panic outside the extraction guard.
const EXIT_PANIC: u8 = 101;

/// Process-level services only the binary can provide: its global
/// allocator is what measures and caps the heap.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessHooks {
    /// Heap bytes in use now.
    pub allocated: fn() -> usize,
    /// Caps the heap at this many bytes; `false` if that is not possible.
    pub set_memory_limit: fn(usize) -> bool,
}

/// Entry point of the `athrix-extract-worker` binary.
///
/// Arguments: `--memory-limit <bytes>` (heap cap; allocation past it aborts
/// the process), `--stack <bytes>` (stack of the extraction thread,
/// default 64 MiB), `--version`. Requests come on stdin, responses go to
/// stdout (see the protocol table in [`crate::worker`]); stderr carries
/// diagnostics only.
#[doc(hidden)]
pub fn run_worker_process(hooks: ProcessHooks) -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!(
                "athrix-extract-worker {} (protocol {VERSION}, pdf: {})",
                env!("CARGO_PKG_VERSION"),
                cfg!(feature = "pdf")
            );
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("athrix-extract-worker: {msg}");
            return ExitCode::from(EXIT_STARTUP);
        }
    };
    if let Some(limit) = args.memory_limit {
        if !(hooks.set_memory_limit)(limit) {
            eprintln!("athrix-extract-worker: cannot cap the heap at {limit} bytes");
            return ExitCode::from(EXIT_STARTUP);
        }
    }
    let options = ServeOptions {
        allocated: hooks.allocated,
        test_hooks: std::env::var_os(TEST_HOOKS_ENV).is_some_and(|v| v == "1"),
    };
    // All work happens on a thread whose stack size is set explicitly: the
    // main thread's stack is fixed by the linker (1 MiB on Windows), too
    // small for deeply nested documents.
    let thread = std::thread::Builder::new()
        .name("extract".into())
        .stack_size(args.stack)
        .spawn(move || {
            let input = BufReader::with_capacity(64 * 1024, io::stdin().lock());
            let output = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
            serve(input, output, &options)
        });
    match thread {
        Ok(handle) => handle.join().unwrap_or(ExitCode::from(EXIT_PANIC)),
        Err(e) => {
            eprintln!("athrix-extract-worker: cannot start the extraction thread: {e}");
            ExitCode::from(EXIT_STARTUP)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    memory_limit: Option<usize>,
    stack: usize,
}

/// `Ok(None)` for `--version`.
fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Option<Args>, String> {
    let mut out = Args {
        memory_limit: None,
        stack: DEFAULT_STACK_BYTES,
    };
    let mut args = args.map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<usize, String> {
            let v = args.next().ok_or_else(|| format!("{name} needs a value"))?;
            v.parse().map_err(|_| format!("bad {name} value {v:?}"))
        };
        match arg.as_str() {
            "--memory-limit" => out.memory_limit = Some(value("--memory-limit")?),
            "--stack" => out.stack = value("--stack")?.max(1024 * 1024),
            "--version" => return Ok(None),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some(out))
}

pub(crate) struct ServeOptions {
    pub(crate) allocated: fn() -> usize,
    pub(crate) test_hooks: bool,
}

/// Answers requests until stdin closes, a Shutdown request, or a protocol
/// error.
pub(crate) fn serve(mut input: impl Read, mut output: impl Write, opts: &ServeOptions) -> ExitCode {
    let hello = protocol::encode_response(&Response::Hello {
        version: VERSION,
        flags: if cfg!(feature = "pdf") { FLAG_PDF } else { 0 },
        pid: std::process::id(),
    });
    if protocol::write_frame(&mut output, &hello).is_err() {
        return ExitCode::from(EXIT_PIPE);
    }
    loop {
        let request = match protocol::read_frame(&mut input, MAX_REQUEST_FRAME) {
            Ok(Some(payload)) => protocol::decode_request(&payload).map_err(|e| e.to_string()),
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => Err(e.to_string()),
        };
        let request = match request {
            Ok(r) => r,
            Err(msg) => return protocol_error(&mut output, &msg),
        };
        let mut frame = match request {
            Request::Shutdown => return ExitCode::SUCCESS,
            Request::ExtractPath { id, limits, path } => answer(id, crate::extract(&path, &limits)),
            Request::ExtractBytes {
                id,
                limits,
                kind,
                bytes,
            } => {
                let result = crate::extract_bytes(kind, &bytes, &limits);
                drop(bytes);
                answer(id, result)
            }
            Request::Debug { id, hook } => {
                if !opts.test_hooks {
                    return protocol_error(&mut output, "test hooks are disabled");
                }
                run_hook(hook);
                protocol::encode_response(&Response::Done { id, heap: 0 })
            }
        };
        // Measured after the input and the result are gone, minus the
        // frame itself: what the worker keeps between requests.
        let heap = (opts.allocated)().saturating_sub(frame.capacity());
        protocol::patch_heap(&mut frame, heap as u64);
        if protocol::write_frame(&mut output, &frame).is_err() {
            return ExitCode::from(EXIT_PIPE);
        }
    }
}

fn answer(id: u64, result: Result<crate::Extracted, ExtractError>) -> Vec<u8> {
    let response = match result {
        Ok(extracted) => Response::Extracted {
            id,
            heap: 0,
            extracted,
        },
        Err(error) => Response::Failed { id, heap: 0, error },
    };
    protocol::encode_response(&response)
}

fn protocol_error(output: &mut impl Write, msg: &str) -> ExitCode {
    eprintln!("athrix-extract-worker: protocol error: {msg}");
    let frame = protocol::encode_response(&Response::ProtocolError(msg.to_owned()));
    let _ = protocol::write_frame(output, &frame);
    ExitCode::from(EXIT_PROTOCOL)
}

/// Allocations kept alive by [`DebugHook::Alloc`] with `keep`.
static KEPT: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

fn run_hook(hook: DebugHook) {
    match hook {
        DebugHook::Hang => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        DebugHook::Overflow => {
            std::hint::black_box(recurse(0));
        }
        DebugHook::Panic => panic!("test hook: panic outside the extraction guard"),
        DebugHook::Alloc { bytes, keep } => {
            let block = vec![0x5Au8; usize::try_from(bytes).unwrap_or(usize::MAX)];
            if keep {
                KEPT.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(block);
            }
        }
    }
}

/// Recurses without bound (each frame holds 1 KiB), for the stack
/// overflow test hook.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let frame = std::hint::black_box([depth as u8; 1024]);
    if depth == u64::MAX {
        return 0;
    }
    recurse(depth + 1).wrapping_add(u64::from(frame[(depth % 1024) as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Limits};
    use protocol::{Request, decode_response, encode_request};

    fn run(input: Vec<u8>, test_hooks: bool) -> (ExitCode, Vec<Response>) {
        let mut out = Vec::new();
        let code = serve(
            &input[..],
            &mut out,
            &ServeOptions {
                allocated: || 0,
                test_hooks,
            },
        );
        let mut responses = Vec::new();
        let mut r = &out[..];
        while let Some(p) = protocol::read_frame(&mut r, u32::MAX).unwrap() {
            responses.push(decode_response(&p).unwrap());
        }
        (code, responses)
    }

    #[test]
    fn serves_until_shutdown() {
        let mut input =
            protocol::encode_extract_bytes(1, &Limits::default(), Kind::Text, "سلام".as_bytes());
        input.extend(protocol::encode_extract_bytes(
            2,
            &Limits::default(),
            Kind::Docx,
            b"nope",
        ));
        input.extend(encode_request(&Request::Shutdown));
        input.extend(encode_request(&Request::Shutdown));
        let (code, responses) = run(input, false);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(responses.len(), 3, "{responses:?}");
        assert!(matches!(
            responses[0],
            Response::Hello {
                version: VERSION,
                ..
            }
        ));
        assert!(
            matches!(&responses[1], Response::Extracted { id: 1, extracted, .. } if extracted.text == "سلام")
        );
        assert!(matches!(responses[2], Response::Failed { id: 2, .. }));
    }

    #[test]
    fn malformed_frames_end_the_worker() {
        for bad in [
            vec![0xFF, 0xFF, 0xFF, 0xFF],
            vec![1, 0, 0, 0, 0x55],
            vec![3, 0, 0, 0, 1],
        ] {
            let (code, responses) = run(bad, true);
            assert_eq!(code, ExitCode::from(EXIT_PROTOCOL));
            assert!(matches!(responses.last(), Some(Response::ProtocolError(_))));
        }
        let hook = encode_request(&Request::Debug {
            id: 1,
            hook: DebugHook::Alloc {
                bytes: 1,
                keep: false,
            },
        });
        let (code, responses) = run(hook, false);
        assert_eq!(code, ExitCode::from(EXIT_PROTOCOL));
        assert!(
            matches!(responses.last(), Some(Response::ProtocolError(m)) if m.contains("disabled"))
        );
    }

    #[test]
    fn arguments() {
        let parse = |a: &[&str]| parse_args(a.iter().map(OsString::from));
        assert_eq!(
            parse(&["--memory-limit", "1024", "--stack", "8388608"]),
            Ok(Some(Args {
                memory_limit: Some(1024),
                stack: 8 << 20
            }))
        );
        assert_eq!(parse(&["--version"]), Ok(None));
        assert!(parse(&["--stack"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
