use std::path::PathBuf;

/// Represents a WAV file header and metadata
#[derive(Debug, Clone)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub data_size: u32,
    pub file_path: PathBuf,
}

/// Write a WAV header to a file
pub fn write_wav_header(
    file: &mut std::fs::File,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    data_size: u32,
) -> std::io::Result<()> {
    use std::io::Write;

    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;

    // RIFF header
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_size).to_le_bytes())?;
    file.write_all(b"WAVE")?;

    // fmt chunk
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // chunk size
    file.write_all(&1u16.to_le_bytes())?; // PCM format
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits_per_sample.to_le_bytes())?;

    // data chunk
    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;

    Ok(())
}

/// Validate a WAV file exists and has reasonable properties
pub fn validate_wav(path: &PathBuf) -> anyhow::Result<WavInfo> {
    let data = std::fs::read(path)?;
    if data.len() < 44 {
        anyhow::bail!("File too small to be a valid WAV");
    }

    if &data[0..4] != b"RIFF" {
        anyhow::bail!("Missing RIFF header");
    }
    if &data[8..12] != b"WAVE" {
        anyhow::bail!("Missing WAVE format");
    }

    let channels = u16::from_le_bytes([data[22], data[23]]);
    let sample_rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let bits_per_sample = u16::from_le_bytes([data[34], data[35]]);
    let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);

    Ok(WavInfo {
        sample_rate,
        channels,
        bits_per_sample,
        data_size,
        file_path: path.clone(),
    })
}
