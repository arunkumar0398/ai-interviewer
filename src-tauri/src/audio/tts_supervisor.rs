use crate::audio::capture::{BlockingLifecycle, CaptureCompletion};
use crate::audio::pipe::{ChildProcessGuard, ProcessCompletion, StderrDrain};
use crate::audio::playback::{
    await_playback, pcm_bytes_to_samples, PlaybackOutcome, PLAYBACK_TIMEOUT_SECS,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};

/// TTS events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum TtsEvent {
    Speaking { text: String },
    Finished { duration_ms: u64 },
    Error { message: String },
    ProcessCrashed { restarts: u32 },
}

/// Global semaphore that bounds concurrent Piper TTS processes to 1.
/// Held for the duration of a `speak()` call.
static TTS_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));

/// Process-level timeout for Piper TTS. The child process owner enforces this
/// directly — any outer timeout is only defense-in-depth.
const PIPER_TIMEOUT_SECS: u64 = 60;

/// Maximum chunk size for reading Piper stdout in bounded reads.
const STDOUT_CHUNK_SIZE: usize = 8192;

pub struct PiperSupervisor {
    piper_bin: PathBuf,
    model_path: PathBuf,
    sample_rate: u32,
    max_restarts: u32,
}

impl PiperSupervisor {
    pub fn new(paths: &crate::paths::AppPaths) -> anyhow::Result<Self> {
        // Executable, model, AND model config resolve from ONE coherent
        // layout (P1-3): the same resolver readiness uses, so the runtime can
        // never execute a model different from the pair that was validated.
        let piper = crate::paths::resolve_piper(&paths.tool_dir)
            .ok_or_else(|| anyhow::anyhow!("Piper runtime not found"))?;
        if !piper.executable.exists() {
            anyhow::bail!("Piper binary not found at {}", piper.executable.display());
        }
        if !piper.model.exists() {
            anyhow::bail!("Piper model not found at {}", piper.model.display());
        }
        if !piper.model_config.exists() {
            anyhow::bail!(
                "Piper model config not found at {}",
                piper.model_config.display()
            );
        }
        Ok(Self {
            piper_bin: piper.executable,
            model_path: piper.model,
            sample_rate: 22050,
            max_restarts: 3,
        })
    }

