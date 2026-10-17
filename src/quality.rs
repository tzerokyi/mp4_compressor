use crate::process::run_ffmpeg;
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn a(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[derive(Debug, Clone, Copy)]
pub enum Metric {
    Vmaf,
    Ssim,
}
impl Metric {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vmaf => "VMAF",
            Self::Ssim => "SSIM",
        }
    }
    pub fn cutoff(self, vmaf_target: f64) -> f64 {
        match self {
            Self::Vmaf => vmaf_target,
            Self::Ssim => (0.98 + (vmaf_target - 95.0) * 0.002).clamp(0.95, 0.995),
        }
    }
}

pub fn sample_offsets(duration: f64) -> Vec<(f64, f64)> {
    if duration <= 3.0 {
        return vec![(0.0, duration)];
    }
    let slice = 1.5_f64.min(duration / 3.0);
    let available = (duration - slice).max(0.0);
    [0.15, 0.80]
        .into_iter()
        .map(|ratio| (available * ratio, slice))
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct SearchResult {
    pub crf: u8,
    pub score: f64,
    pub met_target: bool,
}

fn sample_reference(
    input: &Path,
    output: &Path,
    start: f64,
    seconds: f64,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut args = a(&[
        "-hide_banner",
        "-nostdin",
        "-y",
        "-loglevel",
        "error",
        "-ss",
    ]);
    args.push(start.to_string().into());
    args.push("-i".into());
    args.push(input.as_os_str().into());
    args.extend(a(&["-t"]));
    args.push(seconds.to_string().into());
    args.extend(a(&[
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        "pad=ceil(iw/2)*2:ceil(ih/2)*2",
        "-c:v",
        "ffv1",
        "-level",
        "3",
        "-pix_fmt",
        "yuv420p",
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "matroska",
    ]));
    args.push(output.as_os_str().into());
    run_ffmpeg(&args, None, cancel, |_| {})
}

fn score_candidate(
    reference: &Path,
    folder: &Path,
    crf: u8,
    metric: Metric,
    cancel: &AtomicBool,
) -> Result<f64, String> {
    let candidate = folder.join(format!("crf{crf}.mp4"));
    let log = folder.join("vmaf.json");
    let ssim_log = folder.join("ssim.log");
    let mut args = a(&["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-i"]);
    args.push(reference.as_os_str().into());
    args.extend(a(&["-an", "-c:v", "libx264", "-preset", "slow", "-crf"]));
    args.push(crf.to_string().into());
    args.extend(a(&[
        "-pix_fmt",
        "yuv420p",
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "mp4",
    ]));
    args.push(candidate.as_os_str().into());
    run_ffmpeg(&args, None, cancel, |_| {})?;
    let mut score_args = a(&["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-i"]);
    score_args.push(candidate.as_os_str().into());
    score_args.push("-i".into());
    score_args.push(reference.as_os_str().into());
    let filter = match metric {
        Metric::Vmaf => "[0:v:0]setpts=PTS-STARTPTS[dist];[1:v:0]setpts=PTS-STARTPTS[ref];[dist][ref]libvmaf=log_fmt=json:log_path=vmaf.json",
        Metric::Ssim => "[0:v:0]setpts=PTS-STARTPTS[dist];[1:v:0]setpts=PTS-STARTPTS[ref];[dist][ref]ssim=stats_file=ssim.log",
    };
    score_args.extend(a(&[
        "-filter_complex",
        filter,
        "-an",
        "-f",
        "null",
        "-progress",
        "pipe:1",
        "-nostats",
        "-",
    ]));
    run_ffmpeg(&score_args, Some(folder), cancel, |_| {})?;
    let mean = match metric {
        Metric::Vmaf => {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&log).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("Invalid VMAF JSON: {e}"))?;
            value
                .pointer("/pooled_metrics/vmaf/mean")
                .and_then(serde_json::Value::as_f64)
                .ok_or("VMAF mean score missing")?
        }
        Metric::Ssim => {
            parse_ssim(&std::fs::read_to_string(&ssim_log).map_err(|e| e.to_string())?)?
        }
    };
    if !mean.is_finite() {
        return Err("Invalid quality score".into());
    }
    Ok(mean)
}

