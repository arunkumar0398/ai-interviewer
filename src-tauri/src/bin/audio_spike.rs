use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn resolve_tools_dir() -> PathBuf {
    // 1. CLI arg
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    // 2. Environment variable
    if let Ok(val) = std::env::var("AI_INTERVIEWER_TOOLS") {
        return PathBuf::from(val);
    }
    // 3. Next to the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let p = exe_dir.join("tools");
            if p.exists() {
                return p;
            }
        }
    }
    // 4. Dev fallback: workspace root / tools
    #[cfg(debug_assertions)]
    {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let dev = std::path::PathBuf::from(manifest_dir)
            .parent()
            .expect("workspace root")
            .join("tools");
        if dev.exists() {
            return dev;
        }
    }
    panic!(
        "No tools directory found. Set AI_INTERVIEWER_TOOLS or place tools/ next to the binary."
    );
}

fn main() -> anyhow::Result<()> {
    println!("=== Audio Spike: Native Audio Roundtrip ===\n");

    let tools = resolve_tools_dir();
    println!("Tools directory: {}", tools.display());

    // Use shared path resolution (supports both canonical and legacy layouts)
    let (piper_bin, piper_model) = ai_interviewer_lib::paths::resolve_piper_paths(&tools);
    let whisper_bin = ai_interviewer_lib::paths::resolve_whisper_path(&tools);
    let whisper_model = ai_interviewer_lib::paths::resolve_whisper_model_path(&tools);

    let piper_bin = piper_bin.expect("Piper binary not found");
    let piper_model = piper_model.expect("Piper model not found");
    let whisper_bin = whisper_bin.expect("Whisper binary not found");
    let whisper_model = whisper_model.expect("Whisper model not found");

    println!("  Piper binary:  {}", piper_bin.display());
    println!("  Piper model:   {}", piper_model.display());
    println!("  Whisper binary:{}", whisper_bin.display());
    println!("  Whisper model: {}", whisper_model.display());

    let work_dir = tools.join("spike");
    std::fs::create_dir_all(&work_dir)?;
    let tts_output = work_dir.join("question.wav");

    // Step 1: Generate TTS via stdin
    let question = "Tell me about your experience with systems programming.";
    println!("[1/4] Generating TTS for: \"{}\"", question);

    let mut child = Command::new(&piper_bin)
        .arg("--model")
        .arg(&piper_model)
        .arg("--output_file")
        .arg(&tts_output)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(question.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("Piper TTS failed: {}", stderr);
    }

    if !tts_output.exists() {
        anyhow::bail!("Piper did not create WAV file. Stderr: {}", stderr);
    }

    let metadata = std::fs::metadata(&tts_output)?;
    println!("      WAV generated: {} bytes", metadata.len());

    // Step 2: Verify WAV is valid
    println!("[2/4] Verifying WAV file...");
    let data = std::fs::read(&tts_output)?;
    if data.len() < 44 {
        anyhow::bail!("File too small to be a valid WAV");
    }
    if &data[0..4] != b"RIFF" {
        anyhow::bail!("Missing RIFF header");
    }
    if &data[8..12] != b"WAVE" {
        anyhow::bail!("Missing WAVE format");
    }
    let wav_channels = u16::from_le_bytes([data[22], data[23]]);
    let wav_sample_rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let wav_bits = u16::from_le_bytes([data[34], data[35]]);
    println!("      Sample rate: {} Hz", wav_sample_rate);
    println!("      Channels: {}", wav_channels);
    println!("      Bits per sample: {}", wav_bits);

    // Step 3: "Playback" (placeholder — real WASAPI in Phase 1)
    println!("[3/4] Playback (placeholder — real WASAPI capture in Phase 1)");
    println!("      Would play: {}", tts_output.display());

    // Step 4: Transcribe with whisper
    println!("[4/4] Transcribing with whisper.cpp...");

    let output = Command::new(&whisper_bin)
        .arg("--model")
        .arg(&whisper_model)
        .arg("--file")
        .arg(&tts_output)
        .arg("--language")
        .arg("en")
        .arg("-otxt")
        .arg("-of")
        .arg(work_dir.join("transcript"))
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        println!("      Whisper stderr: {}", stderr);
        anyhow::bail!("Whisper transcription failed");
    }

    let txt_file = work_dir.join("transcript.txt");
    if txt_file.exists() {
        let transcript = std::fs::read_to_string(&txt_file)?;
        println!("\n=== TRANSCRIPT ===");
        println!("{}", transcript.trim());
        println!("==================\n");

        if transcript.trim().len() > 5 {
            println!("SUCCESS: Audio roundtrip complete!");
            println!("  - Piper TTS generated WAV");
            println!("  - Whisper transcribed successfully");
            println!("  - Transcript length: {} chars", transcript.trim().len());
        } else {
            println!("WARNING: Transcript seems too short, may need investigation");
        }
    } else {
        println!("      Whisper output files:");
        for entry in std::fs::read_dir(&work_dir)? {
            let entry = entry?;
            println!("        {}", entry.file_name().to_string_lossy());
        }
        println!("\n      Whisper stdout: {}", stdout);
    }

    Ok(())
}
