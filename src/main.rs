mod app;
mod icon;

use mp4_compressor::encode::{Mode, Settings};
use mp4_compressor::{process, worker};
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, mpsc, Arc};

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|x| x == "--help") {
        println!("mp4-compressor [--check] [--headless INPUT ...] [--mode auto|balanced|fast|manual] [--reduction 80] [--crf 25]");
        return Ok(());
    }
    if args.iter().any(|x| x == "--check") {
        println!("{}", process::check_dependencies());
        return Ok(());
    }
    if args.iter().any(|x| x == "--headless") {
        let mut settings = Settings::default();
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut idx = 0;
        while idx < args.len() {
            match args[idx].as_str() {
                "--headless" => {}
                "--mode" => {
                    idx += 1;
                    let raw = args.get(idx).map(String::as_str).unwrap_or("");
                    settings.mode = match raw {
                        "auto" => Mode::Auto,
                        "balanced" => Mode::Balanced,
                        "fast" => Mode::Fast,
                        "manual" => Mode::Manual,
                        _ => {
                            eprintln!("Unknown mode: {raw}");
                            std::process::exit(2);
                        }
                    };
                }
                "--crf" => {
                    idx += 1;
                    let raw = args.get(idx).map(String::as_str).unwrap_or("");
                    settings.crf = raw.parse().unwrap_or_else(|_| {
                        eprintln!("Invalid CRF: {raw}");
                        std::process::exit(2);
                    });
                }
                "--reduction" => {
                    idx += 1;
                    let raw = args.get(idx).map(String::as_str).unwrap_or("");
                    settings.reduction_percent = raw
                        .parse::<u8>()
                        .ok()
                        .filter(|value| *value <= 95)
                        .unwrap_or_else(|| {
                            eprintln!("Reduction must be an integer from 0 to 95: {raw}");
                            std::process::exit(2);
                        });
                }
                raw if raw.starts_with("--") => {
                    eprintln!("Unknown argument: {raw}");
                    std::process::exit(2);
                }
                raw => paths.push(PathBuf::from(raw)),
            }
            idx += 1;
        }
        if paths.is_empty() {
            eprintln!("No input paths specified");
            std::process::exit(2);
        }
        let (tx, rx) = mpsc::channel();
        worker::run_batch(paths, settings, tx, Arc::new(AtomicBool::new(false)));
        let mut failed = false;
        for ev in rx {
            println!("{ev:?}");
            if matches!(
                &ev,
                worker::Event::Failed(_, _) | worker::Event::Skipped(_, _)
            ) {
                failed = true;
            }
            if matches!(&ev, worker::Event::Done) {
                break;
            }
        }
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1160.0, 900.0])
            .with_min_inner_size([820.0, 700.0])
            .with_icon(std::sync::Arc::new(icon::native_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "mp4 compressor",
        native,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
