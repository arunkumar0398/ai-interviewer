/// Test: WAV file write/read roundtrip with hound
/// Verifies incremental writing produces a valid WAV file
#[test]
fn wav_write_read_roundtrip() {
    let tmp_dir = std::env::temp_dir();
    let wav_path = tmp_dir.join("test_roundtrip.wav");
    let _ = std::fs::remove_file(&wav_path);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    // Write 1 second of 440Hz sine wave
    let sample_rate = 16000u32;
    let duration_secs = 1u32;
    let freq = 440.0f64;

    {
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        for i in 0..(sample_rate * duration_secs) {
            let t = i as f64 / sample_rate as f64;
            let sample = (2.0 * std::f64::consts::PI * freq * t).sin();
            let i16_sample = (sample * 32767.0) as i16;
            writer.write_sample(i16_sample).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();
    }

    // Read it back
    let mut reader = hound::WavReader::open(&wav_path).unwrap();
    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();

    assert_eq!(samples.len(), 16000); // 1 second at 16kHz
    assert!(samples.iter().any(|&s| s != 0), "Should have non-zero samples");

    // Cleanup
    let _ = std::fs::remove_file(&wav_path);
}

/// Test: SHA-256 checksum is deterministic
#[test]
fn sha256_deterministic() {
    use sha2::Digest;

    let data = b"hello world";
    let hash1 = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    };
    let hash2 = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    };

    assert_eq!(hash1, hash2);
    assert_eq!(
        hash1,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

/// Test: SHA-256 file hash matches in-memory hash
#[test]
fn sha256_file_matches_memory() {
    use sha2::Digest;
    use std::io::Write;

    let tmp_path = std::env::temp_dir().join("test_sha256_file.bin");
    let content = b"integration test content for hashing";

    // Write file
    {
        let mut file = std::fs::File::create(&tmp_path).unwrap();
        file.write_all(content).unwrap();
    }

    // Hash file using our function pattern
    let file_hash = {
        let mut file = std::fs::File::open(&tmp_path).unwrap();
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf).unwrap();
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        format!("{:x}", hasher.finalize())
    };

    // Hash same content in memory
    let mem_hash = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
    };

    assert_eq!(file_hash, mem_hash);

    let _ = std::fs::remove_file(&tmp_path);
}

/// Test: Atomic rename produces final file
#[test]
fn atomic_rename_creates_file() {
    let tmp_dir = std::env::temp_dir();
    let final_path = tmp_dir.join("test_atomic_final.wav");
    let tmp_path = tmp_dir.join("test_atomic_final.wav.tmp");

    let _ = std::fs::remove_file(&final_path);
    let _ = std::fs::remove_file(&tmp_path);

    // Create temp file
    std::fs::write(&tmp_path, b"test audio data").unwrap();
    assert!(tmp_path.exists());
    assert!(!final_path.exists());

    // Atomic rename
    std::fs::rename(&tmp_path, &final_path).unwrap();

    assert!(!tmp_path.exists());
    assert!(final_path.exists());
    assert_eq!(std::fs::read(&final_path).unwrap(), b"test audio data");

    let _ = std::fs::remove_file(&final_path);
}

/// Test: Temporary WAV file can be created and flushed
#[test]
fn temp_wav_write_flush() {
    let tmp_path = std::env::temp_dir().join("test_flush.wav.tmp");
    let _ = std::fs::remove_file(&tmp_path);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    {
        let mut writer = hound::WavWriter::create(&tmp_path, spec).unwrap();
        for i in 0..1600 {
            let sample = (i as f64 * 0.1 * std::f64::consts::PI).sin();
            writer.write_sample((sample * 32767.0) as i16).unwrap();
        }
        // Periodic flush (like our capture code does)
        writer.flush().unwrap();
        assert!(tmp_path.exists(), "Temp file should exist after flush");

        // More samples
        for i in 0..1600 {
            let sample = (i as f64 * 0.1 * std::f64::consts::PI).cos();
            writer.write_sample((sample * 32767.0) as i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    // Verify file is readable
    let mut reader = hound::WavReader::open(&tmp_path).unwrap();
    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), 3200); // 0.2 seconds at 16kHz

    let _ = std::fs::remove_file(&tmp_path);
}

/// Test: CaptureEvent serialization
#[test]
fn capture_event_serialization() {
    let event = ai_interviewer_lib::audio::capture::CaptureEvent::Started { sample_rate: 16000 };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("16000"));

    let event = ai_interviewer_lib::audio::capture::CaptureEvent::Stopped {
        file_path: "/tmp/test.wav".to_string(),
        duration_ms: 5000,
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("/tmp/test.wav"));
    assert!(json.contains("5000"));
}
