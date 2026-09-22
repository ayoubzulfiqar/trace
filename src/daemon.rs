//! Transports: inline stdio server, shared per-project daemon, and the stdio
//! shim that connects agents to the daemon.
//!
//! - `trace daemon <root>` listens on a per-project Unix socket. Messages are
//!   newline-delimited JSON-RPC over a persistent connection; every
//!   connection gets its own protocol session while all share one [`Server`]
//!   (one index, one SQLite store). A lock file guarantees one daemon per
//!   project; an idle timeout lets auto-spawned daemons exit on their own.
//! - `trace serve <root>` is the shim agents launch: it connects to the
//!   daemon (spawning it when needed) and pipes stdio through. If the daemon
//!   dies mid-session the shim answers in-flight requests with an error,
//!   respawns/reconnects, replays the MCP handshake, and carries on; if the
//!   daemon cannot be reached at all it serves inline.
//! - Non-Unix platforms always serve inline over stdio.

use crate::mcp::{Refresh, Server, Session};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Largest accepted JSON-RPC message (one line).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Idle timeout for daemons the shim spawns on demand.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The user's trace home (`$TRACE_HOME`, else `~/.trace`).
pub fn trace_home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TRACE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".trace"))
        .unwrap_or_else(|| PathBuf::from(".trace"))
}

/// Stable 64-bit identifier of a project root (FNV-1a over the canonical
/// path — unlike `DefaultHasher`, stable across Rust releases).
pub fn project_id(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Where a project's daemon keeps its socket, lock and log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub log: PathBuf,
}

/// `sun_path` holds 104 (macOS) to 108 (Linux) bytes including the NUL.
const MAX_SOCKET_PATH: usize = 100;

/// A short directory for sockets whose natural path is too long: the
/// per-user runtime dir when there is one, else a per-uid directory in the
/// temp dir (created 0700 and ownership-checked before use — never a shared,
/// predictable socket path another user could squat).
fn short_socket_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(runtime).join("trace");
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("trace-{uid}"))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("trace")
    }
}

/// Paths for `root`'s daemon. The socket name embeds the binary version so an
/// upgraded shim never talks to a daemon from an older release.
pub fn paths_for_root(root: &Path) -> DaemonPaths {
    let id = project_id(root);
    let dir = trace_home_dir().join(format!("project-{id}"));
    let version = env!("CARGO_PKG_VERSION");
    let mut socket = dir.join(format!("daemon-{version}.sock"));
    if socket.as_os_str().len() >= MAX_SOCKET_PATH {
        socket = short_socket_dir().join(format!("{id}-{version}.sock"));
    }
    DaemonPaths {
        lock: dir.join(format!("daemon-{version}.lock")),
        log: dir.join("daemon.log"),
        socket,
        dir,
    }
}

/// Read one `\n`-terminated message of at most `limit` bytes into `buf`.
/// Returns `Ok(false)` at EOF.
fn read_message(reader: &mut impl BufRead, buf: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    buf.clear();
    let n = io::Read::take(&mut *reader, limit as u64 + 1).read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(false);
    }
    if buf.len() > limit && !buf.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message exceeds {limit} bytes"),
        ));
    }
    Ok(true)
}

fn write_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Serve one message stream until EOF: the loop shared by stdio and sockets.
pub fn serve_stream(
    server: &Server,
    reader: impl io::Read,
    mut writer: impl Write,
) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut session = Session::default();
    let mut buf = Vec::with_capacity(8192);
    loop {
        match read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let resp = crate::mcp::error_response(
                    serde_json::Value::Null,
                    crate::mcp::INVALID_REQUEST,
                    &e.to_string(),
                );
                let _ = write_line(&mut writer, &resp.to_string());
                return Err(e);
            }
            Err(e) => return Err(e),
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(resp) = server.handle_message(&mut session, line) {
            write_line(&mut writer, &resp)?;
        }
    }
}

