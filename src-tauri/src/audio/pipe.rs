use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

/// Maximum diagnostic bytes retained from a child's stderr. The tail is kept
/// so a chatty child cannot grow the buffer without bound.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Drains a child's stderr concurrently into a bounded buffer so a chatty
/// child can never block on a full pipe while we wait for it to exit. The
/// drain task is aborted when this is dropped; the child process ownership
/// (kill_on_drop, explicit kill + wait) stays with the caller.
pub struct StderrDrain {
    buf: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl StderrDrain {
    /// Take ownership of the child's stderr and begin draining it.
    pub fn start(mut stderr: tokio::process::ChildStderr) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let buf_clone = buf.clone();
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut guard = buf_clone.lock().await;
                        guard.extend_from_slice(&chunk[..n]);
                        if guard.len() > MAX_STDERR_BYTES {
                            let excess = guard.len() - MAX_STDERR_BYTES;
                            guard.drain(..excess);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { buf, task }
    }

    /// Current drained stderr content as a lossy string (best-effort tail).
    pub async fn text(&self) -> String {
        let guard = self.buf.lock().await;
        String::from_utf8_lossy(&guard).to_string()
    }
}

impl Drop for StderrDrain {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Explicitly kill and reap a child process on a known error path (stdin
/// write error, stdout read error, timeout, cancellation). The caller must
/// use this whenever the child may still be running; `.kill_on_drop(true)` is
/// only emergency defense-in-depth, never the normal cleanup path.
pub async fn terminate_child(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Tri-state lifecycle of a native child process (RC-2B):
/// `NotStarted -> Running -> Reaped`.
///
/// - `NotStarted`: no child has been spawned yet.
/// - `Running`: a child has been spawned and may be alive.
/// - `Reaped`: the last child has been conclusively killed (if needed) AND
///   waited — the OS-level zombie is gone.
///
/// The slot/evidence owner waits on `wait()` before resolving ownership, so
/// a force-abort of the owning async task cannot release the recording slot
/// or clean evidence while a child may still be running.
#[derive(Clone, Default)]
pub struct ProcessCompletion {
    inner: Arc<tokio::sync::Notify>,
    state: Arc<std::sync::atomic::AtomicU8>,
}

// 0 = NotStarted (the default), 1 = Running, 2 = Reaped.
const PROC_RUNNING: u8 = 1;
const PROC_REAPED: u8 = 2;

impl ProcessCompletion {
    /// Mark that a child is about to be / has been spawned. MUST be called
    /// before `Command::spawn` with no await in between (RC-2B), so a
    /// spawn-blocked or spawn-failed state is still visible to owners.
    pub fn mark_running(&self) {
        self.state.store(PROC_RUNNING, Ordering::SeqCst);
    }

    /// Whether a child may currently be alive (spawned but not reaped).
    pub fn running(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PROC_RUNNING
    }

    /// Whether the child has been conclusively killed + waited.
    pub fn reaped(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PROC_REAPED
    }

    /// Wait until the child is conclusively Reaped. Signalled exactly once;
    /// the signal is stored, so a late waiter still completes immediately.
    pub async fn wait(&self) {
        loop {
            if self.reaped() {
                return;
            }
            let notified = self.inner.notified();
            if self.reaped() {
                return;
            }
            notified.await;
        }
    }

    /// Mark the child Reaped (after kill + wait completed on a controlled
    /// path, or after the detached reap task finished).
    pub fn signal_reaped(&self) {
        self.state.store(PROC_REAPED, Ordering::SeqCst);
        self.inner.notify_one();
    }
}

/// Spawn a task on the current tokio runtime if available, else on a plain
/// thread. Used by `ChildProcessGuard::drop`, which can run during task
/// cancellation (still on the runtime) or during shutdown (possibly off it).
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(fut);
        }
        Err(_) => {
            std::thread::spawn(|| {
                // Block on the future with a fresh runtime.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(fut);
                }
            });
        }
    }
}

