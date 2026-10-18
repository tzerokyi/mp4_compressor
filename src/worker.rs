use crate::encode::{self, encode_args, Mode, Pass, RateControl, Settings};
use crate::{probe, process};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::time::Instant;

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
    let started_at = Instant::now();
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
    let (real_mode, mut note) = match requested_mode {
        Mode::Fast => match process::nvenc_probe() {
            Ok(()) => (Mode::Fast, "NVENC VBR p3".to_string()),
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
                    format!("CPU fallback (NVENC unavailable: {reason})"),
                )
            }
        },
        Mode::Auto => (Mode::Auto, "CPU x264 2-pass".to_string()),
        Mode::Balanced => (Mode::Balanced, "CPU x264 1-pass".to_string()),
        Mode::Manual => {
            if !(14..=35).contains(&settings.crf) {
                return Err("CRF must be 14..35".into());
            }
            (Mode::Manual, format!("Manual x264 CRF {}", settings.crf))
        }
    };
    let rate = if real_mode == Mode::Manual {
        RateControl::QualityCrf(settings.crf)
    } else {
        let bps = encode::target_video_bitrate(&media, settings.reduction_percent)?;
        let target_mb = encode::estimated_target_bytes(media.bytes, settings.reduction_percent)
            as f64
            / 1_000_000.0;
        note = format!(
            "{note}, target -{}% ({target_mb:.1} MB), video {:.2} Mb/s",
            settings.reduction_percent,
            bps as f64 / 1_000_000.0
        );
        RateControl::TargetBitrate(bps)
    };
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    let temp = tempfile::Builder::new()
        .prefix(".mp4-compressor-working-")
        .tempdir_in(dest_dir)
        .map_err(|e| format!("Cannot create output temporary directory: {e}"))?;
    let candidate = temp.path().join("encoded.mp4");
    if real_mode == Mode::Auto {
        let logs = temp.path().join("x264-pass");
        let args = encode_args(
            input,
            &candidate,
            &media,
            real_mode,
            rate,
            Some(Pass {
                number: 1,
                log_prefix: &logs,
            }),
            false,
        );
        emit(
            tx,
            Event::Stage(input.to_owned(), "Pass 1/2 · bitrate analysis".into(), 0.0),
        );
        process::run_ffmpeg(&args, None, cancel, |seconds| {
            emit(
                tx,
                Event::Progress(
                    input.to_owned(),
                    (seconds as f64 / media.duration * 0.5).clamp(0.0, 0.5) as f32,
                ),
            );
        })?;
    }
    let logs = temp.path().join("x264-pass");
    let pass = (real_mode == Mode::Auto).then_some(Pass {
        number: 2,
        log_prefix: &logs,
    });
    let cuda_decode = real_mode == Mode::Fast && encode::can_try_cuda_decode(&media);
    let args = encode_args(
        input,
        &candidate,
        &media,
        real_mode,
        rate,
        pass,
        cuda_decode,
    );
    emit(
        tx,
        Event::Stage(
            input.to_owned(),
            format!("Encoding ({note})"),
            if real_mode == Mode::Auto { 0.5 } else { 0.0 },
        ),
    );
    let progress = |seconds: f32| {
        emit(
            tx,
            Event::Progress(
                input.to_owned(),
                ((seconds as f64 / media.duration).clamp(0.0, 0.99)
                    * if real_mode == Mode::Auto { 0.49 } else { 0.99 }
                    + if real_mode == Mode::Auto { 0.5 } else { 0.0 }) as f32,
            ),
        );
    };
    if let Err(first_error) = process::run_ffmpeg(&args, None, cancel, progress) {
        if !cuda_decode || cancel.load(Ordering::Relaxed) {
            return Err(first_error);
        }
        emit(
            tx,
            Event::Stage(
                input.to_owned(),
                format!("CUDA decode failed; retrying NVENC with CPU decode ({first_error})"),
                0.0,
            ),
        );
        let retry = encode_args(input, &candidate, &media, real_mode, rate, pass, false);
        process::run_ffmpeg(&retry, None, cancel, |_| {})?;
        note.push_str(" (CPU decode / NVENC encode)");
    } else if cuda_decode {
        note.push_str(" (CUDA decode / NVENC encode)");
    } else if real_mode == Mode::Fast {
        note.push_str(" (CPU decode / NVENC encode)");
    }
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
    let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
    let final_note = format!(
        "{note} · {elapsed:.1}s ({:.1}x realtime)",
        media.duration / elapsed
    );
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
            Ok(()) => return Ok(Some((output, media.bytes, check.bytes, final_note))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => match copy_no_replace(&candidate, &output) {
                Ok(true) => {
                    return Ok(Some((
                        output,
                        media.bytes,
                        check.bytes,
                        format!("{final_note} (copy fallback)"),
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