/// Build the index in the background so the first query is fast.
fn warm_up(server: &Arc<Server>) {
    if server.blocked_reason().is_some() {
        return;
    }
    let server = Arc::clone(server);
    let _ = std::thread::Builder::new()
        .name("trace-warmup".into())
        .spawn(move || {
            if let Err(e) = server.ensure_index(Refresh::IfStale) {
                eprintln!("trace: initial index failed: {e}");
            }
        });
}

/// Inline mode: serve MCP over this process's stdin/stdout.
pub fn run_stdio(root: PathBuf) -> io::Result<()> {
    let server = Arc::new(Server::new(root));
    if let Some(reason) = server.blocked_reason() {
        eprintln!("trace: {reason}");
    }
    warm_up(&server);
    serve_stream(&server, io::stdin().lock(), io::stdout().lock())
}

/// `trace serve`: connect agents to the shared daemon, or serve inline.
pub fn run_shim(root: PathBuf) -> io::Result<()> {
    if std::env::var_os("TRACE_NO_DAEMON").is_some_and(|v| !v.is_empty() && v != "0") {
        return run_stdio(root);
    }
    #[cfg(unix)]
    {
        if crate::root::is_broad_root(&root) {
            // Never spawn a daemon for `/` or `$HOME`; inline mode explains why.
            return run_stdio(root);
        }
        match unix::connect_or_spawn(&root) {
            Ok(stream) => return unix::proxy(root, stream),
            Err(e) => eprintln!("trace: daemon unavailable ({e}); serving inline"),
        }
    }
    run_stdio(root)
}

/// Is a daemon serving `root`, and on which socket/pid?
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub running: bool,
    pub socket: PathBuf,
    pub pid: Option<u32>,
}

pub fn daemon_status(root: &Path) -> DaemonStatus {
    let paths = paths_for_root(root);
    #[cfg(unix)]
    let running = std::os::unix::net::UnixStream::connect(&paths.socket).is_ok();
    #[cfg(not(unix))]
    let running = false;
    let pid = std::fs::read_to_string(&paths.lock)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|_| running);
    DaemonStatus {
        running,
        socket: paths.socket,
        pid,
    }
}

#[cfg(unix)]
pub use unix::{ensure_daemon_running, run_daemon};

