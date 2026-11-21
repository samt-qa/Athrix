//! Out-of-process extraction: [`WorkerPool`] runs documents through the
//! `athrix-extract-worker` executable, so that a parser bug (panic, stack
//! overflow, runaway allocation, endless loop) costs one worker process
//! instead of the application.
//!
//! # Why
//!
//! In-process extraction catches panics, but not a stack overflow (which
//! aborts the process), not memory exhaustion, and not a parser that never
//! returns. `pdf-extract` can do all three on hostile PDFs (see the `pdf`
//! feature), so PDFs should only be parsed in a worker.
//!
//! # How
//!
//! A worker is a child process speaking a small versioned, length-prefixed
//! binary protocol over stdin/stdout (described in the source of
//! `worker/protocol.rs`). It is reused for many files. For each request the
//! pool:
//!
//! - enforces [`WorkerConfig::timeout`]: on expiry the worker is killed and
//!   the call returns [`ExtractError::Timeout`];
//! - detects a dead worker (crash, stack overflow, allocation past
//!   [`WorkerConfig::memory_limit_bytes`]) or a protocol violation, kills
//!   it, and returns [`ExtractError::WorkerCrashed`] with the exit status
//!   and the worker's last stderr line;
//! - recycles the worker after [`WorkerConfig::recycle_after`] files, or
//!   when its heap stays above [`WorkerConfig::recycle_memory_bytes`] after
//!   a request.
//!
//! A replacement worker is started lazily by the next request. Only start-up
//! failures (missing executable, failed handshake) trigger a backoff, from
//! [`WorkerConfig::backoff_initial`] doubling up to
//! [`WorkerConfig::backoff_max`]; during it, requests fail fast with
//! [`ExtractError::WorkerUnavailable`] instead of blocking. Crashes caused
//! by a document are that document's outcome and do not delay the next one.
//!
//! # Threads
//!
//! [`WorkerPool`] is `Send + Sync`. Share one (in an `Arc`) between the
//! scheduler's extraction threads and set [`WorkerConfig::max_workers`] to
//! the number of threads: each call checks a worker out exclusively, and a
//! call finding all `max_workers` busy waits for one.
//!
//! # Memory cap
//!
//! The worker binary installs a counting global allocator (the pure-Rust
//! `cap` crate) and caps its heap at [`WorkerConfig::memory_limit_bytes`];
//! an allocation past the cap aborts the worker, which the pool reports as
//! a crash. This works the same on Windows and Linux and needs no `unsafe`
//! here. A Windows Job Object would also cover non-heap memory, but needs
//! Win32 calls this crate does not make (`unsafe_code` is denied); stacks
//! are bounded by the explicit stack size instead.
//!
//! # Finding the executable
//!
//! [`WorkerConfig::program`], or else `athrix-extract-worker[.exe]` next to
//! the current executable (and, for test binaries in `target/*/deps`, one
//! directory up). See [`default_program`].
//!
//! # Features and shipping
//!
//! Cargo features belong to a package, not to one of its targets, so the
//! `pdf` feature cannot be on for the worker binary and off for the library
//! in the same build. It therefore stays off by default: an application
//! that depends on this crate without `pdf` cannot parse a PDF in-process
//! (it gets [`ExtractError::Unsupported`]), and the worker is built in a
//! *separate* cargo invocation with the feature:
//!
//! ```text
//! cargo build --release -p athrix-extract --bin athrix-extract-worker --features pdf
//! ```
//!
//! (Building it in the same invocation as the application would unify the
//! feature into the application's copy of the library too.) The desktop
//! app ships it as a Tauri sidecar: copy the executable to
//! `src-tauri/binaries/athrix-extract-worker-<target-triple>[.exe]` (e.g. in
//! `beforeBuildCommand`) and list `"binaries/athrix-extract-worker"` under
//! `bundle.externalBin` in `tauri.conf.json`. Tauri installs it next to the
//! main executable without the triple suffix, which is where
//! [`default_program`] looks. The app then routes files through
//! [`WorkerPool::extract_isolated`].

mod protocol;
mod server;

use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{ExtractError, Extracted, Kind, Limits};
use protocol::{MAX_REQUEST_FRAME, Request, Response, VERSION};

