/// Test: WAV file write/read roundtrip with hound
/// Verifies incremental writing produces a valid WAV file
use sha2::Digest;
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
    assert!(
        samples.iter().any(|&s| s != 0),
        "Should have non-zero samples"
    );

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
            if n == 0 {
                break;
            }
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

// ============================================================
// NEW: Audio pipeline edge case tests
// ============================================================

/// Test: WAV write/read with 5-second silence (settling period simulation)
#[test]
fn wav_settling_period_simulation() {
    let wav_path = std::env::temp_dir().join("test_settling.wav");
    let _ = std::fs::remove_file(&wav_path);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    // Write 5 seconds of silence (all zeros) — simulates settling period
    {
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        for _ in 0..(16000 * 5) {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    let mut reader = hound::WavReader::open(&wav_path).unwrap();
    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), 80000, "5 seconds at 16kHz");
    assert!(
        samples.iter().all(|&s| s == 0),
        "All samples should be zero"
    );

    let _ = std::fs::remove_file(&wav_path);
}

/// Test: WAV write/read with 60-second duration (max interview answer)
#[test]
fn wav_max_duration_boundary() {
    let wav_path = std::env::temp_dir().join("test_max_duration.wav");
    let _ = std::fs::remove_file(&wav_path);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    {
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        // Write 60 seconds of a simple pattern
        for i in 0..(16000 * 60) {
            let sample =
                ((i as f64 / 16000.0 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 32767.0) as i16;
            writer.write_sample(sample).unwrap();
            // Periodic flush like our capture code
            if i % 8000 == 0 {
                writer.flush().unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    let mut reader = hound::WavReader::open(&wav_path).unwrap();
    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), 960000, "60 seconds at 16kHz");

    let _ = std::fs::remove_file(&wav_path);
}

/// Test: SHA-256 of large file (100KB) is deterministic
#[test]
fn sha256_large_file_deterministic() {
    use std::io::Write;

    let path = std::env::temp_dir().join("test_large_sha256.bin");
    let _ = std::fs::remove_file(&path);

    // Write 100KB of patterned data
    {
        let mut file = std::fs::File::create(&path).unwrap();
        for i in 0..100 {
            let chunk = vec![((i * 7 + 13) & 0xFF) as u8; 1024];
            file.write_all(&chunk).unwrap();
        }
    }

    // Hash twice — must be identical
    let hash1 = {
        let mut file = std::fs::File::open(&path).unwrap();
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        format!("{:x}", hasher.finalize())
    };

    let hash2 = {
        let mut file = std::fs::File::open(&path).unwrap();
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        format!("{:x}", hasher.finalize())
    };

    assert_eq!(hash1, hash2, "Same file must produce same hash");
    assert_eq!(hash1.len(), 64, "SHA-256 hex is 64 chars");

    let _ = std::fs::remove_file(&path);
}

/// Test: Atomic rename when source .tmp is missing (graceful handling)
#[test]
fn atomic_rename_missing_source() {
    let tmp_dir = std::env::temp_dir();
    let final_path = tmp_dir.join("test_atomic_missing_final.wav");
    let tmp_path = tmp_dir.join("test_atomic_missing_final.wav.tmp");

    let _ = std::fs::remove_file(&final_path);
    let _ = std::fs::remove_file(&tmp_path);

    // Attempt rename when source doesn't exist
    let result = std::fs::rename(&tmp_path, &final_path);
    assert!(result.is_err(), "Should fail when source is missing");
    assert!(!final_path.exists());

    let _ = std::fs::remove_file(&final_path);
}

/// Test: WAV with different sample rates (22050 for Piper, 16000 for Whisper)
#[test]
fn wav_multiple_sample_rates() {
    for rate in &[16000u32, 22050, 44100] {
        let wav_path = std::env::temp_dir().join(format!("test_rate_{}.wav", rate));
        let _ = std::fs::remove_file(&wav_path);

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: *rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        {
            let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
            for i in 0..*rate {
                let sample = ((i as f64 / *rate as f64 * 440.0 * 2.0 * std::f64::consts::PI).sin()
                    * 32767.0) as i16;
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }

        let mut reader = hound::WavReader::open(&wav_path).unwrap();
        let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), *rate as usize, "1 second at {}Hz", rate);

        let _ = std::fs::remove_file(&wav_path);
    }
}

/// Test: CaptureEvent error serialization
#[test]
fn capture_event_error_roundtrip() {
    let event = ai_interviewer_lib::audio::capture::CaptureEvent::Error {
        message: "Device not found".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("Device not found"));
    assert!(json.contains("Error"));
}

/// Test: WAV metadata computation matches orchestrator formula
#[test]
fn wav_duration_formula_matches_file() {
    let wav_path = std::env::temp_dir().join("test_formula_duration.wav");
    let _ = std::fs::remove_file(&wav_path);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    // Write exactly 3 seconds
    {
        let mut writer = hound::WavWriter::create(&wav_path, spec).unwrap();
        for i in 0..(16000 * 3) {
            let sample =
                ((i as f64 / 16000.0 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 32767.0) as i16;
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    let file_size = std::fs::metadata(&wav_path).unwrap().len();
    let duration_ms = {
        let data_bytes = file_size.saturating_sub(44);
        (data_bytes * 1000) / (16000 * 2)
    };

    // Should be approximately 3000ms (within 100ms tolerance for header variations)
    assert!(
        duration_ms >= 2900 && duration_ms <= 3100,
        "Duration should be ~3000ms, got {}ms (file_size={})",
        duration_ms,
        file_size
    );

    let _ = std::fs::remove_file(&wav_path);
}
