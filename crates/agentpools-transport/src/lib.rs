//! Cross-platform stdio child process management and JSON stream parsing.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentpools::CancellationToken;
use serde_json::Value;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[derive(Debug)]
pub enum TransportError {
    Spawn(io::Error),
    Io(io::Error),
    Protocol(String),
    Remote(String),
    Timeout,
    Cancelled,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "cannot spawn process: {err}"),
            Self::Io(err) => write!(f, "transport I/O failed: {err}"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
            Self::Remote(err) => write!(f, "remote error: {err}"),
            Self::Timeout => write!(f, "operation timed out"),
            Self::Cancelled => write!(f, "operation cancelled"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(err) | Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Cross-platform process launch configuration.
#[derive(Clone, Debug)]
pub struct ProcessConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: HashMap<OsString, OsString>,
    pub cwd: PathBuf,
    pub inherit_stderr: bool,
}

impl ProcessConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: cwd.into(),
            inherit_stderr: false,
        }
    }
}

/// Safely terminates a child process across Windows, Linux, and macOS.
///
/// 1. Checks if the child already exited via `try_wait()`.
/// 2. If not, waits up to `grace_period` for clean exit.
/// 3. If still running, forcefully terminates the child, handling platform-specific error quirks.
/// 4. Waits for the child and joins the reader thread.
///
/// Termination covers the whole process tree the child may have grown:
/// on Windows the child is assigned to a kill-on-close Job at spawn;
/// on Unix the child starts as a process-group leader and the group is signalled.
/// Either way a wedged child (or a parent that died without `close`) leaves no orphans.
pub fn terminate_child(
    child: &mut Child,
    reader: Option<JoinHandle<()>>,
    grace_period: Duration,
) -> io::Result<()> {
    if child.try_wait()?.is_none() {
        let deadline = Instant::now() + grace_period;
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !exited {
            force_kill(child)?;
        }
    }
    let _ = child.wait();
    join_reader(reader, Duration::from_secs(2));
    Ok(())
}

/// Join the stdout reader without hanging Drop forever: grandchildren can
/// inherit the stdout pipe and hold it open after the direct child dies
/// (observed: 29s Drop on Windows). Abandoned readers exit on EOF.
fn join_reader(reader: Option<JoinHandle<()>>, timeout: Duration) {
    let Some(handle) = reader else { return };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Still blocked on an inherited pipe: detach. It exits when the pipe
    // closes (process-group kill on Unix, Job close on Windows). Never block
    // Drop/shutdown on it.
}

