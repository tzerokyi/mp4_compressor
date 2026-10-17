use crate::encode::{final_encode_args, Mode, Settings};
use crate::{probe, process, quality};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

#[derive(Debug)]
pub enum Event {
    Started(PathBuf),
    Stage(PathBuf, String, f32),
    Progress(PathBuf, f32),
    Finished {
        input: PathBuf,
        output: PathBuf,
        before: u64,
        after: u64,
        note: String,
    },
    Skipped(PathBuf, String),
    Failed(PathBuf, String),
    Done,
}

fn emit(tx: &Sender<Event>, ev: Event) {
    let _ = tx.send(ev);
}

pub fn run_batch(
    inputs: Vec<PathBuf>,
    settings: Settings,
    tx: Sender<Event>,
    cancel: Arc<AtomicBool>,
) {
    for input in inputs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        emit(&tx, Event::Started(input.clone()));
        match process_one(&input, &settings, &tx, &cancel) {
            Ok(Some((output, before, after, note))) => emit(
                &tx,
                Event::Finished {
                    input: input.clone(),
                    output,
                    before,
                    after,
                    note,
                },
            ),
            Ok(None) => emit(
                &tx,
                Event::Skipped(input.clone(), "No reduction in size".into()),
            ),
            Err(err) => {
                if cancel.load(Ordering::Relaxed) {
                    emit(&tx, Event::Skipped(input.clone(), "Cancelled".into()));
                    break;
                }
                emit(&tx, Event::Failed(input.clone(), err));
            }
        }
    }
    emit(&tx, Event::Done);
}