    /// Speak text through Piper TTS. Blocks until finished or stop_flag is set.
    /// Acquires the global TTS semaphore to ensure only one Piper runs at a time.
    /// Spawns a new Piper process per call (simple, reliable).
    /// Kills the child process immediately when stop_flag is set or timeout fires.
    /// `output_completion` is the PRODUCTION playback lifecycle signal (RC-1):
    /// it is marked Scheduled immediately before the physical output worker is
    /// submitted and signalled Finished when that worker has fully exited, so
    /// the shared audio slot stays occupied until the real Piper question
    /// playback has physically ended — even if the owning command is dropped
    /// or aborted while the blocking output is inside native audio calls.
    ///
    /// `process_completion` is the CHILD-PROCESS lifecycle signal (RC-2B):
    /// marked Running before every spawn and signalled Reaped only after the
    /// child has been conclusively killed + waited, so a force-abort of this
    /// task can never resolve slot/evidence ownership while a Piper process
    /// may still be alive.
    pub async fn speak(
        &self,
        text: &str,
        event_tx: mpsc::Sender<TtsEvent>,
        stop_flag: Arc<AtomicBool>,
        output_completion: CaptureCompletion,
        process_completion: ProcessCompletion,
    ) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }

        // Acquire TTS semaphore — only one Piper at a time
        let _permit = TTS_SEMAPHORE
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("TTS semaphore closed"))?;

        let piper_bin = self.piper_bin.clone();
        let model_path = self.model_path.clone();
        let sample_rate = self.sample_rate;
        let max_restarts = self.max_restarts;
        let text = text.to_string();

        let mut restarts = 0u32;

        'restart: loop {
            if stop_flag.load(Ordering::SeqCst) {
                return Ok(());
            }

            // RC-2B: mark Running BEFORE spawn (no await in between) so a
            // force-abort can never resolve ownership while the child may be
            // alive.
            process_completion.mark_running();
            // Spawn Piper with tokio async process — stdout/stderr are async
            let child = match Command::new(&piper_bin)
                .arg("--model")
                .arg(&model_path)
                .arg("--output-raw")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    // Spawn failed — nothing was ever alive to reap.
                    process_completion.signal_reaped();
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!("Failed to start Piper: {}", e),
                    });
                    return Err(anyhow::anyhow!("Failed to start Piper: {}", e));
                }
            };
            // The child's kill+wait lifecycle is RAII-owned: on EVERY exit
            // path (including force-abort) the child is reaped before the
            // completion signals Reaped (RC-2B).
            let mut proc_guard = ChildProcessGuard::new(child, process_completion.clone());

            // Drain stderr concurrently so Piper can never block on a full
            // stderr pipe while we consume its stdout PCM.
            let stderr_drain = proc_guard.child_mut().stderr.take().map(StderrDrain::start);

            // ONE absolute deadline for this process lifecycle (P2-4): stdout
            // reads, cancellation, child exit, and the final wait share the
            // budget — EOF does not reset the timeout.
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(PIPER_TIMEOUT_SECS);

            // Write text to stdin
            if let Some(mut stdin) = proc_guard.child_mut().stdin.take() {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(text.as_bytes()).await {
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!("Failed to write to Piper stdin: {}", e),
                    });
                    // Child may still be running — kill and reap explicitly.
                    proc_guard.terminate().await;
                    return Err(anyhow::anyhow!("Stdin write failed: {}", e));
                }
                drop(stdin);
            }

            let _ = event_tx.try_send(TtsEvent::Speaking { text: text.clone() });

            // Read raw PCM from stdout with async I/O and cancellation
            let mut pcm_data = Vec::new();
            let mut stdout = proc_guard.child_mut().stdout.take();
            let mut buf = vec![0u8; STDOUT_CHUNK_SIZE];

            let read_result: anyhow::Result<()> = loop {
                tokio::select! {
                    // Read next chunk from stdout
                    result = async {
                        if let Some(ref mut stdout) = stdout {
                            stdout.read(&mut buf).await
                        } else {
                            // No stdout — should not happen, but treat as EOF
                            Ok(0)
                        }
                    } => {
                        match result {
                            Ok(0) => {
                                // EOF — stdout closed, Piper is done writing
                                break Ok(());
                            }
                            Ok(n) => {
                                pcm_data.extend_from_slice(&buf[..n]);
                            }
                            Err(e) => {
                                break Err(anyhow::anyhow!("Piper stdout read error: {}", e));
                            }
                        }
                    }
                    // Check stop flag — polling wake-up ensures reliable cancellation
                    _ = crate::interview::orchestrator::wait_for_stop(stop_flag.clone()) => {
                        proc_guard.terminate().await;
                        return Ok(());
                    }
                    // Check deadline
                    _ = tokio::time::sleep_until(deadline) => {
                        proc_guard.terminate().await;
                        let _ = event_tx.try_send(TtsEvent::Error {
                            message: format!(
                                "Piper timed out after {}s — process killed",
                                PIPER_TIMEOUT_SECS
                            ),
                        });
                        return Err(anyhow::anyhow!(
                            "Piper timed out after {}s",
                            PIPER_TIMEOUT_SECS
                        ));
                    }
                }
            };

            if let Err(e) = read_result {
                // Child may still be running (stdout read error) — kill and
                // reap explicitly.
                proc_guard.terminate().await;
                let _ = event_tx.try_send(TtsEvent::Error {
                    message: format!("Piper read error: {}", e),
                });
                return Err(e);
            }

            // Wait for process to finish — the SAME absolute deadline (P2-4),
            // so EOF does not grant a fresh timeout budget.
            let wait_result =
                tokio::time::timeout_at(deadline, proc_guard.child_mut().wait()).await;

            match wait_result {
                Ok(Ok(status)) => {
                    // The child exited and was waited — conclusively reaped on
                    // this controlled path (RC-2B).
                    proc_guard.mark_reaped();
                    if status.success() {
                        // Play the collected PCM data. A playback failure must
                        // surface as an error: the candidate did not actually
                        // hear the question, so the round must not continue.
                        let outcome = play_raw_pcm_async(
                            &pcm_data,
                            sample_rate,
                            stop_flag.clone(),
                            output_completion.clone(),
                        )
                        .await
                        .map_err(|e| {
                            let _ = event_tx.try_send(TtsEvent::Error {
                                message: format!("Playback failed: {}", e),
                            });
                            anyhow::anyhow!("Playback failed: {}", e)
                        })?;
                        // P2-4: Finished is only truthful after ACTUAL playback
                        // completion. A stop during playback is Cancelled — the
                        // orchestrator observes the stop flag and aborts the
                        // round; no Finished is emitted.
                        if matches!(outcome, PlaybackOutcome::Completed) {
                            let _ = event_tx.try_send(TtsEvent::Finished { duration_ms: 0 });
                        }
                        return Ok(());
                    }
                    // Non-zero exit — may need restart
                    restarts += 1;
                    let stderr_text = match &stderr_drain {
                        Some(d) => d.text().await,
                        None => String::new(),
                    };
                    if restarts >= max_restarts {
                        let _ = event_tx.try_send(TtsEvent::Error {
                            message: format!(
                                "Piper crashed {} times, giving up: {}",
                                max_restarts,
                                stderr_text.trim()
                            ),
                        });
                        return Err(anyhow::anyhow!(
                            "Piper exceeded max restarts: {}",
                            stderr_text.trim()
                        ));
                    }
                    let _ = event_tx.try_send(TtsEvent::ProcessCrashed { restarts });
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    continue 'restart;
                }
                Ok(Err(e)) => {
                    // Child state is uncertain after a wait error — terminate
                    // and reap explicitly before propagating (P2-2);
                    // kill_on_drop stays only as defense-in-depth.
                    proc_guard.terminate().await;
                    let stderr_text = match &stderr_drain {
                        Some(d) => d.text().await,
                        None => String::new(),
                    };
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!("Piper wait error: {} ({})", e, stderr_text.trim()),
                    });
                    return Err(anyhow::anyhow!("Piper wait error: {}", e));
                }
                Err(_) => {
                    // Timeout waiting for child to exit — force kill and reap.
                    proc_guard.terminate().await;
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!(
                            "Piper timed out after {}s — process killed",
                            PIPER_TIMEOUT_SECS
                        ),
                    });
                    return Err(anyhow::anyhow!(
                        "Piper timed out after {}s",
                        PIPER_TIMEOUT_SECS
                    ));
                }
            }
        }
    }
}