#[doc(hidden)]
pub use protocol::DebugHook;
#[doc(hidden)]
pub use server::{ProcessHooks, run_worker_process};

/// Name of the worker binary target (without the platform's executable
/// suffix).
pub const WORKER_BIN_NAME: &str = "athrix-extract-worker";

/// Environment variable that enables [`DebugHook`] requests in a worker.
const TEST_HOOKS_ENV: &str = "ATHRIX_EXTRACT_WORKER_TEST_HOOKS";

/// Default stack of the worker's extraction thread.
const DEFAULT_STACK_BYTES: usize = 64 * 1024 * 1024;

/// How long a crashed worker gets to finish exiting (and flush stderr)
/// before it is killed.
const CRASH_GRACE: Duration = Duration::from_secs(2);

/// Bytes of worker stderr kept for crash reports.
const STDERR_TAIL: usize = 4096;

/// `CREATE_NO_WINDOW`: a console worker started from a GUI application
/// must not open a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Settings of a [`WorkerPool`].
///
/// Start from [`WorkerConfig::default`] and change fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WorkerConfig {
    /// Worker executable; `None` for [`default_program`].
    pub program: Option<PathBuf>,
    /// Longest time one document may take. Default 30 s.
    pub timeout: Duration,
    /// Longest time a new worker may take to say hello. Default 10 s.
    pub startup_timeout: Duration,
    /// Most worker processes alive at once. Default 1.
    pub max_workers: usize,
    /// Files handled by one worker before it is replaced. Default 1000.
    pub recycle_after: u32,
    /// Heap a worker may keep between requests before it is replaced.
    /// Default 256 MiB.
    pub recycle_memory_bytes: u64,
    /// Heap cap of a worker process; an allocation past it kills the
    /// worker. `0` for none. Default 1 GiB.
    pub memory_limit_bytes: u64,
    /// Stack of the worker's extraction thread. Default 64 MiB.
    pub stack_bytes: usize,
    /// [`WorkerPool::extract_isolated`] sends every format to the worker,
    /// not only PDFs. Default `false`.
    pub isolate_all: bool,
    /// First delay before retrying after a worker failed to start.
    /// Default 100 ms.
    pub backoff_initial: Duration,
    /// Longest delay between start attempts. Default 10 s.
    pub backoff_max: Duration,
    /// Enables [`DebugHook`] requests (tests only).
    #[doc(hidden)]
    pub test_hooks: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            program: None,
            timeout: Duration::from_secs(30),
            startup_timeout: Duration::from_secs(10),
            max_workers: 1,
            recycle_after: 1000,
            recycle_memory_bytes: 256 * 1024 * 1024,
            memory_limit_bytes: 1024 * 1024 * 1024,
            stack_bytes: DEFAULT_STACK_BYTES,
            isolate_all: false,
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_secs(10),
            test_hooks: false,
        }
    }
}

/// The default worker executable: `athrix-extract-worker[.exe]` in the
/// directory of the current executable, or in its parent when the current
/// executable is a test binary under `target/<profile>/deps`.
pub fn default_program() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let name = format!("{WORKER_BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "current exe has no directory"))?;
    let beside = dir.join(&name);
    if !beside.is_file() && dir.file_name().is_some_and(|n| n == "deps") {
        if let Some(up) = dir.parent().map(|p| p.join(&name)).filter(|p| p.is_file()) {
            return Ok(up);
        }
    }
    Ok(beside)
}

/// Counters of a [`WorkerPool`], for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkerStats {
    /// Worker processes started successfully.
    pub spawned: u64,
    /// Requests answered by a worker (success or extraction error).
    pub answered: u64,
    /// Workers retired after `recycle_after` files or memory growth.
    pub recycled: u64,
    /// Workers that died or broke the protocol during a request.
    pub crashes: u64,
    /// Workers killed for exceeding the timeout.
    pub timeouts: u64,
    /// Failed attempts to start a worker.
    pub startup_failures: u64,
}