fn process_one(
    input: &Path,
    settings: &Settings,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
) -> Result<Option<(PathBuf, u64, u64, String)>, String> {
    emit(
        tx,
        Event::Stage(input.to_owned(), "Inspecting source".into(), 0.0),
    );
    let media = probe::inspect(input)?;
    probe::reject_unsafe_source(&media)?;
    if !process::has_encoder("libx264") {
        return Err("FFmpeg libx264 encoder not available".into());
    }
    let source_dir = input
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dest_dir = settings.output_dir.as_deref().unwrap_or(source_dir);
    if !dest_dir.is_dir() {
        return Err(format!("Output folder missing: {}", dest_dir.display()));
    }
    let requested_mode = settings.mode;
    let (real_mode, crf, note) = match requested_mode {
        Mode::Fast => match process::nvenc_probe() {
            Ok(()) => (Mode::Fast, 23, "Hardware NVENC / CQ 23".to_string()),
            Err(reason) => {
                emit(
                    tx,
                    Event::Stage(
                        input.to_owned(),
                        format!("NVENC unavailable: {reason}; continuing on CPU"),
                        0.0,
                    ),
                );
                (
                    Mode::Balanced,
                    20,
                    format!("CPU fallback (NVENC unavailable: {reason})"),
                )
            }
        },
        Mode::Auto
            if process::has_encoder("ffv1")
                && (process::has_filter("libvmaf") || process::has_filter("ssim")) =>
        {
            let metric = if process::has_filter("libvmaf") {
                quality::Metric::Vmaf
            } else {
                quality::Metric::Ssim
            };
            let outcome = quality::search(
                input,
                media.duration,
                metric,
                settings.vmaf_target,
                cancel,
                |fraction, progress_msg| {
                    emit(
                        tx,
                        Event::Stage(input.to_owned(), progress_msg.to_owned(), fraction),
                    );
                },
            )?;
            let note = if outcome.met_target {
                format!(
                    "Smart {}: CRF {}, sample score {:.4}",
                    metric.name(),
                    outcome.crf,
                    outcome.score
                )
            } else {
                format!(
                    "Smart {}: target {:.2} NOT met (sample {:.2}); CRF {} best-effort quality",
                    metric.name(),
                    metric.cutoff(settings.vmaf_target),
                    outcome.score,
                    outcome.crf
                )
            };
            (Mode::Balanced, outcome.crf, note)
        }
        Mode::Auto => {
            emit(
                tx,
                Event::Stage(
                    input.to_owned(),
                    "VMAF/SSIM/FFV1 missing; explicit fallback to CRF 20".into(),
                    0.0,
                ),
            );
            (
                Mode::Balanced,
                20,
                "Quality measurement unavailable: used CRF 20".to_string(),
            )
        }
        Mode::Balanced => (Mode::Balanced, 20, "CPU x264 CRF 20".to_string()),
        Mode::Manual => {
            if !(14..=35).contains(&settings.crf) {
                return Err("CRF must be 14..35".into());
            }
            (
                Mode::Manual,
                settings.crf,
                format!("Manual CRF {}", settings.crf),
            )
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    let temp = tempfile::Builder::new()
        .prefix(".mp4-compressor-working-")
        .tempdir_in(dest_dir)
        .map_err(|e| format!("Cannot create output temporary directory: {e}"))?;
    let candidate = temp.path().join("encoded.mp4");
    let args = final_encode_args(input, &candidate, &media, real_mode, crf);
    emit(
        tx,
        Event::Stage(input.to_owned(), format!("Encoding ({note})"), 0.0),
    );
    process::run_ffmpeg(&args, None, cancel, |seconds| {
        emit(
            tx,
            Event::Progress(
                input.to_owned(),
                (seconds as f64 / media.duration).clamp(0.0, 0.99) as f32,
            ),
        );
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    emit(
        tx,
        Event::Stage(input.to_owned(), "Verifying output".into(), 0.99),
    );
    let check = probe::inspect(&candidate)?;
    if check.video.codec_name.as_deref() != Some("h264")
        || check.video.pix_fmt.as_deref() != Some("yuv420p")
    {
        return Err("Final output is not H.264 yuv420p".into());
    }
    if check
        .audio
        .as_ref()
        .is_some_and(|a| a.codec_name.as_deref() != Some("aac"))
    {
        return Err("Final audio is not AAC".into());
    }
    if media.audio.is_some() != check.audio.is_some() {
        return Err("Audio stream missing or unexpectedly added".into());
    }
    if !probe::duration_is_close(media.duration, check.duration, probe::fps(&media.video)) {
        return Err(format!(
            "Duration mismatch: original {:.3}s, result {:.3}s",
            media.duration, check.duration
        ));
    }
    let expect_w = media.video.width.unwrap_or(0) + media.video.width.unwrap_or(0) % 2;
    let expect_h = media.video.height.unwrap_or(0) + media.video.height.unwrap_or(0) % 2;
    if check.video.width != Some(expect_w) || check.video.height != Some(expect_h) {
        return Err("Unexpected output dimensions".into());
    }
    verify_decode(&candidate, check.duration, cancel)?;
    if settings.discard_if_larger && check.bytes >= media.bytes {
        return Ok(None);
    }
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    for n in 1..=1000 {
        let filename = if n == 1 {
            format!("{stem}_compressed.mp4")
        } else {
            format!("{stem}_compressed_{n}.mp4")
        };
        let output = dest_dir.join(filename);
        if output == input {
            continue;
        }
        match std::fs::hard_link(&candidate, &output) {
            Ok(()) => return Ok(Some((output, media.bytes, check.bytes, note))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => match copy_no_replace(&candidate, &output) {
                Ok(true) => {
                    return Ok(Some((
                        output,
                        media.bytes,
                        check.bytes,
                        format!("{note} (copy fallback)"),
                    )))
                }
                Ok(false) => continue,
                Err(e) => return Err(format!("Cannot publish output: {e}")),
            },
        }
    }
    Err("Too many output filename conflicts".into())
}

fn copy_no_replace(source: &Path, destination: &Path) -> std::io::Result<bool> {
    use std::io::{ErrorKind, Write};
    let mut target = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    let result = (|| -> std::io::Result<()> {
        let mut from = std::fs::File::open(source)?;
        std::io::copy(&mut from, &mut target)?;
        target.flush()?;
        target.sync_all()
    })();
    drop(target);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result.map(|()| true)
}

fn verify_decode(path: &Path, duration: f64, cancel: &AtomicBool) -> Result<(), String> {
    for start in [0.0, (duration - 2.0).max(0.0)] {
        let mut args: Vec<std::ffi::OsString> = [
            "-hide_banner",
            "-nostdin",
            "-y",
            "-xerror",
            "-loglevel",
            "error",
            "-ss",
        ]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
        args.push(start.to_string().into());
        args.push("-i".into());
        args.push(path.as_os_str().into());
        args.extend(
            [
                "-t",
                "2",
                "-map",
                "0:v:0",
                "-an",
                "-f",
                "null",
                "-progress",
                "pipe:1",
                "-nostats",
                "-",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        );
        process::run_ffmpeg(&args, None, cancel, |_| {})?;
        if duration <= 2.0 {
            break;
        }
    }
    Ok(())
}
