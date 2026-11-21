//! Out-of-process extraction through the real `athrix-extract-worker`
//! binary: round trips, crash / timeout / overflow recovery, recycling and
//! protocol hygiene.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use athrix_extract::worker::DebugHook;
use athrix_extract::{ExtractError, Kind, Limits, WorkerConfig, WorkerPool};

#[cfg(feature = "pdf")]
#[path = "../src/tests/pdf_fixtures.rs"]
mod pdf_fixtures;

const WORKER: &str = env!("CARGO_BIN_EXE_athrix-extract-worker");

fn config() -> WorkerConfig {
    let mut c = WorkerConfig::default();
    c.program = Some(PathBuf::from(WORKER));
    c.test_hooks = true;
    c.timeout = Duration::from_secs(20);
    c
}

/// A fresh scratch directory under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("worker-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn text_round_trip_reuses_one_worker() {
    let pool = WorkerPool::new(config());
    let dir = scratch("text");
    let path = dir.join("ملاحظات.txt");
    std::fs::write(&path, "مرحبا بالعالم\r\nسطر ثان").unwrap();
    for _ in 0..3 {
        let e = pool.extract(&path, &Limits::default()).unwrap();
        assert_eq!(e.kind, Kind::Text);
        assert_eq!(e.text, "مرحبا بالعالم\nسطر ثان");
        assert_eq!(e.encoding, Some("UTF-8"));
    }
    let e = pool
        .extract_bytes(
            Kind::Html,
            "<title>عنوان</title><p>نص</p>".as_bytes(),
            &Limits::default(),
        )
        .unwrap();
    assert_eq!(e.metadata.title.as_deref(), Some("عنوان"));
    let missing = pool.extract(dir.join("missing.txt"), &Limits::default());
    assert!(
        matches!(&missing, Err(ExtractError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound),
        "{missing:?}"
    );
    let stats = pool.stats();
    assert_eq!((stats.spawned, stats.answered, stats.crashes), (1, 5, 0));
}

#[test]
fn hang_times_out_and_the_worker_is_replaced() {
    let mut c = config();
    c.timeout = Duration::from_millis(500);
    let pool = WorkerPool::new(c);
    pool.extract_bytes(Kind::Text, b"warm up", &Limits::default())
        .unwrap();
    let start = Instant::now();
    let r = pool.debug_hook(DebugHook::Hang);
    assert!(matches!(r, Err(ExtractError::Timeout(_))), "{r:?}");
    assert!(start.elapsed() < Duration::from_secs(10));
    let e = pool
        .extract_bytes(Kind::Text, b"after the hang", &Limits::default())
        .unwrap();
    assert_eq!(e.text, "after the hang");
    let stats = pool.stats();
    assert_eq!((stats.timeouts, stats.spawned), (1, 2));
}

#[test]
fn stack_overflow_is_a_crash_and_the_next_file_works() {
    let pool = WorkerPool::new(config());
    let r = pool.debug_hook(DebugHook::Overflow);
    match &r {
        Err(ExtractError::WorkerCrashed(msg)) => {
            // The status names the overflow on Windows; Linux reports the
            // signal (SIGSEGV or SIGABRT) and std's "overflowed its stack".
            assert!(
                msg.contains("stack overflow")
                    || msg.contains("overflowed its stack")
                    || msg.contains("signal"),
                "{msg}"
            );
        }
        other => panic!("{other:?}"),
    }
    let e = pool
        .extract_bytes(Kind::Text, b"still alive", &Limits::default())
        .unwrap();
    assert_eq!(e.text, "still alive");
    assert_eq!(pool.stats().crashes, 1);
}

#[test]
fn panic_outside_the_guard_is_a_crash() {
    let pool = WorkerPool::new(config());
    let r = pool.debug_hook(DebugHook::Panic);
    assert!(
        matches!(&r, Err(ExtractError::WorkerCrashed(m)) if m.contains("exit code 101")),
        "{r:?}"
    );
    pool.extract_bytes(Kind::Text, b"ok", &Limits::default())
        .unwrap();
}

#[test]
fn memory_cap_kills_the_worker() {
    let mut c = config();
    c.memory_limit_bytes = 64 << 20;
    let pool = WorkerPool::new(c);
    pool.debug_hook(DebugHook::Alloc {
        bytes: 8 << 20,
        keep: false,
    })
    .unwrap();
    let r = pool.debug_hook(DebugHook::Alloc {
        bytes: 256 << 20,
        keep: false,
    });
    assert!(
        matches!(&r, Err(ExtractError::WorkerCrashed(m)) if m.contains("memory allocation")),
        "{r:?}"
    );
    pool.extract_bytes(Kind::Text, b"ok", &Limits::default())
        .unwrap();
}

#[test]
fn workers_are_recycled_by_count_and_by_memory() {
    let mut c = config();
    c.recycle_after = 2;
    let pool = WorkerPool::new(c);
    pool.extract_bytes(Kind::Text, b"1", &Limits::default())
        .unwrap();
    let first = pool.idle_worker_pid().unwrap();
    pool.extract_bytes(Kind::Text, b"2", &Limits::default())
        .unwrap();
    assert_eq!(pool.idle_worker_pid(), None, "retired after 2 files");
    pool.extract_bytes(Kind::Text, b"3", &Limits::default())
        .unwrap();
    assert_ne!(pool.idle_worker_pid(), Some(first));

    let mut c = config();
    c.recycle_memory_bytes = 16 << 20;
    let pool = WorkerPool::new(c);
    pool.debug_hook(DebugHook::Alloc {
        bytes: 1 << 20,
        keep: true,
    })
    .unwrap();
    assert!(pool.idle_worker_pid().is_some(), "small growth is kept");
    pool.debug_hook(DebugHook::Alloc {
        bytes: 32 << 20,
        keep: true,
    })
    .unwrap();
    assert_eq!(pool.idle_worker_pid(), None, "retired after growing");
    assert_eq!(pool.stats().recycled, 1);
}

