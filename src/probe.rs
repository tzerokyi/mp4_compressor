use serde::Deserialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Stream {
    pub codec_type: Option<String>,
    pub codec_name: Option<String>,
    pub profile: Option<String>,
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub avg_frame_rate: Option<String>,
    pub bit_rate: Option<String>,
    #[serde(default)]
    pub tags: serde_json::Value,
    #[serde(default)]
    pub side_data_list: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Format {
    pub duration: Option<String>,
    pub size: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProbeResponse {
    streams: Vec<Stream>,
    format: Format,
}

#[derive(Debug, Clone)]
pub struct Media {
    pub duration: f64,
    pub bytes: u64,
    pub video: Stream,
    pub audio: Option<Stream>,
}

pub fn inspect(path: &Path) -> Result<Media, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|err| format!("Cannot run ffprobe: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let data: ProbeResponse = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("Invalid ffprobe output: {err}"))?;
    let video = data
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .cloned()
        .ok_or("No video stream found")?;
    let duration: f64 = data
        .format
        .duration
        .as_deref()
        .ok_or("Missing duration")?
        .parse()
        .map_err(|_| "Invalid duration")?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err("Duration must be positive and finite".into());
    }
    let bytes = std::fs::metadata(path)
        .map_err(|err| err.to_string())?
        .len();
    if bytes == 0 {
        return Err("Empty source file".into());
    }
    let audio = data
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"))
        .cloned();
    Ok(Media {
        duration,
        bytes,
        video,
        audio,
    })
}

pub fn reject_unsafe_source(media: &Media) -> Result<(), String> {
    let v = &media.video;
    if v.width.unwrap_or(0) == 0 || v.height.unwrap_or(0) == 0 {
        return Err("Unsupported video geometry".into());
    }
    let transfer = v.color_transfer.as_deref().unwrap_or("").to_lowercase();
    if transfer.contains("smpte2084") || transfer.contains("arib-std-b67") {
        return Err("HDR detected. SDR tone mapping is not implemented; source protected.".into());
    }
    let pix = v.pix_fmt.as_deref().unwrap_or("").to_lowercase();
    if pix.contains("10le") || pix.contains("12le") || pix.contains("p10") || pix.contains("p12") {
        return Err("10/12-bit video detected; automatic 8-bit conversion is disabled.".into());
    }
    let rotation = v
        .tags
        .get("rotate")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let display_rotation = v
        .side_data_list
        .iter()
        .filter_map(|x| x.get("rotation"))
        .filter_map(|x| x.as_f64().or_else(|| x.as_i64().map(|v| v as f64)))
        .find(|rotation| rotation.abs() > 0.01);
    if rotation.abs() > 0.01 || display_rotation.is_some() {
        return Err("Display rotation detected; safe rotation mapping is not implemented.".into());
    }
    Ok(())
}

pub fn duration_is_close(input: f64, output: f64, fps: Option<f64>) -> bool {
    let period = fps.filter(|x| *x > 0.0).map(|x| 1.0 / x).unwrap_or(0.05);
    (input - output).abs() <= (period * 3.0).max(0.20)
}

pub fn fps(stream: &Stream) -> Option<f64> {
    let s = stream.avg_frame_rate.as_deref()?;
    if let Some((n, d)) = s.split_once('/') {
        let n = n.parse::<f64>().ok()?;
        let d = d.parse::<f64>().ok()?;
        if d == 0.0 {
            return None;
        }
        return Some(n / d).filter(|f| f.is_finite() && *f > 0.0);
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite() && *f > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_fps() {
        let s = Stream {
            avg_frame_rate: Some("30000/1001".into()),
            ..Default::default()
        };
        let result = fps(&s).expect("fps");
        assert!((result - 29.97).abs() < 0.01);
    }
    #[test]
    fn duration_tolerance() {
        assert!(duration_is_close(10.0, 10.03, Some(30.0)));
        assert!(!duration_is_close(10.0, 8.0, Some(30.0)));
    }
}