/// RAII ownership of a native child process (RC-2B). Wraps the child so that
/// on EVERY exit path — including a force-abort of the owning async task that
/// bypasses the normal continuation — the child is conclusively killed AND
/// waited (reaped) before `ProcessCompletion` signals `Reaped`. Controlled
/// paths call `mark_reaped()` after their own kill+wait (or after a normal
/// `wait()`), so `Drop` does not re-kill. `kill_on_drop(true)` on the child
/// remains only as emergency defense-in-depth; the guard is the authoritative
/// ownership mechanism.
pub struct ChildProcessGuard {
    child: Option<tokio::process::Child>,
    completion: ProcessCompletion,
    reaped: bool,
}

impl ChildProcessGuard {
    /// Take ownership of a freshly spawned child. The completion MUST already
    /// be `mark_running()`.
    pub fn new(child: tokio::process::Child, completion: ProcessCompletion) -> Self {
        Self {
            child: Some(child),
            completion,
            reaped: false,
        }
    }

    /// Mutable access to the child for `wait()`, `kill()`, `take()` etc.
    pub fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("child still owned")
    }

    /// Controlled reaping: the caller already killed (if needed) AND waited
    /// this child. Prevents a redundant kill in `Drop` and signals Reaped.
    pub fn mark_reaped(&mut self) {
        self.reaped = true;
        self.completion.signal_reaped();
    }

    /// Controlled kill + wait + mark Reaped (timeout/cancellation paths).
    pub async fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.mark_reaped();
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        if self.reaped {
            // Already conclusively reaped on a controlled path.
            self.completion.signal_reaped();
            return;
        }
        if let Some(mut child) = self.child.take() {
            // The owning task is being dropped without a controlled reap
            // (e.g. force-abort). Kill + wait on a detached task and signal
            // Reaped ONLY after the wait completes — ownership never resolves
            // while the child may still be alive (RC-2B).
            let completion = self.completion.clone();
            spawn_detached(async move {
                let _ = child.kill().await;
                let _ = child.wait().await;
                completion.signal_reaped();
            });
        } else {
            // No child left (already waited/taken) — nothing to reap.
            self.completion.signal_reaped();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stderr_drain_collects_output() {
        // Use a tokio child that writes to stderr, then exits.
        let mut child = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/c", "echo drain-me 1>&2"]
            } else {
                vec!["-c", "echo drain-me >&2"]
            })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let drain = child.stderr.take().map(StderrDrain::start);
        let status = child.wait().await.unwrap();
        assert!(status.success());

        let text = match drain.as_ref() {
            Some(d) => d.text().await,
            None => String::new(),
        };
        assert!(
            text.contains("drain-me"),
            "expected drained stderr to contain output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn stderr_drain_bounded() {
        let mut child = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/c", "for /l %i in (1,1,500) do @echo long-line-%i 1>&2"]
            } else {
                vec![
                    "-c",
                    "i=0; while [ $i -lt 500 ]; do echo long-line-$i >&2; i=$((i+1)); done",
                ]
            })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let drain = child.stderr.take().map(StderrDrain::start);
        let status = child.wait().await.unwrap();
        assert!(status.success());

        let text = match drain.as_ref() {
            Some(d) => d.text().await,
            None => String::new(),
        };
        assert!(
            text.len() <= MAX_STDERR_BYTES,
            "buffer should be bounded, got {} bytes",
            text.len()
        );
        // The tail should include the last lines.
        assert!(text.contains("long-line-499"));
    }

    /// RC-2B: the process lifecycle is explicitly tri-state — NotStarted ->
    /// Running -> Reaped — and `mark_running` is the transition that makes an
    /// in-flight child visible to slot/evidence owners.
    #[test]
    fn process_completion_tri_state() {
        let completion = ProcessCompletion::default();
        assert!(!completion.running());
        assert!(!completion.reaped());

        completion.mark_running();
        assert!(completion.running());
        assert!(!completion.reaped());

        completion.signal_reaped();
        assert!(!completion.running());
        assert!(completion.reaped());
    }

    fn sleep_command() -> tokio::process::Command {
        if cfg!(windows) {
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.args(["/c", "ping -n 30 127.0.0.1 >nul"]);
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", "sleep 30"]);
            cmd
        }
    }

    /// RC-2B: dropping the guard WITHOUT a controlled reap must still kill +
    /// wait the child on a detached task and signal Reaped only afterwards —
    /// ownership never resolves while the child may be alive.
    #[tokio::test]
    async fn child_process_guard_drop_kills_and_waits_before_reaped() {
        let mut cmd = sleep_command();
        cmd.kill_on_drop(true);
        let child = cmd.spawn().unwrap();
        let completion = ProcessCompletion::default();
        completion.mark_running();

        let child_id = child.id();
        let guard = ChildProcessGuard::new(child, completion.clone());
        // Simulate a force-abort of the owning task: the guard is dropped
        // without mark_reaped/terminate.
        drop(guard);

        // The detached reap task must kill + wait and then signal Reaped.
        tokio::time::timeout(std::time::Duration::from_secs(5), completion.wait())
            .await
            .expect("reap must complete in bounded time");
        assert!(completion.reaped());

        // The child must actually be dead (killed + waited).
        let child_id = child_id.expect("spawned child has a pid");
        let mut try_count = 0;
        loop {
            let alive = if cfg!(windows) {
                // tasklist exits 0 even with no matches — parse the output:
                // a CSV row containing the PID means the process is alive.
                let filter = format!("PID eq {child_id}");
                std::process::Command::new("tasklist")
                    .args(["/FI", &filter, "/FO", "CSV", "/NH"])
                    .output()
                    .map(|o| {
                        let out = String::from_utf8_lossy(&o.stdout);
                        out.contains(&format!(",\"{child_id}\","))
                            || out.contains(&format!("\"{child_id}\","))
                            || out.contains(&format!(",{child_id},"))
                    })
                    .unwrap_or(false)
            } else {
                let pid = child_id.to_string();
                std::process::Command::new("ps")
                    .args(["-p", &pid])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            };
            if !alive {
                break;
            }
            try_count += 1;
            if try_count > 20 {
                panic!("child {child_id} still alive after reap");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// RC-2B: a controlled `terminate()` kills + waits + marks Reaped — the
    /// normal timeout/cancellation path.
    #[tokio::test]
    async fn child_process_guard_terminate_marks_reaped() {
        let mut cmd = sleep_command();
        cmd.kill_on_drop(true);
        let child = cmd.spawn().unwrap();
        let completion = ProcessCompletion::default();
        completion.mark_running();

        let mut guard = ChildProcessGuard::new(child, completion.clone());
        assert!(completion.running());
        guard.terminate().await;
        assert!(
            completion.reaped(),
            "controlled terminate must mark Reaped after kill + wait"
        );
        // Dropping after a controlled reap must not re-kill or panic.
        drop(guard);
        assert!(completion.reaped());
    }

    /// RC-2B: a normal `wait()` followed by `mark_reaped()` leaves the
    /// completion Reaped — the success path.
    #[tokio::test]
    async fn child_process_guard_normal_wait_marks_reaped() {
        let mut cmd = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
        cmd.args(if cfg!(windows) {
            vec!["/c", "exit 0"]
        } else {
            vec!["-c", "exit 0"]
        })
        .kill_on_drop(true);
        let child = cmd.spawn().unwrap();
        let completion = ProcessCompletion::default();
        completion.mark_running();

        let mut guard = ChildProcessGuard::new(child, completion.clone());
        let status = guard.child_mut().wait().await.unwrap();
        assert!(status.success());
        guard.mark_reaped();
        assert!(completion.reaped());
        drop(guard);
        assert!(completion.reaped());
    }
}
