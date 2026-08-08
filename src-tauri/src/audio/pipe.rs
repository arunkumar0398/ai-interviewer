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
}
