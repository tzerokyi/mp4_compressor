use eframe::egui;

const ICON: &[u8] = include_bytes!("../assets/icon-128.rgba");

pub fn native_icon() -> egui::IconData {
    egui::IconData {
        rgba: ICON.to_vec(),
        width: 128,
        height: 128,
    }
}

pub fn texture(ctx: &egui::Context) -> egui::TextureHandle {
    ctx.load_texture(
        "mp4-compressor-brand-icon",
        egui::ColorImage::from_rgba_unmultiplied([128, 128], ICON),
        egui::TextureOptions::LINEAR,
    )
}
