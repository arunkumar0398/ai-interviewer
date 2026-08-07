use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
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

/// Supervises a Piper TTS child process with restart and graceful shutdown.
const PIPER_TIMEOUT_SECS: u64 = 60;

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
    /// Kills the child process immediately when stop_flag is set.
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

        tokio::task::spawn_blocking(move || {
            let mut restarts = 0u32;

            'restart: loop {
                if stop_flag.load(Ordering::SeqCst) {
                    return Ok(());
                }

                // Spawn Piper with stdin input (not --text flag)
                let result = std::process::Command::new(&piper_bin)
                    .arg("--model")
                    .arg(&model_path)
                    .arg("--output-raw")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();

                let mut child = match result {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = event_tx.try_send(TtsEvent::Error {
                            message: format!("Failed to start Piper: {}", e),
                        });
                        return Err(anyhow::anyhow!("Failed to start Piper: {}", e));
                    }
                };

                // Write text to stdin
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    if let Err(e) = stdin.write_all(text.as_bytes()) {
                        let _ = event_tx.try_send(TtsEvent::Error {
                            message: format!("Failed to write to Piper stdin: {}", e),
                        });
                        let _ = child.kill();
                        return Err(anyhow::anyhow!("Stdin write failed: {}", e));
                    }
                    drop(stdin);
                }

                // Read raw PCM from stdout and play it via cpal
                let stdout = child.stdout.take();
                let _ = event_tx.try_send(TtsEvent::Speaking { text: text.clone() });

                let _play_result = play_raw_pcm(stdout, sample_rate, stop_flag.clone());

                // Kill the child process if stop was requested during playback
                if stop_flag.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait(); // reap the process
                    return Ok(());
                }

                // Wait for process to finish — deadline-based to enforce timeout
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(PIPER_TIMEOUT_SECS);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if status.success() {
                                let _ = event_tx.try_send(TtsEvent::Finished { duration_ms: 0 });
                                return Ok(());
                            }
                            // Non-zero exit — may need restart
                            restarts += 1;
                            if restarts >= max_restarts {
                                let _ = event_tx.try_send(TtsEvent::Error {
                                    message: format!(
                                        "Piper crashed {} times, giving up",
                                        max_restarts
                                    ),
                                });
                                return Err(anyhow::anyhow!("Piper exceeded max restarts"));
                            }
                            let _ = event_tx.try_send(TtsEvent::ProcessCrashed { restarts });
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            continue 'restart;
                        }
                        Ok(None) => {
                            // Still running — check deadline
                            if std::time::Instant::now() >= deadline {
                                let _ = child.kill();
                                let _ = child.wait(); // reap zombie
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
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(e) => {
                            let _ = event_tx.try_send(TtsEvent::Error {
                                message: format!("Piper wait error: {}", e),
                            });
                            return Err(anyhow::anyhow!("Piper wait error: {}", e));
                        }
                    }
                }
            }
        })
        .await?
    }
}

/// Play raw 16-bit PCM from a reader via cpal output device.
fn play_raw_pcm(
    reader: Option<std::process::ChildStdout>,
    sample_rate: u32,
    stop_flag: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let reader = reader.ok_or_else(|| anyhow::anyhow!("No stdout from Piper"))?;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device found"))?;

    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // Read all raw PCM bytes from Piper stdout
    let mut pcm_data = Vec::new();
    use std::io::Read;
    let mut buf_reader = std::io::BufReader::new(reader);
    buf_reader.read_to_end(&mut pcm_data)?;

    // Convert bytes to i16 samples
    let samples: Vec<i16> = pcm_data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    if samples.is_empty() {
        return Ok(());
    }

    let samples = Arc::new(samples);
    let samples_clone = samples.clone();
    let mut pos = 0usize;

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
        |err| {
            eprintln!("Output stream error: {}", err);
        },
        None,
    )?;

    stream.play()?;

    // Wait for playback to finish or stop
    let total_duration =
        std::time::Duration::from_secs_f64(samples.len() as f64 / sample_rate as f64);
    let deadline =
        std::time::Instant::now() + total_duration + std::time::Duration::from_millis(200);

    while std::time::Instant::now() < deadline {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

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