/// A pool of extraction worker processes. See the [module docs](self).
#[derive(Debug)]
pub struct WorkerPool {
    config: WorkerConfig,
    state: Mutex<PoolState>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct PoolState {
    idle: Vec<Worker>,
    /// Workers alive: idle, checked out, or being started.
    live: usize,
    /// Consecutive start-up failures.
    startup_failures: u32,
    /// No start attempt before this instant (backoff).
    retry_at: Option<Instant>,
    stats: WorkerStats,
}

/// How a request went wrong on the worker side.
enum Failure {
    Timeout,
    Crashed(String),
}

impl WorkerPool {
    /// A pool with `config`. Workers start on first use.
    pub fn new(config: WorkerConfig) -> WorkerPool {
        WorkerPool {
            config,
            state: Mutex::new(PoolState::default()),
            available: Condvar::new(),
        }
    }

    /// The pool's settings.
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Counters since the pool was created.
    pub fn stats(&self) -> WorkerStats {
        self.lock().stats
    }

    /// Extracts `path` in a worker process, whatever its format.
    ///
    /// Same results as [`crate::extract`] run in the worker, plus
    /// [`ExtractError::Timeout`], [`ExtractError::WorkerCrashed`] and
    /// [`ExtractError::WorkerUnavailable`].
    pub fn extract(
        &self,
        path: impl AsRef<Path>,
        limits: &Limits,
    ) -> Result<Extracted, ExtractError> {
        let path = std::path::absolute(path.as_ref())?;
        let limits = limits.clone();
        let answer = self.request(response_bound(&limits), move |id| {
            protocol::encode_request(&Request::ExtractPath { id, limits, path })
        })?;
        into_result(answer)
    }

    /// Extracts `bytes` (a document of format `kind`) in a worker process.
    pub fn extract_bytes(
        &self,
        kind: Kind,
        bytes: &[u8],
        limits: &Limits,
    ) -> Result<Extracted, ExtractError> {
        if bytes.len() as u64 > u64::from(MAX_REQUEST_FRAME) - 1024 {
            return Err(ExtractError::TooLarge(format!(
                "{} bytes is over the worker request limit",
                bytes.len()
            )));
        }
        let answer = self.request(response_bound(limits), |id| {
            protocol::encode_extract_bytes(id, limits, kind, bytes)
        })?;
        into_result(answer)
    }

    /// Extracts `path`, in a worker for PDFs (or for every format with
    /// [`WorkerConfig::isolate_all`]) and in-process ([`crate::extract`])
    /// otherwise.
    ///
    /// A file with an unknown extension is routed by its magic bytes.
    pub fn extract_isolated(
        &self,
        path: impl AsRef<Path>,
        limits: &Limits,
    ) -> Result<Extracted, ExtractError> {
        let path = path.as_ref();
        if self.config.isolate_all || is_pdf(path, limits) {
            self.extract(path, limits)
        } else {
            crate::extract(path, limits)
        }
    }

    /// Stops the idle workers. Busy ones stop when their request ends and
    /// the pool is dropped; new requests start new workers.
    pub fn shutdown_idle(&self) {
        let idle = {
            let mut st = self.lock();
            let idle = std::mem::take(&mut st.idle);
            st.live -= idle.len();
            idle
        };
        drop(idle);
        self.available.notify_all();
    }

    /// Sends a test-only request (the worker must run with
    /// [`WorkerConfig::test_hooks`]).
    #[doc(hidden)]
    pub fn debug_hook(&self, hook: DebugHook) -> Result<(), ExtractError> {
        match self.request(1 << 20, |id| {
            protocol::encode_request(&Request::Debug { id, hook })
        })? {
            Response::Done { .. } => Ok(()),
            other => Err(ExtractError::WorkerCrashed(format!(
                "unexpected answer {other:?}"
            ))),
        }
    }

