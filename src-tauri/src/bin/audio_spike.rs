use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const PIPER_PATH: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools\piper\piper\piper.exe";
const PIPER_MODEL: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools\piper-models\en_US-amy-medium.onnx";
const WHISPER_PATH: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools\whisper\Release\whisper-cli.exe";
const WHISPER_MODEL: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools\models\ggml-tiny.en.bin";
const WORK_DIR: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools\spike";

fn main() -> anyhow::Result<()> {
    println!("=== Audio Spike: Native Audio Roundtrip ===\n");

    // Ensure work directory exists
    std::fs::create_dir_all(WORK_DIR)?;

    let tts_output = PathBuf::from(WORK_DIR).join("question.wav");

    // Step 1: Generate TTS via stdin
    let question = "Tell me about your experience with systems programming.";
    println!("[1/4] Generating TTS for: \"{}\"", question);

    let mut child = Command::new(PIPER_PATH)
        .arg("--model")
        .arg(PIPER_MODEL)
        .arg("--output_file")
        .arg(&tts_output)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write question to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(question.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("Piper TTS failed: {}", stderr);
    }

    // Check if WAV was created
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

    let output = Command::new(WHISPER_PATH)
        .arg("--model")
        .arg(WHISPER_MODEL)
        .arg("--file")
        .arg(&tts_output)
        .arg("--language")
        .arg("en")
        .arg("-otxt")
        .arg("-of")
        .arg(PathBuf::from(WORK_DIR).join("transcript"))
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        println!("      Whisper stderr: {}", stderr);
        anyhow::bail!("Whisper transcription failed");
    }

    // Read the transcription output
    let txt_file = PathBuf::from(WORK_DIR).join("transcript.txt");
    if txt_file.exists() {
        let transcript = std::fs::read_to_string(&txt_file)?;
        println!("\n=== TRANSCRIPT ===");
        println!("{}", transcript.trim());
        println!("==================\n");

        // Verify it contains something meaningful
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
        for entry in std::fs::read_dir(WORK_DIR)? {
            let entry = entry?;
            println!("        {}", entry.file_name().to_string_lossy());
        }
        println!("\n      Whisper stdout: {}", stdout);
    }

    Ok(())
}
