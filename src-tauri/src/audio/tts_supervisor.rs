use crate::audio::pipe::{terminate_child, StderrDrain};
use crate::audio::playback::await_playback;
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
        let tools = crate::paths::resolve_tools(&paths.tool_dir);
        let piper_bin = tools
            .piper_bin
            .ok_or_else(|| anyhow::anyhow!("Piper binary not found"))?;
        let model_path = tools
            .piper_model
            .ok_or_else(|| anyhow::anyhow!("Piper model not found"))?;
        if !piper_bin.exists() {
            anyhow::bail!("Piper binary not found at {}", piper_bin.display());
        }
        if !model_path.exists() {
            anyhow::bail!("Piper model not found at {}", model_path.display());
        }
        Ok(Self {
            piper_bin,
            model_path,
            sample_rate: 22050,
            max_restarts: 3,
        })
    }

    /// Speak text through Piper TTS. Blocks until finished or stop_flag is set.
    /// Acquires the global TTS semaphore to ensure only one Piper runs at a time.
    /// Spawns a new Piper process per call (simple, reliable).
    /// Kills the child process immediately when stop_flag is set or timeout fires.
    pub async fn speak(
        &self,
        text: &str,
        event_tx: mpsc::Sender<TtsEvent>,
        stop_flag: Arc<AtomicBool>,
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

            // Spawn Piper with tokio async process — stdout/stderr are async
            let mut child = match Command::new(&piper_bin)
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
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!("Failed to start Piper: {}", e),
                    });
                    return Err(anyhow::anyhow!("Failed to start Piper: {}", e));
                }
            };

            // Drain stderr concurrently so Piper can never block on a full
            // stderr pipe while we consume its stdout PCM.
            let stderr_drain = child.stderr.take().map(StderrDrain::start);

            // ONE absolute deadline for this process lifecycle (P2-4): stdout
            // reads, cancellation, child exit, and the final wait share the
            // budget — EOF does not reset the timeout.
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(PIPER_TIMEOUT_SECS);

            // Write text to stdin
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(text.as_bytes()).await {
                    let _ = event_tx.try_send(TtsEvent::Error {
                        message: format!("Failed to write to Piper stdin: {}", e),
                    });
                    // Child may still be running — kill and reap explicitly.
                    terminate_child(&mut child).await;
                    return Err(anyhow::anyhow!("Stdin write failed: {}", e));
                }
                drop(stdin);
            }

            let _ = event_tx.try_send(TtsEvent::Speaking { text: text.clone() });

            // Read raw PCM from stdout with async I/O and cancellation
            let mut pcm_data = Vec::new();
            let mut stdout = child.stdout.take();
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
                        terminate_child(&mut child).await;
                        return Ok(());
                    }
                    // Check deadline
                    _ = tokio::time::sleep_until(deadline) => {
                        terminate_child(&mut child).await;
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
                terminate_child(&mut child).await;
                let _ = event_tx.try_send(TtsEvent::Error {
                    message: format!("Piper read error: {}", e),
                });
                return Err(e);
            }

            // Wait for process to finish — the SAME absolute deadline (P2-4),
            // so EOF does not grant a fresh timeout budget.
            let wait_result = tokio::time::timeout_at(deadline, child.wait()).await;

            match wait_result {
                Ok(Ok(status)) => {
                    if status.success() {
                        // Play the collected PCM data. A playback failure must
                        // surface as an error: the candidate did not actually
                        // hear the question, so the round must not continue.
                        if let Err(e) =
                            play_raw_pcm_async(&pcm_data, sample_rate, stop_flag.clone()).await
                        {
                            let _ = event_tx.try_send(TtsEvent::Error {
                                message: format!("Playback failed: {}", e),
                            });
                            return Err(anyhow::anyhow!("Playback failed: {}", e));
                        }
                        let _ = event_tx.try_send(TtsEvent::Finished { duration_ms: 0 });
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
                    terminate_child(&mut child).await;
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
/// Moves the entire cpal Stream lifecycle into spawn_blocking so the
/// !Send Stream never crosses an async await point.
async fn play_raw_pcm_async(
    pcm_data: &[u8],
    sample_rate: u32,
    stop_flag: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // Convert bytes to i16 samples
    let samples: Vec<i16> = pcm_data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    if samples.is_empty() {
        return Ok(());
    }

    // Move the entire cpal Stream lifecycle into spawn_blocking so the
    // !Send Stream never crosses an async await point.
    let stop_flag_clone = stop_flag.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

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
        let mut pos = 0usize;

        // Latch for asynchronous output-stream errors. The error callback
        // writes here; `await_playback` observes it so a device failure after
        // stream.play() succeeds still fails TTS (P1-5). `TtsEvent::Finished`
        // is therefore never emitted after a failed playback.
        let playback_err: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let err_latch = playback_err.clone();
        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    if pos < samples_clone.len() {
                        *sample = samples_clone[pos] as f32 / 32768.0;
                        pos += 1;
                    } else {
                        *sample = 0.0;
                    }
                }
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

        // Wait for playback to finish or stop, surfacing async device errors
        // as authoritative failures.
        let total_duration_ms =
            (samples_arc.len() as f64 / sample_rate as f64 * 1000.0) as u64 + 200;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(total_duration_ms);

        await_playback(&playback_err, Some(&stop_flag_clone), deadline)?;

        drop(stream);
        Ok(())
    })
    .await??;

    Ok(())
}

/// Verify that Piper binary exists and model file is present
pub fn verify_piper_installation(paths: &crate::paths::AppPaths) -> anyhow::Result<()> {
    let tools = crate::paths::resolve_tools(&paths.tool_dir);
    let piper_bin = tools
        .piper_bin
        .ok_or_else(|| anyhow::anyhow!("Piper binary not found"))?;
    let piper_model = tools
        .piper_model
        .ok_or_else(|| anyhow::anyhow!("Piper model not found"))?;
    if !piper_bin.exists() {
        anyhow::bail!("Piper binary not found at {}", piper_bin.display());
    }
    if !piper_model.exists() {
        anyhow::bail!("Piper model not found at {}", piper_model.display());
    }
    Ok(())
}