#[test]
fn shared_between_threads() {
    let mut c = config();
    c.max_workers = 3;
    let pool = std::sync::Arc::new(WorkerPool::new(c));
    let threads: Vec<_> = (0..6)
        .map(|i| {
            let pool = std::sync::Arc::clone(&pool);
            std::thread::spawn(move || {
                for j in 0..5 {
                    let text = format!("نص {i}-{j}");
                    let e = pool
                        .extract_bytes(Kind::Text, text.as_bytes(), &Limits::default())
                        .unwrap();
                    assert_eq!(e.text, text);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let stats = pool.stats();
    assert_eq!(stats.answered, 30);
    assert!(stats.spawned <= 3, "{stats:?}");
}

#[test]
fn extract_isolated_keeps_other_formats_in_process() {
    let mut c = config();
    c.program = Some(PathBuf::from("no/such/worker"));
    let pool = WorkerPool::new(c);
    let dir = scratch("isolated");
    let txt = dir.join("a.txt");
    std::fs::write(&txt, "in process").unwrap();
    assert_eq!(
        pool.extract_isolated(&txt, &Limits::default())
            .unwrap()
            .text,
        "in process"
    );
    // A PDF, even without its extension, goes to the (missing) worker.
    let pdf = dir.join("report.bin");
    std::fs::write(&pdf, b"%PDF-1.4\n%%EOF\n").unwrap();
    let r = pool.extract_isolated(&pdf, &Limits::default());
    assert!(
        matches!(r, Err(ExtractError::WorkerUnavailable(_))),
        "{r:?}"
    );
    assert_eq!(pool.stats().spawned, 0);
}

/// Talks to the binary directly: it must answer garbage with a protocol
/// error frame and exit, and must refuse test hooks unless enabled.
#[test]
fn worker_rejects_malformed_frames() {
    let cases: [(&[u8], bool); 4] = [
        (&[0xFF, 0xFF, 0xFF, 0xFF], true),
        (&[1, 0, 0, 0, 0x55], true),
        (&[9, 0, 0, 0, 0x01, 0, 0], true),
        // A Debug(Hang) request without test hooks.
        (
            &[
                19, 0, 0, 0, 0x70, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            false,
        ),
    ];
    for (frame, hooks) in cases {
        let mut cmd = Command::new(WORKER);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if hooks {
            cmd.env("ATHRIX_EXTRACT_WORKER_TEST_HOOKS", "1");
        } else {
            cmd.env_remove("ATHRIX_EXTRACT_WORKER_TEST_HOOKS");
        }
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(frame).unwrap();
        drop(stdin);
        let mut out = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(2), "{frame:?}");
        // Hello frame, then a ProtocolError frame (tag 0x8F).
        let hello_len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        let rest = &out[4 + hello_len..];
        assert_eq!(rest[4], 0x8F, "{frame:?}: {out:?}");
    }
}

#[test]
fn version_flag() {
    let out = Command::new(WORKER).arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("protocol 1"), "{text}");
}

#[cfg(feature = "pdf")]
mod pdf {
    use super::*;
    use pdf_fixtures::*;

    #[test]
    fn arabic_pdf_round_trip() {
        let pool = WorkerPool::new(config());
        let dir = scratch("pdf");
        let path = dir.join("تقرير.pdf");
        std::fs::write(&path, arabic_pdf(FORMS, &[3, 2, 1], "Hello 2024", None)).unwrap();
        let e = pool.extract_isolated(&path, &Limits::default()).unwrap();
        assert_eq!(e.kind, Kind::Pdf);
        assert_eq!(e.text, "سلام\nHello 2024\nسلام");
        assert_eq!(e.metadata.title.as_deref(), Some("عنوان"));
        assert_eq!(e.metadata.author.as_deref(), Some("Sami"));
        let e = pool
            .extract_bytes(
                Kind::Pdf,
                &arabic_pdf(BASE, &[3, 2, 1], "x", None),
                &Limits::default(),
            )
            .unwrap();
        assert_eq!(e.text, "سلام\nx\nسلام");
    }

    #[test]
    fn library_panic_is_an_error_and_the_worker_lives_on() {
        let pool = WorkerPool::new(config());
        let r = pool.extract_bytes(Kind::Pdf, &panicking_pdf(), &Limits::default());
        assert!(matches!(r, Err(ExtractError::Corrupt(_))), "{r:?}");
        let e = pool
            .extract_bytes(
                Kind::Pdf,
                &arabic_pdf(FORMS, &[3, 2, 1], "next", None),
                &Limits::default(),
            )
            .unwrap();
        assert_eq!(e.text, "سلام\nnext\nسلام");
        let stats = pool.stats();
        assert_eq!((stats.spawned, stats.crashes), (1, 0), "{stats:?}");
    }

    #[test]
    fn self_drawing_form_overflows_the_worker_not_us() {
        let pool = WorkerPool::new(config());
        let r = pool.extract_bytes(Kind::Pdf, &self_drawing_pdf(), &Limits::default());
        assert!(
            matches!(&r, Err(ExtractError::WorkerCrashed(m))
                if m.contains("stack overflow") || m.contains("overflowed its stack") || m.contains("signal")),
            "{r:?}"
        );
        let e = pool
            .extract_bytes(
                Kind::Pdf,
                &arabic_pdf(FORMS, &[3, 2, 1], "after", None),
                &Limits::default(),
            )
            .unwrap();
        assert_eq!(e.text, "سلام\nafter\nسلام");
        assert_eq!(pool.stats().crashes, 1);
    }
}