pub fn search(
    input: &Path,
    duration: f64,
    metric: Metric,
    target: f64,
    cancel: &AtomicBool,
    mut phase: impl FnMut(f32, &str),
) -> Result<SearchResult, String> {
    if !target.is_finite() || !(80.0..=99.9).contains(&target) {
        return Err("Invalid quality target".into());
    }
    let threshold = metric.cutoff(target);
    let tmp = tempfile::Builder::new()
        .prefix("mp4-compressor-samples-")
        .tempdir()
        .map_err(|e| format!("Sample temp directory: {e}"))?;
    let mut folders = Vec::new();
    let offsets = sample_offsets(duration);
    for (idx, (start, seconds)) in offsets.iter().enumerate() {
        let sample_dir = tmp.path().join(format!("sample{idx}"));
        std::fs::create_dir(&sample_dir).map_err(|e| e.to_string())?;
        let reference = sample_dir.join("reference.mkv");
        phase(
            (idx as f32) / (offsets.len() as f32 * 7.0),
            "Extracting reference frames",
        );
        sample_reference(input, &reference, *start, *seconds, cancel)?;
        folders.push((sample_dir, reference));
    }
    const MIN_CRF: u8 = 12;
    const MAX_CRF: u8 = 32;
    const MAX_TESTS: usize = 5;
    let mut low = MIN_CRF;
    let mut high = MAX_CRF;
    let mut best: Option<(u8, f64)> = None;
    let mut lowest_tested: Option<(u8, f64)> = None;
    let mut step = 0;
    while low <= high && step < MAX_TESTS {
        let mid = low + (high - low) / 2;
        let mut scores = Vec::new();
        for (idx, (dir, reference)) in folders.iter().enumerate() {
            phase(
                ((step * folders.len() + idx + folders.len()) as f32
                    / (folders.len() as f32 * (MAX_TESTS + 1) as f32))
                    .min(0.98),
                "Evaluating sample quality",
            );
            scores.push(score_candidate(reference, dir, mid, metric, cancel)?);
        }
        let mean = scores.iter().sum::<f64>() / scores.len() as f64;
        let worst = scores.iter().copied().fold(f64::INFINITY, f64::min);
        let worst_threshold = match metric {
            Metric::Vmaf => threshold - 2.0,
            Metric::Ssim => threshold - 0.015,
        };
        let pass = mean >= threshold && worst >= worst_threshold;
        if lowest_tested.is_none_or(|(tested, _)| mid < tested) {
            lowest_tested = Some((mid, mean));
        }
        phase(
            ((step + 1) as f32 / MAX_TESTS as f32).min(0.99),
            &format!(
                "CRF {mid}, {} mean {mean:.4}, worst {worst:.4}",
                metric.name()
            ),
        );
        if pass {
            best = Some((mid, mean));
            low = mid + 1;
        } else {
            high = mid.saturating_sub(1);
        }
        step += 1;
    }
    if let Some((crf, score)) = best {
        return Ok(SearchResult {
            crf,
            score,
            met_target: true,
        });
    }
    let (crf, score) = match lowest_tested {
        Some((MIN_CRF, score)) => (MIN_CRF, score),
        _ => {
            phase(
                0.98,
                "Strict target not reached; testing highest-quality fallback",
            );
            let scores: Result<Vec<f64>, String> = folders
                .iter()
                .map(|(dir, reference)| score_candidate(reference, dir, MIN_CRF, metric, cancel))
                .collect();
            let scores = scores?;
            (MIN_CRF, scores.iter().sum::<f64>() / scores.len() as f64)
        }
    };
    Ok(SearchResult {
        crf,
        score,
        met_target: false,
    })
}

fn parse_ssim(stats: &str) -> Result<f64, String> {
    let scores: Vec<f64> = stats
        .lines()
        .flat_map(|line| line.split_whitespace())
        .filter_map(|token| token.strip_prefix("All:"))
        .filter_map(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .collect();
    if scores.is_empty() {
        return Err("No SSIM frame scores".into());
    }
    Ok(scores.iter().sum::<f64>() / scores.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_ssim_stats() {
        let score =
            parse_ssim("n:1 Y:0.9 U:0.9 V:0.9 All:0.980000 (12.4)\nn:2 All:0.990000 (12.5)\n")
                .unwrap();
        assert!((score - 0.985).abs() < 0.00001);
    }
    #[test]
    fn smart_samples_are_short() {
        let samples = sample_offsets(240.0);
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|(_, length)| *length <= 1.5));
    }
    #[test]
    fn offset_bounds() {
        for duration in [1.0, 3.5, 8.0, 12.0, 100.0] {
            let samples = sample_offsets(duration);
            for (start, len) in samples {
                assert!(start >= 0.0 && len > 0.0 && start + len <= duration + 0.000001);
            }
        }
    }
}