    /// Process id of an idle worker, if one is idle (tests use it to see
    /// recycling).
    #[doc(hidden)]
    pub fn idle_worker_pid(&self) -> Option<u32> {
        self.lock().idle.last().map(|w| w.pid)
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs one request on a checked-out worker.
    fn request(
        &self,
        max_response: u32,
        frame: impl FnOnce(u64) -> Vec<u8>,
    ) -> Result<Response, ExtractError> {
        let mut worker = self.acquire()?;
        worker.next_id += 1;
        let id = worker.next_id;
        match worker.call(frame(id), id, self.config.timeout, max_response) {
            Ok(answer) => {
                worker.served += 1;
                let recycle = worker.served >= self.config.recycle_after.max(1)
                    || answer.heap() > self.config.recycle_memory_bytes;
                {
                    let mut st = self.lock();
                    st.stats.answered += 1;
                    st.stats.recycled += u64::from(recycle);
                }
                self.release(worker, !recycle);
                Ok(answer)
            }
            Err(Failure::Timeout) => {
                let _ = worker.reap(Duration::ZERO);
                self.lock().stats.timeouts += 1;
                self.release(worker, false);
                Err(ExtractError::Timeout(self.config.timeout))
            }
            Err(Failure::Crashed(why)) => {
                let status = worker.reap(CRASH_GRACE);
                self.lock().stats.crashes += 1;
                self.release(worker, false);
                Err(ExtractError::WorkerCrashed(format!("{why}; {status}")))
            }
        }
    }

    /// Checks out an idle worker, or starts one, or waits for one.
    fn acquire(&self) -> Result<Worker, ExtractError> {
        let mut st = self.lock();
        loop {
            if let Some(worker) = st.idle.pop() {
                return Ok(worker);
            }
            if st.live < self.config.max_workers.max(1) {
                if let Some(at) = st.retry_at {
                    let now = Instant::now();
                    if now < at {
                        return Err(ExtractError::WorkerUnavailable(format!(
                            "restarting after {} failed start(s); next attempt in {:?}",
                            st.startup_failures,
                            at - now
                        )));
                    }
                }
                st.live += 1;
                drop(st);
                let started = Worker::spawn(&self.config);
                let mut st = self.lock();
                return match started {
                    Ok(worker) => {
                        st.startup_failures = 0;
                        st.retry_at = None;
                        st.stats.spawned += 1;
                        Ok(worker)
                    }
                    Err(e) => {
                        st.live -= 1;
                        st.startup_failures = st.startup_failures.saturating_add(1);
                        st.stats.startup_failures += 1;
                        let doublings = (st.startup_failures - 1).min(16);
                        let delay = self
                            .config
                            .backoff_initial
                            .saturating_mul(1 << doublings)
                            .min(self.config.backoff_max);
                        st.retry_at = Some(Instant::now() + delay);
                        drop(st);
                        self.available.notify_one();
                        Err(e)
                    }
                };
            }
            st = self
                .available
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Returns a worker to the pool, or retires it.
    fn release(&self, worker: Worker, keep: bool) {
        let retired = {
            let mut st = self.lock();
            if keep {
                st.idle.push(worker);
                None
            } else {
                st.live -= 1;
                Some(worker)
            }
        };
        // Killing and reaping happen outside the lock.
        drop(retired);
        self.available.notify_one();
    }
}

/// Whether `path` should go to the worker as a PDF.
fn is_pdf(path: &Path, limits: &Limits) -> bool {
    match Kind::from_path(path) {
        Some(kind) => kind == Kind::Pdf,
        None => {
            let mut head = Vec::new();
            let read = std::fs::File::open(path).and_then(|f| {
                f.take((8 * 1024u64).min(limits.max_input_bytes))
                    .read_to_end(&mut head)
            });
            read.is_ok() && crate::sniff(&head) == Some(Kind::Pdf)
        }
    }
}

/// Largest response frame accepted for a request under `limits`: the text
/// (at most 4 bytes per character) plus metadata, which comes from the
/// input.
fn response_bound(limits: &Limits) -> u32 {
    let bound = (limits.max_output_chars as u64)
        .saturating_mul(4)
        .saturating_add(limits.max_input_bytes.saturating_mul(3))
        .saturating_add(1 << 20);
    u32::try_from(bound).unwrap_or(u32::MAX)
}

fn into_result(answer: Response) -> Result<Extracted, ExtractError> {
    match answer {
        Response::Extracted { extracted, .. } => Ok(extracted),
        Response::Failed { error, .. } => Err(error),
        other => Err(ExtractError::WorkerCrashed(format!(
            "unexpected answer {other:?}"
        ))),
    }
}

/// Kills and reaps the child when dropped.
#[derive(Debug)]
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One worker process and its I/O threads.
#[derive(Debug)]
struct Worker {
    /// Dropped last-but-one: closing `requests` ends the writer thread.
    requests: Sender<Vec<u8>>,
    responses: Receiver<Result<Vec<u8>, String>>,
    /// Bound on the next response frame, read by the reader thread.
    response_limit: Arc<AtomicU32>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<JoinHandle<()>>,
    pid: u32,
    served: u32,
    next_id: u64,
    process: Process,
}

impl Worker {
    fn spawn(config: &WorkerConfig) -> Result<Worker, ExtractError> {
        let program = match &config.program {
            Some(p) => p.clone(),
            None => default_program().map_err(|e| {
                ExtractError::WorkerUnavailable(format!("cannot locate the worker: {e}"))
            })?,
        };
        let unavailable = ExtractError::WorkerUnavailable;
        let mut cmd = Command::new(&program);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("--stack")
            .arg(config.stack_bytes.to_string());
        if config.memory_limit_bytes > 0 {
            cmd.arg("--memory-limit")
                .arg(config.memory_limit_bytes.to_string());
        }
        if config.test_hooks {
            cmd.env(TEST_HOOKS_ENV, "1");
        } else {
            cmd.env_remove(TEST_HOOKS_ENV);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| unavailable(format!("cannot start {}: {e}", program.display())))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process = Process(child);
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            return Err(unavailable("worker pipes missing".into()));
        };

        let response_limit = Arc::new(AtomicU32::new(1 << 20));
        let (resp_tx, responses) = mpsc::channel();
        let limit = Arc::clone(&response_limit);
        spawn_thread("athrix-extract-worker-out", move || {
            read_responses(stdout, &limit, &resp_tx);
        })
        .map_err(|e| unavailable(format!("cannot start reader thread: {e}")))?;

        let (requests, req_rx) = mpsc::channel::<Vec<u8>>();
        spawn_thread("athrix-extract-worker-in", move || {
            write_requests(stdin, &req_rx);
        })
        .map_err(|e| unavailable(format!("cannot start writer thread: {e}")))?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let tail = Arc::clone(&stderr_tail);
        let stderr_thread = spawn_thread("athrix-extract-worker-err", move || {
            keep_tail(stderr, &tail);
        })
        .map_err(|e| unavailable(format!("cannot start stderr thread: {e}")))?;

        let mut worker = Worker {
            requests,
            responses,
            response_limit,
            stderr_tail,
            stderr_thread: Some(stderr_thread),
            pid: process.0.id(),
            served: 0,
            next_id: 0,
            process,
        };
        let hello = match worker.responses.recv_timeout(config.startup_timeout) {
            Ok(Ok(payload)) => protocol::decode_response(&payload).map_err(|e| e.to_string()),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err("no hello in time".into()),
            Err(RecvTimeoutError::Disconnected) => Err("worker output closed".into()),
        };
        match hello {
            // The flags say whether the worker can do PDFs; one built
            // without the `pdf` feature answers PDFs with `Unsupported`.
            Ok(Response::Hello { version, pid, .. }) if version == VERSION => {
                worker.pid = pid;
                Ok(worker)
            }
            Ok(Response::Hello { version, .. }) => Err(unavailable(format!(
                "{} speaks protocol {version}, expected {VERSION}",
                program.display()
            ))),
            Ok(other) => Err(unavailable(format!("bad hello {other:?}"))),
            Err(why) => {
                let status = worker.reap(CRASH_GRACE);
                Err(unavailable(format!(
                    "{} failed to start: {why}; {status}",
                    program.display()
                )))
            }
        }
    }

    /// Sends one request and waits for its answer.
    fn call(
        &mut self,
        frame: Vec<u8>,
        id: u64,
        timeout: Duration,
        max_response: u32,
    ) -> Result<Response, Failure> {
        self.response_limit.store(max_response, Ordering::Relaxed);
        if self.requests.send(frame).is_err() {
            return Err(Failure::Crashed("worker input closed".into()));
        }
        let payload = match self.responses.recv_timeout(timeout) {
            Ok(Ok(payload)) => payload,
            Ok(Err(why)) => return Err(Failure::Crashed(why)),
            Err(RecvTimeoutError::Timeout) => return Err(Failure::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Failure::Crashed("worker output closed".into()));
            }
        };
        match protocol::decode_response(&payload) {
            Ok(Response::ProtocolError(msg)) => Err(Failure::Crashed(format!(
                "worker rejected the request: {msg}"
            ))),
            Ok(answer) if answer.id() == Some(id) => Ok(answer),
            Ok(answer) => Err(Failure::Crashed(format!(
                "protocol error: answer {:?} for request {id}",
                answer.id()
            ))),
            Err(e) => Err(Failure::Crashed(format!("protocol error: {e}"))),
        }
    }

    /// Waits up to `grace` for the worker to exit, kills it if it has not,
    /// and describes how it ended.
    fn reap(&mut self, grace: Duration) -> String {
        let child = &mut self.process.0;
        let deadline = Instant::now() + grace;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    break child.wait().ok();
                }
            }
        };
        // The process is gone, so its stderr is at end of file.
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8_lossy(&tail);
        // The last meaningful line ("memory allocation of N bytes failed",
        // "thread 'extract' has overflowed its stack", a panic message),
        // skipping std's "note: run with RUST_BACKTRACE=1" hint.
        let last = text
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("note:"));
        match last {
            Some(line) => format!("{}: {}", describe(status), truncate(line, 300)),
            None => describe(status),
        }
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(256 * 1024)
        .spawn(f)
}