/// Play raw 16-bit PCM from a pre-collected buffer via cpal output device.
/// Returns the distinct outcome (Completed vs Cancelled) so callers only
/// report Finished after actual playback completion (P2-4).
/// Moves the entire cpal Stream lifecycle into spawn_blocking so the
/// !Send Stream never crosses an async await point.
///
/// Lifecycle ownership (RC-1): `output_completion` is the PRODUCTION
/// playback lifecycle signal — `mark_scheduled()` runs immediately BEFORE
/// `spawn_blocking` (no await in between) and the closure signals Finished
/// on EVERY exit path. The shared audio slot therefore stays occupied until
/// the physical output worker has fully terminated, even when the outer
/// interview worker is dropped/aborted while the blocking output is inside
/// native calls (`default_output_device()`, `build_output_stream()`,
/// `stream.play()`) that cannot be force-cancelled.
async fn play_raw_pcm_async(
    pcm_data: &[u8],
    sample_rate: u32,
    stop_flag: Arc<AtomicBool>,
    output_completion: CaptureCompletion,
) -> anyhow::Result<PlaybackOutcome> {
    // Reject empty/malformed PCM BEFORE any device work (P1-3): Piper exiting
    // 0 with zero audio must never be reported as successfully spoken — the
    // candidate heard nothing, so the round must fail, not proceed.
    let samples = pcm_bytes_to_samples(pcm_data)?;

    // RC-1: the production playback becomes `Scheduled` BEFORE spawn_blocking
    // is submitted, with no await in between — a blocking output worker queued
    // on a busy pool still owns the audio slot, so the slot can never be freed
    // while a physical output might start later.
    output_completion.mark_scheduled();

    // Move the entire cpal Stream lifecycle into spawn_blocking so the
    // !Send Stream never crosses an async await point.
    let stop_flag_clone = stop_flag.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<PlaybackOutcome> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        // RC-1: signal the physical output lifecycle on EVERY exit path
        // (success, `?`, bail, panic) — the slot owner waits on this before
        // releasing the shared audio slot.
        let _lifecycle = BlockingLifecycle(output_completion.clone());

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No output device found"))?;

        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples_arc = Arc::new(samples);
        let samples_clone = samples_arc.clone();

        // Actual playback progress, shared between the output callback and the
        // completion wait (P2-3): success requires the callback to consume
        // EVERY sample. Elapsed expected duration alone is never proof of
        // successful playback.
        let position = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pos_clone = position.clone();

        // Latch for asynchronous output-stream errors. The error callback
        // writes here; the playback loop observes it so a device failure after
        // stream.play() succeeds still fails TTS (P1-5). `TtsEvent::Finished`
        // is therefore never emitted after a failed playback.
        let playback_err: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let err_latch = playback_err.clone();
        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let start = pos_clone.load(std::sync::atomic::Ordering::Relaxed);
                for (i, sample) in data.iter_mut().enumerate() {
                    let idx = start + i;
                    *sample = if idx < samples_clone.len() {
                        samples_clone[idx] as f32 / 32768.0
                    } else {
                        0.0 // silence after end
                    };
                }
                pos_clone.fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
            },
            move |err| {
                eprintln!("Output stream error: {}", err);
                let msg = format!("Output stream error: {}", err);
                if let Ok(mut guard) = err_latch.lock() {
                    if guard.is_none() {
                        *guard = Some(msg);
                    }
                }
            },
            None,
        )?;

        stream.play()?;

        // Wait for ACTUAL sample progress to complete, surfacing async device
        // errors as authoritative failures. The absolute deadline bounds a
        // stalled output device (no progress -> Err, never success).
        let total_samples = samples_arc.len();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(PLAYBACK_TIMEOUT_SECS);

        let outcome = await_playback(
            &playback_err,
            Some(&stop_flag_clone),
            deadline,
            &position,
            total_samples,
        )?;

        drop(stream);
        Ok(outcome)
    })
    .await?
}

/// Verify that the coherent Piper runtime (binary, model, config) is present
pub fn verify_piper_installation(paths: &crate::paths::AppPaths) -> anyhow::Result<()> {
    let piper = crate::paths::resolve_piper(&paths.tool_dir)
        .ok_or_else(|| anyhow::anyhow!("Piper runtime not found"))?;
    if !piper.executable.exists() {
        anyhow::bail!("Piper binary not found at {}", piper.executable.display());
    }
    if !piper.model.exists() {
        anyhow::bail!("Piper model not found at {}", piper.model.display());
    }
    if !piper.model_config.exists() {
        anyhow::bail!(
            "Piper model config not found at {}",
            piper.model_config.display()
        );
    }
    Ok(())
}
