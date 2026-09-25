//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! Minimal WAV I/O for parent samples and the offline demo renderer.
//!
//! Supports the formats that matter for a producer's sample library:
//! 16/24/32-bit integer and 32-bit float PCM, mono or stereo.

use std::path::Path;

use hound::{SampleFormat, WavReader, WavWriter};

/// Read a WAV file as per-channel f32 buffers (no resampling).
pub fn read_wav_channels(path: &Path) -> Result<(Vec<Vec<f32>>, u32), String> {
    let reader = WavReader::open(path).map_err(|e| format!("无法打开 {}: {e}", path.display()))?;
    let spec = reader.spec();
    let sr = spec.sample_rate;
    let channels = spec.channels as usize;
    if channels == 0 || channels > 2 {
        return Err(format!("仅支持 mono/stereo，此文件有 {channels} 声道"));
    }

    let mut chans: Vec<Vec<f32>> = vec![Vec::new(); channels];
    match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => {
            for (i, s) in reader.into_samples::<f32>().enumerate() {
                let s = s.map_err(|e| e.to_string())?;
                chans[i % channels].push(s);
            }
        }
        (SampleFormat::Int, 16) => {
            for (i, s) in reader.into_samples::<i16>().enumerate() {
                let s = s.map_err(|e| e.to_string())?;
                chans[i % channels].push(s as f32 / 32768.0);
            }
        }
        (SampleFormat::Int, 24) => {
            for (i, s) in reader.into_samples::<i32>().enumerate() {
                let s = s.map_err(|e| e.to_string())?;
                chans[i % channels].push(s as f32 / 8388608.0);
            }
        }
        (SampleFormat::Int, 32) => {
            for (i, s) in reader.into_samples::<i32>().enumerate() {
                let s = s.map_err(|e| e.to_string())?;
                chans[i % channels].push(s as f32 / 2147483648.0);
            }
        }
        (fmt, bits) => {
            return Err(format!("不支持的 WAV 格式: {fmt:?} {bits}-bit"));
        }
    }

    Ok((chans, sr))
}

/// Mix per-channel buffers down to mono (used for parent profile analysis).
pub fn to_mono(chans: &[Vec<f32>]) -> Vec<f32> {
    if chans.len() == 1 {
        return chans[0].clone();
    }
    let len = chans.iter().map(|c| c.len()).min().unwrap_or(0);
    let inv = 1.0 / chans.len() as f32;
    (0..len)
        .map(|i| chans.iter().map(|c| c[i]).sum::<f32>() * inv)
        .collect()
}

/// Write per-channel f32 buffers as 32-bit float WAV.
pub fn write_wav_32f(
    path: &Path,
    chans: &[Vec<f32>],
    sample_rate: u32,
) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: chans.len() as u16,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer =
        WavWriter::create(path, spec).map_err(|e| format!("无法创建 {}: {e}", path.display()))?;
    let len = chans.iter().map(|c| c.len()).min().unwrap_or(0);
    for i in 0..len {
        for c in chans {
            writer
                .write_sample(c[i].clamp(-1.0, 1.0))
                .map_err(|e| e.to_string())?;
        }
    }
    writer.finalize().map_err(|e| e.to_string())
}