fn read_responses(stdout: impl Read, limit: &AtomicU32, tx: &Sender<Result<Vec<u8>, String>>) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    loop {
        let item = match protocol::read_frame(&mut reader, limit.load(Ordering::Relaxed)) {
            Ok(Some(payload)) => Ok(payload),
            Ok(None) => Err("worker closed its output".to_owned()),
            Err(e) => Err(format!("bad frame from worker: {e}")),
        };
        let last = item.is_err();
        if tx.send(item).is_err() || last {
            return;
        }
    }
}

fn write_requests(mut stdin: ChildStdin, rx: &Receiver<Vec<u8>>) {
    for frame in rx {
        if protocol::write_frame(&mut stdin, &frame).is_err() {
            return;
        }
    }
}

fn keep_tail(mut stderr: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut t = tail.lock().unwrap_or_else(PoisonError::into_inner);
                t.extend_from_slice(&buf[..n]);
                if t.len() > STDERR_TAIL {
                    let cut = t.len() - STDERR_TAIL;
                    t.drain(..cut);
                }
            }
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// A human description of how a worker ended.
fn describe(status: Option<ExitStatus>) -> String {
    let Some(status) = status else {
        return "exit status unknown".into();
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let what = match signal {
                6 => " (SIGABRT: abort, e.g. memory cap reached)",
                9 => " (SIGKILL)",
                11 => " (SIGSEGV: e.g. stack overflow)",
                _ => "",
            };
            return format!("killed by signal {signal}{what}");
        }
    }
    match status.code() {
        #[cfg(windows)]
        Some(code) if code < 0 => {
            let ntstatus = code as u32;
            let what = match ntstatus {
                0xC000_00FD => " (stack overflow)",
                0xC000_0409 => " (abort, e.g. memory cap reached)",
                0xC000_0005 => " (access violation)",
                0xC000_0017 | 0xC000_012D => " (out of memory)",
                _ => "",
            };
            format!("exit status {ntstatus:#010X}{what}")
        }
        Some(code) => format!("exit code {code}"),
        None => "exited without a code".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<WorkerPool>();
    }

    #[test]
    fn missing_executable_fails_fast_with_backoff() {
        let config = WorkerConfig {
            program: Some(PathBuf::from(
                "definitely-not-here/athrix-extract-worker-missing",
            )),
            backoff_initial: Duration::from_secs(60),
            ..WorkerConfig::default()
        };
        let pool = WorkerPool::new(config);
        let first = pool.extract_bytes(Kind::Text, b"x", &Limits::default());
        assert!(
            matches!(&first, Err(ExtractError::WorkerUnavailable(m)) if m.contains("cannot start")),
            "{first:?}"
        );
        let second = pool.extract_bytes(Kind::Text, b"x", &Limits::default());
        assert!(
            matches!(&second, Err(ExtractError::WorkerUnavailable(m)) if m.contains("next attempt")),
            "{second:?}"
        );
        assert_eq!(pool.stats().startup_failures, 1);
    }

    #[test]
    fn response_bound_saturates() {
        let huge = Limits {
            max_output_chars: usize::MAX,
            ..Limits::default()
        };
        assert_eq!(response_bound(&huge), u32::MAX);
        assert!(response_bound(&Limits::default()) > 16 << 20);
    }
}
