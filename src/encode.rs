use crate::probe::Media;
use std::ffi::OsString;
use std::path::Path;

/// The modes all retain H.264/MP4; the two CPU target modes trade speed for
/// bitrate accuracy. Manual is intentionally quality-driven rather than size-driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Balanced,
    Fast,
    Manual,
}

impl Mode {
    pub fn display(self) -> &'static str {
        match self {
            Self::Auto => "Precise CPU (2-pass)",
            Self::Balanced => "Quick CPU (1-pass)",
            Self::Fast => "Fast NVIDIA (VBR)",
            Self::Manual => "Manual quality (CPU)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub crf: u8,
    /// Requested reduction of the input's *total* file size, 0..=95.
    /// This is a target, not a guaranteed cap or a perceptual quality score.
    pub reduction_percent: u8,
    pub output_dir: Option<std::path::PathBuf>,
    pub discard_if_larger: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Balanced,
            crf: 25,
            reduction_percent: 80,
            output_dir: None,
            discard_if_larger: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    TargetBitrate(u64),
    QualityCrf(u8),
}

#[derive(Debug, Clone, Copy)]
pub struct Pass<'a> {
    pub number: u8,
    pub log_prefix: &'a Path,
}

pub fn compatible_aac(media: &Media) -> bool {
    media.audio.as_ref().is_some_and(|a| {
        a.codec_name.as_deref() == Some("aac")
            && matches!(a.profile.as_deref(), Some("LC") | Some("AAC LC"))
    })
}

pub fn estimated_target_bytes(input_bytes: u64, reduction_percent: u8) -> u64 {
    ((input_bytes as u128 * u128::from(100 - reduction_percent.min(95))) / 100) as u64
}

/// Reserve 2% for MP4 mux overhead and an estimated audio track. A 96 kbps
/// video floor deliberately permits exceeding an impossible requested target.
pub fn target_video_bitrate(media: &Media, reduction_percent: u8) -> Result<u64, String> {
    if reduction_percent > 95 {
        return Err("Reduction percentage must be between 0 and 95".into());
    }
    if !media.duration.is_finite() || media.duration <= 0.0 || media.bytes == 0 {
        return Err("Invalid source duration or size".into());
    }
    let target_bytes = estimated_target_bytes(media.bytes, reduction_percent);
    let audio_bps = if let Some(audio) = &media.audio {
        if compatible_aac(media) {
            audio
                .bit_rate
                .as_deref()
                .and_then(|bps| bps.parse::<u64>().ok())
                .unwrap_or(160_000)
                .min(1_000_000)
        } else {
            128_000
        }
    } else {
        0
    };
    let total_bps = (target_bytes as f64 * 8.0 / media.duration * 0.98).max(0.0);
    let video_bps = (total_bps - audio_bps as f64).max(96_000.0);
    if !video_bps.is_finite() || video_bps > u64::MAX as f64 {
        return Err("Target bitrate is outside the supported range".into());
    }
    Ok(video_bps.round() as u64)
}

fn needs_padding(media: &Media) -> bool {
    !media.video.width.unwrap_or(0).is_multiple_of(2)
        || !media.video.height.unwrap_or(0).is_multiple_of(2)
}

/// A GPU decoding attempt is only meaningful for even-sized supported 8-bit
/// sources. If runtime CUDA decoding fails, the worker retries CPU decode.
pub fn can_try_cuda_decode(media: &Media) -> bool {
    !needs_padding(media)
        && matches!(
            media.video.codec_name.as_deref(),
            Some("h264" | "hevc" | "mpeg2video" | "vp9" | "av1")
        )
}