/// Forcefully terminate a (possibly wedged) child without blocking.
/// Direct-child kill plus whole-tree coverage; already-exited races are ignored.
fn force_kill(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        // Own process group (see `spawn`): signal the group so grandchildren die too.
        let pgid = child.id() as i32;
        if libc_killpg(pgid, 9) == 0 {
            return Ok(());
        }
        // ESRCH (already gone) is fine; anything else falls through to child.kill().
        if libc_killpg(pgid, 0) != 0 {
            return Ok(());
        }
    }
    match child.kill() {
        Ok(()) => Ok(()),
        // On Windows, killing an already exited process may return InvalidInput or PermissionDenied
        Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
unsafe extern "C" {
    /// Minimal libc surface (no new dependency): group signalling only.
    /// `sig == 0` performs existence check without delivering.
    #[link_name = "killpg"]
    safe fn libc_killpg(pgrp: i32, sig: i32) -> i32;
    /// Minimal libc surface (no new dependency): new process-group leader.
    #[link_name = "setsid"]
    safe fn libc_setsid() -> i32;
    /// Minimal libc surface (no new dependency): liveness probe for one pid.
    /// `sig == 0` performs existence check without delivering.
    #[link_name = "kill"]
    safe fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Dedicated JSON-RPC/line-delimited stdio transport for a single child process.
pub struct JsonProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
    reader: Option<JoinHandle<()>>,
    process_id: u32,
    /// Windows containment: open until drop so wedged grandchildren die with us.
    #[cfg(windows)]
    _job: Option<WindowsJob>,
}

/// Windows Job Object with kill-on-close (no new dependency: hand-rolled FFI).
/// The child is assigned right after spawn; if the parent dies, crashes, or
/// never calls `close`, the OS reaps the whole tree. Assignment can fail
/// (e.g. nested-job conflict) — that only falls back to plain spawn.
#[cfg(windows)]
struct WindowsJob {
    handle: *mut core::ffi::c_void,
}

#[cfg(windows)]
mod win_job {
    use super::WindowsJob;

    type Handle = *mut core::ffi::c_void;
    type Dword = u32;
    type Bool = i32;

    #[repr(C)]
    struct BasicLimitInfo {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: Dword,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: Dword,
        affinity: usize,
        priority_class: Dword,
        scheduling_class: Dword,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInfo {
        basic: BasicLimitInfo,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: Dword = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: Dword = 0x2000;

    // ABI locks: the OS validates buffer length exactly.
    const _: [(); 64] = [(); size_of::<BasicLimitInfo>()];
    const _: [(); 48] = [(); size_of::<IoCounters>()];
    const _: [(); 144] = [(); size_of::<ExtendedLimitInfo>()];

    unsafe extern "system" {
        safe fn CreateJobObjectW(attributes: *const core::ffi::c_void, name: *const u16) -> Handle;
        safe fn SetInformationJobObject(
            job: Handle,
            info_class: Dword,
            info: *const core::ffi::c_void,
            info_len: Dword,
        ) -> Bool;
        safe fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        safe fn CloseHandle(object: Handle) -> Bool;
    }

    impl WindowsJob {
        pub(super) fn contain(process: Handle) -> Option<Self> {
            // Raw job/create/assign syscalls with valid local buffers; null handles checked.
            let job = CreateJobObjectW(core::ptr::null(), core::ptr::null());
            if job.is_null() {
                return None;
            }
            let info = ExtendedLimitInfo {
                basic: BasicLimitInfo {
                    per_process_user_time_limit: 0,
                    per_job_user_time_limit: 0,
                    limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    minimum_working_set_size: 0,
                    maximum_working_set_size: 0,
                    active_process_limit: 0,
                    affinity: 0,
                    priority_class: 0,
                    scheduling_class: 0,
                },
                io: IoCounters {
                    read_operation_count: 0,
                    write_operation_count: 0,
                    other_operation_count: 0,
                    read_transfer_count: 0,
                    write_transfer_count: 0,
                    other_transfer_count: 0,
                },
                process_memory_limit: 0,
                job_memory_limit: 0,
                peak_process_memory_used: 0,
                peak_job_memory_used: 0,
            };
            // Best-effort containment: nested-job conflicts fall back to plain spawn.
            if SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&raw const info).cast(),
                size_of::<ExtendedLimitInfo>() as Dword,
            ) == 0
            {
                CloseHandle(job);
                return None;
            }
            if AssignProcessToJobObject(job, process) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self { handle: job })
        }
    }

    impl Drop for WindowsJob {
        fn drop(&mut self) {
            CloseHandle(self.handle);
        }
    }

    // Handles are only closed on drop; never shared across threads unsafely.
    unsafe impl Send for WindowsJob {}
}

impl JsonProcess {
    pub fn spawn(config: &ProcessConfig) -> Result<Self, TransportError> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .current_dir(&config.cwd)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if config.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            });

        // Unix: own process group so `force_kill` reaps grandchildren too.
        // A forked child is never a group leader, so setsid cannot fail here;
        // a failure aborts spawn loudly instead of mis-signalling our own group.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc_setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(TransportError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Protocol("missing child stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Protocol("missing child stdout pipe".into()))?;
        let process_id = child.id();

        // Windows: contain the child tree so crashes/wedges without `close`
        // leave no orphans. Best-effort: nested-job conflicts fall back to plain spawn.
        #[cfg(windows)]
        let contained = {
            use std::os::windows::io::AsRawHandle;
            WindowsJob::contain(child.as_raw_handle())
        };

        let (sender, incoming) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("agentpools-transport-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let parsed = serde_json::from_str::<Value>(trimmed)
                                .map_err(|err| format!("invalid JSON record: {err}"));
                            if sender.send(parsed).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = sender.send(Err(format!("reading child stdout: {err}")));
                            break;
                        }
                    }
                }
            })
            .map_err(TransportError::Spawn)?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming,
            pending: VecDeque::new(),
            reader: Some(reader),
            process_id,
            #[cfg(windows)]
            _job: contained,
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn send(&mut self, message: &Value) -> Result<(), TransportError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| TransportError::Protocol("transport stdin is closed".into()))?;
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|err| TransportError::Protocol(format!("encoding request failed: {err}")))?;
        stdin.write_all(b"\n").map_err(TransportError::Io)?;
        stdin.flush().map_err(TransportError::Io)
    }

    /// Read any incoming message within the given duration.
    /// Returns `None` if timeout expires without a message.
    pub fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, TransportError> {
        if let Some(msg) = self.pending.pop_front() {
            return Ok(Some(msg));
        }
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(msg)) => Ok(Some(msg)),
            Ok(Err(err)) => Err(TransportError::Protocol(err)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(TransportError::Protocol("process stdout closed".into()))
            }
        }
    }

    /// Read the next event, prioritizing pending queued items, with cancellation support.
    pub fn next_event(
        &mut self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Value, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        if let Some(msg) = self.pending.pop_front() {
            return Ok(msg);
        }
        self.receive_fresh(deadline, Some(cancellation))
    }

    /// Blocks until a new message arrives from the child stdout, respecting deadline and cancellation.
    pub fn receive_fresh(
        &mut self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(TransportError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Timeout);
            }
            match self
                .incoming
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(Ok(msg)) => return Ok(msg),
                Ok(Err(err)) => return Err(TransportError::Protocol(err)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(TransportError::Protocol("process stdout closed".into()));
                }
            }
        }
    }

    /// Await a response matching `id`, queuing intermediate notifications.
    pub fn response(
        &mut self,
        id: u64,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        loop {
            let message = self.receive_fresh(deadline, cancellation)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(TransportError::Remote(error.to_string()));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| TransportError::Protocol("response has no result".into()));
            }
            self.pending.push_back(message);
            if self.pending.len() > 4096 {
                return Err(TransportError::Protocol("too many pending events".into()));
            }
        }
    }

    /// Closes stdin (sending EOF to process), then waits up to `grace_period` before force killing.
    pub fn close(&mut self) -> Result<(), TransportError> {
        self.stdin.take();
        // Windows: release containment first so kill-on-close reaps
        // grandchildren before we join the stdout reader (they may hold the
        // pipe open and stall the join).
        #[cfg(windows)]
        drop(self._job.take());
        terminate_child(
            &mut self.child,
            self.reader.take(),
            Duration::from_millis(500),
        )
        .map_err(TransportError::Io)
    }

    /// Terminate immediately with custom grace period.
    pub fn terminate(&mut self, grace_period: Duration) -> Result<(), TransportError> {
        self.stdin.take();
        #[cfg(windows)]
        drop(self._job.take());
        terminate_child(&mut self.child, self.reader.take(), grace_period)
            .map_err(TransportError::Io)
    }
}

