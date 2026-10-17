use crate::probe::Media;
use std::ffi::OsString;
use std::path::Path;

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
            Self::Auto => "Smart (quality probe, CPU)",
            Self::Balanced => "Balanced CPU",
            Self::Fast => "Fast NVIDIA",
            Self::Manual => "Manual CPU",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub crf: u8,
    pub vmaf_target: f64,
    pub output_dir: Option<std::path::PathBuf>,
    pub discard_if_larger: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            crf: 20,
            vmaf_target: 95.0,
            output_dir: None,
            discard_if_larger: true,
        }
    }
}

pub fn compatible_aac(media: &Media) -> bool {
    media.audio.as_ref().is_some_and(|a| {
        a.codec_name.as_deref() == Some("aac")
            && matches!(a.profile.as_deref(), Some("LC") | Some("AAC LC"))
    })
}

pub fn final_encode_args(
    input: &Path,
    output: &Path,
    media: &Media,
    mode: Mode,
    crf: u8,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-xerror",
        "-loglevel",
        "warning",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(input.as_os_str().into());
    push(
        &mut args,
        &["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"],
    );
    if mode == Mode::Fast {
        push(
            &mut args,
            &[
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p5",
                "-rc",
                "vbr",
                "-cq",
                "23",
                "-b:v",
                "0",
            ],
        );
    } else {
        push(&mut args, &["-c:v", "libx264", "-preset", "slow", "-crf"]);
        args.push(crf.to_string().into());
    }
    push(
        &mut args,
        &[
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-fps_mode",
            "passthrough",
        ],
    );
    push(&mut args, &["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"]);
    if media.audio.is_some() {
        if compatible_aac(media) {
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
                    "160k",
                    "-ar",
                    "48000",
                ],
            );
        }
    } else {
        push(&mut args, &["-an"]);
    }
    push(
        &mut args,
        &["-movflags", "+faststart", "-max_muxing_queue_size", "1024"],
    );
    push(
        &mut args,
        &[
            "-progress",
            "pipe:1",
            "-stats_period",
            "0.5",
            "-nostats",
            "-f",
            "mp4",
        ],
    );
    args.push(output.as_os_str().into());
    args
}

fn push(v: &mut Vec<OsString>, args: &[&str]) {
    v.extend(args.iter().map(OsString::from));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Media, Stream};
    #[test]
    fn output_keeps_spaced_path_as_one_argument() {
        let m = Media {
            duration: 10.,
            bytes: 1000,
            video: Stream::default(),
            audio: None,
        };
        let args = final_encode_args(
            Path::new("source sample.mp4"),
            Path::new("sample out.mp4"),
            &m,
            Mode::Manual,
            21,
        );
        assert!(args
            .iter()
            .any(|a| a.as_os_str() == std::ffi::OsStr::new("source sample.mp4")));
        assert_eq!(
            args.last().unwrap().as_os_str(),
            std::ffi::OsStr::new("sample out.mp4")
        );
        assert!(args
            .iter()
            .any(|a| a.as_os_str() == std::ffi::OsStr::new("-an")));
    }
}