#[allow(clippy::too_many_arguments)]
pub fn encode_args(
    input: &Path,
    output: &Path,
    media: &Media,
    mode: Mode,
    rate: RateControl,
    pass: Option<Pass<'_>>,
    cuda_decode: bool,
) -> Vec<OsString> {
    let first_pass = pass.is_some_and(|p| p.number == 1);
    let gpu = mode == Mode::Fast;
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-xerror",
        "-loglevel",
        "warning",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if gpu && cuda_decode {
        push(
            &mut args,
            &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
        );
    }
    push(&mut args, &["-i"]);
    args.push(input.as_os_str().into());
    push(&mut args, &["-map", "0:v:0", "-sn", "-dn"]);
    if !first_pass {
        push(&mut args, &["-map", "0:a:0?"]);
    }

    if gpu {
        push(
            &mut args,
            &["-c:v", "h264_nvenc", "-preset", "p3", "-rc", "vbr"],
        );
        if let RateControl::TargetBitrate(bps) = rate {
            push(&mut args, &["-b:v"]);
            args.push(bps.to_string().into());
            push(&mut args, &["-maxrate"]);
            args.push((bps.saturating_mul(3) / 2).to_string().into());
            push(&mut args, &["-bufsize"]);
            args.push(bps.saturating_mul(3).to_string().into());
        }
        push(&mut args, &["-spatial-aq", "1", "-bf", "3"]);
    } else {
        push(&mut args, &["-c:v", "libx264", "-preset", "medium"]);
        match rate {
            RateControl::TargetBitrate(bps) => {
                push(&mut args, &["-b:v"]);
                args.push(bps.to_string().into());
            }
            RateControl::QualityCrf(crf) => {
                push(&mut args, &["-crf"]);
                args.push(crf.to_string().into());
            }
        }
        if let Some(p) = pass {
            push(&mut args, &["-pass"]);
            args.push(p.number.to_string().into());
            push(&mut args, &["-passlogfile"]);
            args.push(p.log_prefix.as_os_str().into());
        }
    }
    push(
        &mut args,
        &["-profile:v", "high", "-fps_mode", "passthrough"],
    );
    // CPU pad requires CPU frames. Skipping an unnecessary pad is essential
    // for CUDA decoder -> NVENC zero-copy on ordinary even-sized inputs.
    if !cuda_decode {
        push(&mut args, &["-pix_fmt", "yuv420p"]);
        if needs_padding(media) {
            push(&mut args, &["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"]);
        }
    }
    if first_pass || media.audio.is_none() {
        push(&mut args, &["-an"]);
    } else if compatible_aac(media) {
        push(&mut args, &["-c:a", "copy"]);
    } else {
        push(
            &mut args,
            &[
                "-c:a",
                "aac",
                "-profile:a",
                "aac_low",
                "-b:a",
                "128k",
                "-ar",
                "48000",
            ],
        );
    }
    if !first_pass {
        push(
            &mut args,
            &["-movflags", "+faststart", "-max_muxing_queue_size", "1024"],
        );
    }
    push(
        &mut args,
        &["-progress", "pipe:1", "-stats_period", "0.5", "-nostats"],
    );
    if first_pass {
        push(&mut args, &["-f", "null", "-"]);
    } else {
        push(&mut args, &["-f", "mp4"]);
        args.push(output.as_os_str().into());
    }
    args
}

fn push(v: &mut Vec<OsString>, args: &[&str]) {
    v.extend(args.iter().map(OsString::from));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Media, Stream};

    fn media(bytes: u64) -> Media {
        Media {
            duration: 120.0,
            bytes,
            video: Stream {
                width: Some(1920),
                height: Some(1080),
                codec_name: Some("h264".into()),
                ..Default::default()
            },
            audio: None,
        }
    }

    #[test]
    fn target_math() {
        let m = media(200_000_000);
        assert_eq!(estimated_target_bytes(m.bytes, 90), 20_000_000);
        let bitrate = target_video_bitrate(&m, 90).unwrap();
        assert!((1_300_000..1_310_000).contains(&bitrate));
        assert!(target_video_bitrate(&m, 96).is_err());
    }

    #[test]
    fn two_pass_is_bitrate_driven_and_safe_for_paths() {
        let m = media(200_000_000);
        let args = encode_args(
            Path::new("input spaced.mp4"),
            Path::new("output spaced.mp4"),
            &m,
            Mode::Auto,
            RateControl::TargetBitrate(1_000_000),
            Some(Pass {
                number: 1,
                log_prefix: Path::new("statistics spaced"),
            }),
            false,
        );
        let strings: Vec<_> = args.iter().map(|x| x.to_string_lossy()).collect();
        assert!(strings.iter().any(|x| x == "-passlogfile"));
        assert!(strings.iter().any(|x| x == "statistics spaced"));
        assert_eq!(strings.last().unwrap(), "-");
        assert!(!strings.iter().any(|x| x == "-crf"));
        assert!(!strings.iter().any(|x| x == "output spaced.mp4"));
    }

    #[test]
    fn gpu_zero_copy_requires_even_geometry() {
        let mut m = media(200_000_000);
        assert!(can_try_cuda_decode(&m));
        let args = encode_args(
            Path::new("in.mp4"),
            Path::new("out.mp4"),
            &m,
            Mode::Fast,
            RateControl::TargetBitrate(1_000_000),
            None,
            true,
        );
        let strings: Vec<_> = args.iter().map(|x| x.to_string_lossy()).collect();
        assert!(strings.iter().any(|x| x == "cuda"));
        assert!(!strings.iter().any(|x| x == "-vf"));
        m.video.width = Some(1919);
        assert!(!can_try_cuda_decode(&m));
    }
}