#[cfg(unix)]
mod unix {
    use super::*;
    use serde_json::Value;
    use std::collections::HashSet;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    /// Id used for the replayed `initialize` after a reconnect; its response
    /// is swallowed by the shim.
    const REPLAY_ID: &str = "__trace_shim_replay__";

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Create (or adopt) a directory only this user can enter. Refuses a
    /// symlink or a directory owned by someone else — a socket there could
    /// be impersonated.
    pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let meta = std::fs::symlink_metadata(dir)?;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        if !meta.is_dir() || meta.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a directory owned by this user", dir.display()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn check_socket_path(socket: &Path) -> io::Result<()> {
        if socket.as_os_str().len() >= MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("socket path too long: {}", socket.display()),
            ));
        }
        match socket.parent() {
            Some(parent) => create_private_dir(parent),
            None => Ok(()),
        }
    }

    /// Decrements the live-connection count (and stamps activity) even if
    /// the connection thread unwinds.
    struct ActiveGuard {
        active: Arc<AtomicUsize>,
        last_activity: Arc<Mutex<Instant>>,
    }

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            *lock(&self.last_activity) = Instant::now();
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Take the project's daemon lock. Service daemons (no idle timeout) wait
    /// for an on-demand daemon to idle out; on-demand daemons defer to a live
    /// daemon but wait briefly for one that is shutting down.
    fn acquire_lock(
        file: &File,
        socket: &Path,
        service_mode: bool,
        root: &Path,
    ) -> io::Result<bool> {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
            Err(std::fs::TryLockError::WouldBlock) => {}
        }
        if service_mode {
            eprintln!(
                "trace daemon: another daemon serves {}; waiting to take over when it exits",
                root.display()
            );
            file.lock()?;
            return Ok(true);
        }
        if UnixStream::connect(socket).is_ok() {
            eprintln!("trace daemon already running for {}", root.display());
            return Ok(false);
        }
        // The holder unlinked its socket: it is draining and about to exit.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            match file.try_lock() {
                Ok(()) => return Ok(true),
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    eprintln!("trace daemon already running for {}", root.display());
                    return Ok(false);
                }
            }
        }
    }

    /// Run the daemon for `root` until killed or idle for `idle_timeout`
    /// (`None`: never — service mode). Returns Ok when another daemon
    /// already serves the project.
    pub fn run_daemon(root: PathBuf, idle_timeout: Option<Duration>) -> io::Result<()> {
        let root = root.canonicalize().unwrap_or(root);
        let paths = paths_for_root(&root);
        create_private_dir(&paths.dir)?;
        check_socket_path(&paths.socket)?;

        let mut lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock)?;
        if !acquire_lock(&lock_file, &paths.socket, idle_timeout.is_none(), &root)? {
            return Ok(());
        }
        lock_file.set_len(0)?;
        writeln!(lock_file, "{}", std::process::id())?;

        // We hold the lock, so any existing socket file is stale.
        let _ = std::fs::remove_file(&paths.socket);
        let listener = UnixListener::bind(&paths.socket)?;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
        eprintln!(
            "trace daemon {} (pid {}) serving {} on {}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            root.display(),
            paths.socket.display()
        );

        let server = Arc::new(Server::new(root));
        warm_up(&server);

        let active = Arc::new(AtomicUsize::new(0));
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        if let Some(timeout) = idle_timeout.filter(|t| !t.is_zero()) {
            let active = Arc::clone(&active);
            let last_activity = Arc::clone(&last_activity);
            let socket = paths.socket.clone();
            std::thread::Builder::new()
                .name("trace-idle".into())
                .spawn(move || loop {
                    std::thread::sleep(
                        (timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(30)),
                    );
                    if active.load(Ordering::SeqCst) == 0
                        && lock(&last_activity).elapsed() >= timeout
                    {
                        eprintln!("trace daemon idle for {}s; exiting", timeout.as_secs());
                        // Unlink first so new clients spawn a successor, give
                        // connections that raced the unlink time to be
                        // accepted, then drain them.
                        let _ = std::fs::remove_file(&socket);
                        std::thread::sleep(Duration::from_millis(200));
                        while active.load(Ordering::SeqCst) > 0 {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        std::process::exit(0);
                    }
                })?;
        }

        // Keep the lock file handle alive for the process lifetime.
        let _lock_guard: File = lock_file;
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("trace daemon: accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            active.fetch_add(1, Ordering::SeqCst);
            *lock(&last_activity) = Instant::now();
            let guard = ActiveGuard {
                active: Arc::clone(&active),
                last_activity: Arc::clone(&last_activity),
            };
            let server = Arc::clone(&server);
            let spawned = std::thread::Builder::new()
                .name("trace-conn".into())
                .spawn(move || {
                    let _guard = guard;
                    let result = stream
                        .try_clone()
                        .and_then(|reader| serve_stream(&server, reader, &stream));
                    if let Err(e) = result {
                        if e.kind() != io::ErrorKind::BrokenPipe {
                            eprintln!("trace daemon: connection ended: {e}");
                        }
                    }
                });
            if let Err(e) = spawned {
                // The closure (and its guard) was dropped, releasing the count.
                eprintln!("trace daemon: cannot spawn connection thread: {e}");
            }
        }
        Ok(())
    }

    fn spawn_daemon(root: &Path) -> io::Result<()> {
        let paths = paths_for_root(root);
        create_private_dir(&paths.dir)?;
        // Keep the log bounded.
        if std::fs::metadata(&paths.log).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
            let _ = std::fs::remove_file(&paths.log);
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log)?;
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .arg("daemon")
            .arg("--idle-timeout")
            .arg(DEFAULT_IDLE_TIMEOUT.as_secs().to_string())
            .arg(root)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            // Own process group: the agent's Ctrl-C / SIGHUP must not kill it.
            .process_group(0)
            .spawn()?;
        // Reap it if it exits while we are alive (e.g. lost the lock race).
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// Make sure a daemon serves `root`, spawning one if necessary.
    pub fn ensure_daemon_running(root: &Path) -> io::Result<()> {
        connect_or_spawn(root).map(drop)
    }

    /// Connect to `root`'s daemon, spawning it and waiting up to 5 s.
    pub fn connect_or_spawn(root: &Path) -> io::Result<UnixStream> {
        let paths = paths_for_root(root);
        check_socket_path(&paths.socket)?;
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            return Ok(stream);
        }
        spawn_daemon(root)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delay = Duration::from_millis(5);
        loop {
            match UnixStream::connect(&paths.socket) {
                Ok(stream) => return Ok(stream),
                Err(e) if Instant::now() >= deadline => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "daemon did not start within 5s ({e}); see {}",
                            paths.log.display()
                        ),
                    ))
                }
                Err(_) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_millis(200));
                }
            }
        }
    }

    /// The canonical form of a JSON-RPC id, used as a map key.
    fn id_key(msg: &Value) -> Option<String> {
        msg.get("id")
            .filter(|id| !id.is_null())
            .map(|id| id.to_string())
    }

    struct ShimState {
        /// Request ids sent and not yet answered. Whoever *removes* an id
        /// owns its single response: the reader (a daemon reply or a
        /// "connection lost" error) or the writer (a resend elsewhere).
        pending: Mutex<HashSet<String>>,
        /// The client's `initialize` request, replayed after a reconnect.
        initialize: Mutex<Option<Value>>,
        output: Mutex<Box<dyn Write + Send>>,
    }

    impl ShimState {
        fn emit(&self, line: &str) -> io::Result<()> {
            let mut out = lock(&self.output);
            write_line(&mut *out, line)
        }

        /// Should a daemon message be forwarded? Responses only when this
        /// connection still owns their id (never twice, never after a resend).
        fn claim_response(&self, parsed: Option<&Value>) -> bool {
            match parsed {
                Some(Value::Array(items)) => {
                    let mut pending = lock(&self.pending);
                    let mut deliver = items.is_empty();
                    for item in items {
                        match id_key(item) {
                            Some(key) => deliver |= pending.remove(&key),
                            None => deliver = true,
                        }
                    }
                    deliver
                }
                Some(msg) => match id_key(msg) {
                    Some(key) if key == format!("\"{REPLAY_ID}\"") => false,
                    Some(key) => lock(&self.pending).remove(&key),
                    None => true,
                },
                None => true,
            }
        }

        /// Take ownership of the ids in `keys` still pending.
        fn claim(&self, keys: &[String]) -> Vec<String> {
            let mut pending = lock(&self.pending);
            keys.iter()
                .filter(|k| pending.remove(*k))
                .cloned()
                .collect()
        }
    }

    struct DaemonConn {
        writer: UnixStream,
        alive: Arc<AtomicBool>,
        reader: std::thread::JoinHandle<()>,
    }

    impl DaemonConn {
        fn start(stream: UnixStream, state: Arc<ShimState>) -> io::Result<Self> {
            let alive = Arc::new(AtomicBool::new(true));
            let reader_stream = stream.try_clone()?;
            let reader_alive = Arc::clone(&alive);
            let reader = std::thread::Builder::new()
                .name("trace-shim-reader".into())
                .spawn(move || {
                    let mut reader = BufReader::new(reader_stream);
                    let mut buf = Vec::new();
                    while let Ok(true) = read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES) {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let parsed: Option<Value> = serde_json::from_str(line).ok();
                        if state.claim_response(parsed.as_ref()) && state.emit(line).is_err() {
                            break;
                        }
                    }
                    // Mark dead first, then fail whatever is still ours.
                    reader_alive.store(false, Ordering::SeqCst);
                    let orphaned: Vec<String> = lock(&state.pending).drain().collect();
                    for key in orphaned {
                        let id: Value = serde_json::from_str(&key).unwrap_or(Value::Null);
                        let resp = crate::mcp::error_response(
                            id,
                            crate::mcp::INTERNAL_ERROR,
                            "trace daemon connection lost while handling this request; please retry",
                        );
                        let _ = state.emit(&resp.to_string());
                    }
                })?;
            Ok(DaemonConn {
                writer: stream,
                alive,
                reader,
            })
        }

        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        fn send(&mut self, line: &str) -> io::Result<()> {
            write_line(&mut self.writer, line)
        }

        /// Stop sending and wait for the reader to deliver (or fail) what is
        /// still in flight.
        fn close(self) {
            let _ = self.writer.shutdown(std::net::Shutdown::Write);
            let _ = self.reader.join();
        }
    }

    /// Lines from the client, decoded lossily (invalid UTF-8 must not end
    /// the session).
    struct InputLines<R> {
        reader: R,
        buf: Vec<u8>,
    }

    impl<R: BufRead> Iterator for InputLines<R> {
        type Item = io::Result<String>;

        fn next(&mut self) -> Option<Self::Item> {
            match read_message(&mut self.reader, &mut self.buf, MAX_MESSAGE_BYTES) {
                Ok(true) => Some(Ok(String::from_utf8_lossy(&self.buf).into_owned())),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            }
        }
    }

    fn reconnect(
        connect: &mut dyn FnMut() -> io::Result<UnixStream>,
        state: &Arc<ShimState>,
    ) -> io::Result<DaemonConn> {
        let stream = connect()?;
        let mut conn = DaemonConn::start(stream, Arc::clone(state))?;
        let init = lock(&state.initialize).clone();
        if let Some(mut init) = init {
            init["id"] = Value::String(REPLAY_ID.into());
            conn.send(&init.to_string())?;
            conn.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
        }
        Ok(conn)
    }

    /// Finish the session in-process after the daemon became unreachable.
    fn serve_rest_inline(
        server: Server,
        state: &ShimState,
        current: Option<String>,
        lines: impl Iterator<Item = io::Result<String>>,
    ) -> io::Result<()> {
        eprintln!("trace: daemon unreachable; continuing inline");
        let mut session = Session::default();
        if let Some(init) = lock(&state.initialize).clone() {
            server.handle_message(&mut session, &init.to_string());
        }
        for line in current.into_iter().map(Ok).chain(lines) {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(resp) = server.handle_message(&mut session, line) {
                state.emit(&resp)?;
            }
        }
        Ok(())
    }

    /// `trace serve` over a daemon connection: pipe stdin → daemon and
    /// daemon → stdout, surviving daemon restarts.
    pub fn proxy(root: PathBuf, stream: UnixStream) -> io::Result<()> {
        let connect_root = root.clone();
        let stdin = io::stdin();
        run_proxy(
            stream,
            move || connect_or_spawn(&connect_root),
            stdin.lock(),
            Box::new(io::stdout()),
            move || Server::new(root),
        )
    }

    pub(super) fn run_proxy(
        first: UnixStream,
        mut connect: impl FnMut() -> io::Result<UnixStream>,
        input: impl BufRead,
        output: Box<dyn Write + Send>,
        inline_server: impl FnOnce() -> Server,
    ) -> io::Result<()> {
        let state = Arc::new(ShimState {
            pending: Mutex::new(HashSet::new()),
            initialize: Mutex::new(None),
            output: Mutex::new(output),
        });
        let mut conn = Some(DaemonConn::start(first, Arc::clone(&state))?);
        let mut lines = InputLines {
            reader: input,
            buf: Vec::new(),
        };
        while let Some(line) = lines.next() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Option<Value> = serde_json::from_str(trimmed).ok();
            let mut keys = Vec::new();
            match &parsed {
                Some(Value::Array(items)) => keys.extend(
                    items
                        .iter()
                        .filter(|m| m.get("method").is_some())
                        .filter_map(id_key),
                ),
                Some(msg) => {
                    if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                        *lock(&state.initialize) = Some(msg.clone());
                    }
                    if msg.get("method").is_some() {
                        keys.extend(id_key(msg));
                    }
                }
                None => {}
            }
            lock(&state.pending).extend(keys.iter().cloned());

            let sent = conn
                .as_mut()
                .is_some_and(|c| c.is_alive() && c.send(trimmed).is_ok());
            if sent {
                continue;
            }
            // The daemon went away. Claim this message's ids before the old
            // reader fails them; ids it already failed are not resent.
            let claimed = state.claim(&keys);
            let resend = keys.is_empty() || !claimed.is_empty();
            if let Some(old) = conn.take() {
                old.close();
            }
            let current = resend.then(|| trimmed.to_string());
            let mut fresh = match reconnect(&mut connect, &state) {
                Ok(c) => c,
                Err(_) => return serve_rest_inline(inline_server(), &state, current, lines),
            };
            if resend {
                lock(&state.pending).extend(claimed.iter().cloned());
                if fresh.send(trimmed).is_err() {
                    let still_ours = state.claim(&claimed);
                    fresh.close();
                    let current =
                        (keys.is_empty() || !still_ours.is_empty()).then(|| trimmed.to_string());
                    return serve_rest_inline(inline_server(), &state, current, lines);
                }
            }
            conn = Some(fresh);
        }
        // Input closed: let the daemon finish outstanding replies, then exit.
        if let Some(conn) = conn {
            conn.close();
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn daemon_serves_multiple_persistent_connections() {
            let home = tempfile::TempDir::new().unwrap();
            let project = tempfile::TempDir::new().unwrap();
            std::fs::create_dir(project.path().join(".git")).unwrap();
            std::fs::write(
                project.path().join("lib.rs"),
                "pub fn hello() { world(); }\nfn world() {}\n",
            )
            .unwrap();
            // Point TRACE_HOME at a temp dir only through the computed paths:
            // run the daemon on a thread with explicit paths.
            let root = project.path().canonicalize().unwrap();
            let socket = home.path().join("d.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = Arc::new(Server::new(root));
            let srv = Arc::clone(&server);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let srv = Arc::clone(&srv);
                    std::thread::spawn(move || {
                        let reader = stream.try_clone().unwrap();
                        let _ = serve_stream(&srv, reader, &stream);
                    });
                }
            });

            let mut clients: Vec<(UnixStream, BufReader<UnixStream>)> = (0..2)
                .map(|_| {
                    let s = UnixStream::connect(&socket).unwrap();
                    let r = BufReader::new(s.try_clone().unwrap());
                    (s, r)
                })
                .collect();
            for (i, (writer, reader)) in clients.iter_mut().enumerate() {
                write_line(writer, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).unwrap();
                write_line(
                    writer,
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                )
                .unwrap();
                write_line(writer, &format!(r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"find_callers","arguments":{{"symbol":"world"}}}}}}"#, 10 + i)).unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.contains("\"protocolVersion\""), "{line}");
                line.clear();
                reader.read_line(&mut line).unwrap();
                let resp: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(resp["id"], 10 + i);
                let text = resp["result"]["content"][0]["text"].as_str().unwrap();
                assert!(text.contains("\"caller\":\"hello\""), "{text}");
            }
        }

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedBuf {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                lock(&self.0).extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn shim_answers_each_request_exactly_once_across_daemon_death() {
            for _ in 0..20 {
                let dir = tempfile::TempDir::new().unwrap();
                std::fs::create_dir(dir.path().join(".git")).unwrap();
                let socket = dir.path().join("fake.sock");
                let listener = UnixListener::bind(&socket).unwrap();
                // Fake daemon: the first connection answers `initialize`, reads
                // request 2 and dies without answering; later connections
                // answer every request.
                std::thread::spawn(move || {
                    let mut first = true;
                    for stream in listener.incoming().flatten() {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut writer = stream;
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 {
                            let msg: Value = serde_json::from_str(line.trim()).unwrap();
                            line.clear();
                            let Some(id) = msg.get("id").cloned() else {
                                continue;
                            };
                            if first && id == 2 {
                                break;
                            }
                            let resp =
                                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}});
                            if write_line(&mut writer, &resp.to_string()).is_err() {
                                break;
                            }
                        }
                        first = false;
                    }
                });
                let input = concat!(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
                    "\n",
                );
                let out = SharedBuf::default();
                let socket_path = socket.clone();
                let root = dir.path().to_path_buf();
                run_proxy(
                    UnixStream::connect(&socket).unwrap(),
                    move || UnixStream::connect(&socket_path),
                    input.as_bytes(),
                    Box::new(out.clone()),
                    move || Server::new(root),
                )
                .unwrap();
                let text = String::from_utf8(lock(&out.0).clone()).unwrap();
                let mut ids: Vec<i64> = text
                    .lines()
                    .map(|l| {
                        serde_json::from_str::<Value>(l).unwrap()["id"]
                            .as_i64()
                            .unwrap()
                    })
                    .collect();
                ids.sort();
                assert_eq!(
                    ids,
                    vec![1, 2, 3, 4],
                    "each request answered exactly once:\n{text}"
                );
                assert!(!text.contains(REPLAY_ID));
            }
        }

        #[test]
        fn private_dirs_are_tightened_and_foreign_dirs_refused() {
            let dir = tempfile::TempDir::new().unwrap();
            let sockets = dir.path().join("sockets");
            std::fs::create_dir(&sockets).unwrap();
            std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o777)).unwrap();
            create_private_dir(&sockets).unwrap();
            let mode = std::fs::metadata(&sockets).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700);
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&sockets, &link).unwrap();
            assert!(create_private_dir(&link).is_err(), "symlinks are refused");
        }

        #[test]
        fn large_responses_are_not_truncated() {
            let project = tempfile::TempDir::new().unwrap();
            std::fs::create_dir(project.path().join(".git")).unwrap();
            let mut code = String::new();
            for i in 0..3000 {
                code.push_str(&format!(
                    "pub fn function_with_a_long_name_{i}(argument: u64) -> u64 {{ argument }}\n"
                ));
            }
            std::fs::write(project.path().join("big.rs"), &code).unwrap();
            let server = Server::new(project.path().to_path_buf());
            let input = concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_symbol_outline","arguments":{"path":"big.rs"}}}"#,
                "\n"
            );
            let mut output = Vec::new();
            serve_stream(&server, input.as_bytes(), &mut output).unwrap();
            assert!(
                output.len() > 256 * 1024,
                "response is {} bytes",
                output.len()
            );
            let resp: Value = serde_json::from_slice(&output).unwrap();
            let text = resp["result"]["content"][0]["text"].as_str().unwrap();
            let outline: Value = serde_json::from_str(text).unwrap();
            assert_eq!(outline["symbols"].as_array().unwrap().len(), 3000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids_are_stable_and_distinct() {
        let a = project_id(Path::new("/nonexistent/a"));
        assert_eq!(a, project_id(Path::new("/nonexistent/a")));
        assert_ne!(a, project_id(Path::new("/nonexistent/b")));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn socket_paths_are_versioned_and_bounded() {
        let paths = paths_for_root(Path::new("/nonexistent/project"));
        let name = paths
            .socket
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.contains(env!("CARGO_PKG_VERSION")), "{name}");
        assert!(paths.socket.as_os_str().len() < 108);
    }

    #[test]
    fn stream_loop_handles_notifications_blank_lines_and_parse_errors() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(project.path().join(".git")).unwrap();
        let server = Server::new(project.path().to_path_buf());
        let input = "\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{oops\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n";
        let mut output = Vec::new();
        serve_stream(&server, input.as_bytes(), &mut output).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&output).unwrap().lines().collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("-32700"));
        assert!(lines[1].contains("\"id\":7"));
    }

    #[test]
    fn oversized_messages_are_rejected() {
        let mut reader = BufReader::new(&b"0123456789\n"[..]);
        let mut buf = Vec::new();
        let err = read_message(&mut reader, &mut buf, 4).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
