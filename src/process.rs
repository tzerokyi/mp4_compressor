use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

pub fn parse_time(line: &str) -> Option<f64> {
    let (key, raw) = line.trim().split_once('=')?;
    match key {
        "out_time_us" | "out_time_ms" => raw.parse::<f64>().ok().map(|x| x / 1_000_000.0),
        "out_time" => {
            let bits: Vec<&str> = raw.split(':').collect();
            if bits.len() != 3 {
                return None;
            }
            let h = bits[0].parse::<f64>().ok()?;
            let m = bits[1].parse::<f64>().ok()?;
            let s = bits[2].parse::<f64>().ok()?;
            Some(h * 3600.0 + m * 60.0 + s)
        }
        _ => None,
    }
    .filter(|v| v.is_finite() && *v >= 0.0)
}

pub fn run_ffmpeg(
    args: &[OsString],
    cwd: Option<&Path>,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(f32),
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Cannot start ffmpeg: {err}"))?;
    let out = child.stdout.take().ok_or("Missing ffmpeg stdout")?;
    let err = child.stderr.take().ok_or("Missing ffmpeg stderr")?;
    let (tx, rx) = std::sync::mpsc::channel::<f64>();
    let output_thread = thread::spawn(move || {
        for text in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some(time) = parse_time(&text) {
                let _ = tx.send(time);
            }
        }
    });
    let error_tail = Arc::new(Mutex::new(String::new()));
    let error_copy = Arc::clone(&error_tail);
    let error_thread = thread::spawn(move || {
        let mut reader = BufReader::new(err);
        let mut bytes = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let chunk = String::from_utf8_lossy(&bytes[..count]);
            if let Ok(mut s) = error_copy.lock() {
                s.push_str(&chunk);
                if s.len() > 16000 {
                    let mut start = s.len() - 12000;
                    while !s.is_char_boundary(start) {
                        start += 1;
                    }
                    s.drain(..start);
                }
            }
        }
    });
    let status = loop {
        while let Ok(seconds) = rx.try_recv() {
            on_progress(seconds as f32);
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = output_thread.join();
            let _ = error_thread.join();
            return Err("Cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(75)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_thread.join();
                let _ = error_thread.join();
                return Err(format!("ffmpeg wait error: {err}"));
            }
        }
    };
    let _ = output_thread.join();
    let _ = error_thread.join();
    while let Ok(seconds) = rx.try_recv() {
        on_progress(seconds as f32);
    }
    if status.success() {
        Ok(())
    } else {
        let tail = error_tail.lock().map(|x| x.clone()).unwrap_or_default();
        Err(format!("ffmpeg failed ({status}): {tail}"))
    }
}

fn encoder_names() -> &'static String {
    static ENCODERS: OnceLock<String> = OnceLock::new();
    ENCODERS.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    })
}

fn filter_names() -> &'static String {
    static FILTERS: OnceLock<String> = OnceLock::new();
    FILTERS.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    })
}

pub fn has_encoder(name: &str) -> bool {
    encoder_names()
        .lines()
        .any(|s| s.split_whitespace().nth(1) == Some(name))
}

pub fn has_filter(name: &str) -> bool {
    filter_names()
        .lines()
        .any(|s| s.split_whitespace().nth(1) == Some(name))
}

pub fn nvenc_probe() -> Result<(), String> {
    if !has_encoder("h264_nvenc") {
        return Err("FFmpeg lacks h264_nvenc (check ffmpeg -encoders)".into());
    }
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x240:r=30",
            "-frames:v",
            "3",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| format!("Cannot launch ffmpeg: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let details = String::from_utf8_lossy(&output.stderr);
    let detail = details.trim();
    let diagnostic = if detail.is_empty() {
        "No FFmpeg error output"
    } else {
        detail
    };
    Err(format!("NVENC probe failed: {diagnostic}"))
}

pub fn nvenc_works() -> bool {
    nvenc_probe().is_ok()
}

pub fn check_dependencies() -> String {
    let ffprobe = Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    let x264 = has_encoder("libx264");
    let aac = has_encoder("aac");
    let ffv1 = has_encoder("ffv1");
    let vmaf = has_filter("libvmaf");
    let nvenc = match nvenc_probe() {
        Ok(()) => "available".to_string(),
        Err(reason) => format!("unavailable: {reason}"),
    };
    format!(
        "ffprobe={ffprobe}, libx264={x264}, aac={aac}, ffv1={ffv1}, libvmaf={vmaf}, NVENC={nvenc}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ffmpeg_progress_formats() {
        assert_eq!(parse_time("out_time_us=2500000"), Some(2.5));
        assert_eq!(parse_time("out_time=00:00:02.500000"), Some(2.5));
        assert_eq!(parse_time("progress=continue"), None);
    }
}