impl Drop for JsonProcess {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_config_creation() {
        let config = ProcessConfig::new("node", ".");
        assert_eq!(config.program, PathBuf::from("node"));
        assert_eq!(config.cwd, PathBuf::from("."));
        assert!(!config.inherit_stderr);
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    /// Portable sleeper config for lifecycle tests (no external tools assumed).
    #[cfg(test)]
    fn sleeper_config() -> ProcessConfig {
        let mut config = if cfg!(windows) {
            ProcessConfig::new("cmd", ".")
        } else {
            ProcessConfig::new("sleep", ".")
        };
        if cfg!(windows) {
            config.args = vec!["/C".into(), "ping -n 30 127.0.0.1 >nul".into()];
        } else {
            config.args = vec!["30".into()];
        }
        config
    }

    #[cfg(test)]
    fn pid_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            libc_kill(pid as i32, 0) == 0
        }
        #[cfg(windows)]
        {
            // Query-then-close a process handle; null handle means gone.
            let handle = open_process(QUERY_LIMITED, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = exit_code(handle, &mut code) != 0;
            close_handle(handle);
            ok && code == STILL_ACTIVE
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            true
        }
    }

    #[cfg(all(test, windows))]
    mod win_pid {
        pub(super) type Handle = *mut core::ffi::c_void;
        pub(super) const STILL_ACTIVE: u32 = 259;
        pub(super) const QUERY_LIMITED: u32 = 0x1000;
        unsafe extern "system" {
            #[link_name = "OpenProcess"]
            pub(super) safe fn open_process(access: u32, inherit: i32, pid: u32) -> Handle;
            #[link_name = "GetExitCodeProcess"]
            pub(super) safe fn exit_code(handle: Handle, code: *mut u32) -> i32;
            #[link_name = "CloseHandle"]
            pub(super) safe fn close_handle(handle: Handle) -> i32;
        }
    }

    #[cfg(all(test, windows))]
    use win_pid::{QUERY_LIMITED, STILL_ACTIVE, close_handle, exit_code, open_process};

    #[cfg(test)]
    fn wait_gone(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !pid_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        !pid_alive(pid)
    }

    #[test]
    fn drop_without_close_leaves_no_orphan() {
        // Production spawn path (pipes + reader + containment), never closed.
        let process = JsonProcess::spawn(&sleeper_config()).expect("sleeper spawns");
        let pid = process.process_id();
        assert!(pid_alive(pid), "sleeper must be alive before drop");
        #[cfg(windows)]
        assert!(
            process._job.is_some(),
            "job containment must be established at spawn"
        );
        // Never call close(): Drop alone must reap the child, fast: the
        // sleeper outlives any correct kill by an order of magnitude, so a
        // slow Drop means the kill path is vacuous.
        let started = Instant::now();
        drop(process);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "Drop took {:?}, kill path looks vacuous",
            started.elapsed()
        );
        assert!(
            wait_gone(pid, Duration::from_secs(10)),
            "child {pid} survived Drop without close"
        );
    }

    #[test]
    fn test_transport_error_display() {
        let err = TransportError::Protocol("test error".into());
        assert!(err.to_string().contains("test error"));
        let err = TransportError::Cancelled;
        assert_eq!(err.to_string(), "operation cancelled");
    }
}
