use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_TOOLS_DIR: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools";

fn resolve_tools_dir() -> PathBuf {
    // 1. CLI arg
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    // 2. Environment variable
    if let Ok(val) = std::env::var("AI_INTERVIEWER_TOOLS") {
        return PathBuf::from(val);
    }
    // 3. Default (development)
    PathBuf::from(DEFAULT_TOOLS_DIR)
}

fn main() -> anyhow::Result<()> {
    println!("=== Audio Spike: Native Audio Roundtrip ===\n");

    let tools = resolve_tools_dir();
    println!("Tools directory: {}", tools.display());

    let piper_bin = tools.join("piper").join("piper").join("piper.exe");
    let piper_model = tools.join("piper-models").join("en_US-amy-medium.onnx");
    let whisper_bin = tools.join("whisper").join("Release").join("main.exe");
    let whisper_model = tools.join("models").join("ggml-tiny.en.bin");

    // Validate
    for (label, path) in [
        ("Piper binary", &piper_bin),
        ("Piper model", &piper_model),
        ("Whisper binary", &whisper_bin),
        ("Whisper model", &whisper_model),
    ] {
        if !path.exists() {
            anyhow::bail!("{} not found at: {}", label, path.display());
        }
    }

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
