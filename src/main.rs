//! rustjay-clipper — load a long video instantly (no transcode, no RAM copy),
//! scrub it, mark multiple in/out regions, and export each region as its own
//! clip. H.264 sources export via stream copy (no re-encode, cuts snap to the
//! previous keyframe); anything else re-encodes to H.264.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use eframe::egui;
use ffmpeg_next as ffmpeg;

const AV_TIME_BASE: f64 = 1_000_000.0;
const PREVIEW_MAX_W: u32 = 960;

/// Returns the bundled `ffmpeg` binary if it lives next to the executable
/// (macOS .app, Windows zip), otherwise falls back to `ffmpeg` on PATH.
fn bundled_ffmpeg() -> std::ffi::OsString {
    if let Ok(mut path) = std::env::current_exe() {
        path.pop();
        let candidate = path.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
        if candidate.exists() {
            return candidate.into_os_string();
        }
    }
    std::ffi::OsString::from("ffmpeg")
}

fn main() -> eframe::Result<()> {
    ffmpeg::init().expect("ffmpeg init");
    ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("rustjay-clipper")
            .with_inner_size([1100.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rustjay-clipper",
        options,
        Box::new(|_| Ok(Box::new(ClipperApp::default()))),
    )
}

// ── Seek-based preview player ──────────────────────────────────────────────

struct Player {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    stream_idx: usize,
    time_base: f64,
    pub fps: f64,
    pub duration_s: f64,
    pub is_h264: bool,
    pub src_dims: (u32, u32),
    out_dims: (u32, u32),
    /// Presentation time of the last decoded frame, for the sequential
    /// fast path (playback / small forward scrubs skip the seek).
    last_decoded_s: f64,
}

impl Player {
    fn open(path: &Path) -> Result<Self, String> {
        let ictx = ffmpeg::format::input(&path).map_err(|e| e.to_string())?;
        let stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or("no video stream")?;
        let stream_idx = stream.index();
        let time_base: f64 = stream.time_base().into();
        let fps: f64 = stream.avg_frame_rate().into();
        let fps = if fps > 0.0 { fps } else { 30.0 };
        let params = stream.parameters();
        let is_h264 = params.id() == ffmpeg::codec::Id::H264;

        let duration_s = if ictx.duration() > 0 {
            ictx.duration() as f64 / AV_TIME_BASE
        } else {
            stream.duration() as f64 * time_base
        };

        let decoder = ffmpeg::codec::context::Context::from_parameters(params)
            .map_err(|e| e.to_string())?
            .decoder()
            .video()
            .map_err(|e| e.to_string())?;

        let (sw, sh) = (decoder.width(), decoder.height());
        if sw == 0 || sh == 0 {
            return Err("stream has no dimensions".into());
        }
        let dw = sw.min(PREVIEW_MAX_W);
        let dh = (dw as u64 * sh as u64 / sw as u64) as u32 & !1;
        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            sw,
            sh,
            ffmpeg::util::format::Pixel::RGBA,
            dw,
            dh,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| e.to_string())?;

        Ok(Self {
            ictx,
            decoder,
            scaler,
            stream_idx,
            time_base,
            fps,
            duration_s: duration_s.max(0.0),
            is_h264,
            src_dims: (sw, sh),
            out_dims: (dw, dh),
            last_decoded_s: f64::NEG_INFINITY,
        })
    }

    /// Decode and return the frame at time `t` (seconds). `exact` decodes up
    /// to the requested frame; otherwise the first frame after the seek (the
    /// keyframe) is returned — fast enough to follow a scrub drag live.
    fn frame_at(&mut self, t: f64, exact: bool) -> Option<egui::ColorImage> {
        let t = t.clamp(0.0, self.duration_s.max(0.0));
        // Sequential fast path: short forward step needs no seek.
        let step = t - self.last_decoded_s;
        if !(0.0..=0.75).contains(&step) || !exact {
            let ts = (t * AV_TIME_BASE) as i64;
            self.ictx.seek(ts, ..ts).ok()?;
            self.decoder.flush();
        }

        let mut frame = ffmpeg::util::frame::video::Video::empty();
        let mut out: Option<egui::ColorImage> = None;
        'read: for (stream, packet) in self.ictx.packets() {
            if stream.index() != self.stream_idx {
                continue;
            }
            if self.decoder.send_packet(&packet).is_err() {
                continue;
            }
            while self.decoder.receive_frame(&mut frame).is_ok() {
                let pts_s = frame.timestamp().unwrap_or(0) as f64 * self.time_base;
                self.last_decoded_s = pts_s;
                if !exact || pts_s + 0.5 / self.fps >= t {
                    out = Some(self.to_image(&frame));
                    break 'read;
                }
            }
        }
        out
    }

    fn to_image(&mut self, frame: &ffmpeg::util::frame::video::Video) -> egui::ColorImage {
        let mut rgb = ffmpeg::util::frame::video::Video::empty();
        if self.scaler.run(frame, &mut rgb).is_err() {
            return egui::ColorImage::example();
        }
        let (w, h) = (self.out_dims.0 as usize, self.out_dims.1 as usize);
        let stride = rgb.stride(0);
        let data = rgb.data(0);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            pixels.extend_from_slice(&data[row * stride..row * stride + w * 4]);
        }
        egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels)
    }
}

// ── App ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
struct Region {
    start_s: f64,
    end_s: f64,
}

#[derive(Default)]
struct ClipperApp {
    path: Option<PathBuf>,
    player: Option<Player>,
    regions: Vec<Region>,
    pending_in: Option<f64>,
    playhead_s: f64,
    playing: bool,
    last_tick: Option<Instant>,
    /// One persistent texture, updated in place.
    tex: Option<egui::TextureHandle>,
    shown_at: Option<(f64, bool)>,
    force_reencode: bool,
    out_dir: Option<PathBuf>,
    export_rx: Option<mpsc::Receiver<String>>,
    exporting: bool,
    status: String,
}

impl ClipperApp {
    fn open_file(&mut self, path: PathBuf) {
        match Player::open(&path) {
            Ok(p) => {
                self.status = format!(
                    "{} — {:.1}s @ {:.2} fps, {}x{}, {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    p.duration_s,
                    p.fps,
                    p.src_dims.0,
                    p.src_dims.1,
                    if p.is_h264 { "h264 (stream-copy export)" } else { "will re-encode" },
                );
                self.player = Some(p);
                self.path = Some(path);
                self.regions.clear();
                self.pending_in = None;
                self.playhead_s = 0.0;
                self.playing = false;
                self.shown_at = None;
            }
            Err(e) => self.status = format!("Open failed: {e}"),
        }
    }

    fn commit_region(&mut self) {
        let Some(a) = self.pending_in.take() else {
            self.status = "Set an In point first (I).".into();
            return;
        };
        let b = self.playhead_s;
        let (start_s, end_s) = if b > a { (a, b) } else { (b, a) };
        if end_s - start_s < 0.05 {
            self.status = "Region too short.".into();
            return;
        }
        self.regions.push(Region { start_s, end_s });
        self.regions.sort_by(|x, y| x.start_s.total_cmp(&y.start_s));
        self.status = format!("Region added ({:.2}s–{:.2}s).", start_s, end_s);
    }

    fn start_export(&mut self, regions: Vec<Region>) {
        let Some(src) = self.path.clone() else { return };
        let Some(player) = &self.player else { return };
        if regions.is_empty() {
            self.status = "No regions to export.".into();
            return;
        }
        let copy = player.is_h264 && !self.force_reencode;
        let dir = self
            .out_dir
            .clone()
            .or_else(|| src.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let stem = src.file_stem().unwrap_or_default().to_string_lossy().into_owned();

        let (tx, rx) = mpsc::channel();
        self.export_rx = Some(rx);
        self.exporting = true;
        self.status = format!("Exporting {} clip(s)…", regions.len());

        std::thread::spawn(move || {
            for (i, r) in regions.iter().enumerate() {
                let out = dir.join(format!("{stem}_c{:02}.mp4", i + 1));
                let dur = r.end_s - r.start_s;
                let mut args: Vec<String> = vec![
                    "-y".into(),
                    "-ss".into(), format!("{:.3}", r.start_s),
                    "-i".into(), src.to_string_lossy().into_owned(),
                    "-t".into(), format!("{:.3}", dur),
                ];
                if copy {
                    // Stream copy: cut snaps back to the previous keyframe, so
                    // the clip may start slightly early but loses no content.
                    args.extend(["-c".into(), "copy".into(), "-avoid_negative_ts".into(), "make_zero".into()]);
                } else {
                    args.extend([
                        "-c:v".into(), "libx264".into(),
                        "-preset".into(), "veryfast".into(),
                        "-crf".into(), "18".into(),
                        "-pix_fmt".into(), "yuv420p".into(),
                        "-c:a".into(), "aac".into(),
                    ]);
                }
                args.extend(["-movflags".into(), "+faststart".into()]);
                args.push(out.to_string_lossy().into_owned());

                let res = Command::new(bundled_ffmpeg()).args(&args).output();
                let msg = match res {
                    Ok(o) if o.status.success() => {
                        format!("[{}/{}] {}", i + 1, regions.len(), out.display())
                    }
                    Ok(o) => format!(
                        "[{}/{}] FAILED: {}",
                        i + 1,
                        regions.len(),
                        String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("?")
                    ),
                    Err(e) => format!("[{}/{}] ffmpeg launch failed: {e}", i + 1, regions.len()),
                };
                let _ = tx.send(msg);
            }
            let _ = tx.send("__done__".into());
        });
    }

    fn timeline_bar(&mut self, ui: &mut egui::Ui, duration: f64) {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 56.0),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);

        let to_x = |t: f64| rect.left() + (t / duration).clamp(0.0, 1.0) as f32 * rect.width();

        for (i, r) in self.regions.iter().enumerate() {
            let span = egui::Rect::from_min_max(
                egui::pos2(to_x(r.start_s), rect.top() + 6.0),
                egui::pos2(to_x(r.end_s), rect.bottom() - 6.0),
            );
            let hue = (i as f32 * 0.61) % 1.0;
            let color = egui::ecolor::Hsva::new(hue, 0.55, 0.75, 0.55);
            painter.rect_filled(span, 3.0, color);
            painter.text(
                span.left_top() + egui::vec2(3.0, 1.0),
                egui::Align2::LEFT_TOP,
                format!("{}", i + 1),
                egui::FontId::proportional(10.0),
                egui::Color32::BLACK,
            );
        }

        if let Some(a) = self.pending_in {
            painter.vline(
                to_x(a),
                rect.y_range(),
                egui::Stroke::new(2.0_f32, egui::Color32::YELLOW),
            );
        }
        painter.vline(
            to_x(self.playhead_s),
            rect.y_range(),
            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 80, 80)),
        );

        if response.clicked() || response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                self.playhead_s = frac as f64 * duration;
                self.playing = false;
            }
        }
        // While dragging, show the nearest keyframe (fast); exact on release.
        let exact = !response.dragged();
        self.refresh_preview(ui.ctx(), exact);
    }

    fn refresh_preview(&mut self, ctx: &egui::Context, exact: bool) {
        let Some(player) = &mut self.player else { return };
        let want = (self.playhead_s, exact);
        if self.shown_at == Some(want) {
            return;
        }
        if let Some(img) = player.frame_at(self.playhead_s, exact) {
            match &mut self.tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR))
                }
            }
            self.shown_at = Some(want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smallest check that fails if the seek/decode core breaks: open a synth
    // clip, grab an exact frame mid-file, then step forward sequentially.
    #[test]
    fn player_seeks_and_steps() {
        ffmpeg::init().unwrap();
        let ok = Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success());
        if !ok.unwrap_or(false) {
            eprintln!("skipping: ffmpeg CLI not available");
            return;
        }
        let tmp = std::env::temp_dir().join("rustjay_clipper_test.mp4");
        let synth = Command::new("ffmpeg")
            .args([
                "-y", "-f", "lavfi", "-i", "testsrc2=duration=4:size=320x180:rate=30",
                "-c:v", "libx264", "-pix_fmt", "yuv420p",
                tmp.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(synth.status.success());

        let mut p = Player::open(&tmp).unwrap();
        assert!(p.is_h264);
        assert!((p.duration_s - 4.0).abs() < 0.2, "duration {}", p.duration_s);

        let img = p.frame_at(2.0, true).expect("exact mid-file frame");
        assert_eq!(img.size, [320, 180]);
        // Sequential step must not fall back on a full seek and must advance.
        let before = p.last_decoded_s;
        p.frame_at(before + 1.0 / p.fps, true).expect("stepped frame");
        assert!(p.last_decoded_s > before);
        let _ = std::fs::remove_file(&tmp);
    }
}

fn fmt_ts(t: f64) -> String {
    let m = (t / 60.0) as u64;
    format!("{m:02}:{:06.3}", t - m as f64 * 60.0)
}

impl eframe::App for ClipperApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Export progress.
        if let Some(rx) = &self.export_rx {
            while let Ok(msg) = rx.try_recv() {
                if msg == "__done__" {
                    self.exporting = false;
                    self.status = "Export finished.".into();
                    self.export_rx = None;
                    break;
                }
                self.status = msg;
            }
        }

        let duration = self.player.as_ref().map(|p| p.duration_s).unwrap_or(0.0);
        let fps = self.player.as_ref().map(|p| p.fps).unwrap_or(30.0);

        // Keyboard: space play/pause, I/O marks, arrows step one frame.
        if self.player.is_some() && !ctx.wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Space) {
                    self.playing = !self.playing;
                    self.last_tick = None;
                }
                if i.key_pressed(egui::Key::I) {
                    self.pending_in = Some(self.playhead_s);
                }
                if i.key_pressed(egui::Key::O) {
                    self.commit_region();
                }
                if i.key_pressed(egui::Key::ArrowRight) {
                    self.playhead_s = (self.playhead_s + 1.0 / fps).min(duration);
                    self.playing = false;
                }
                if i.key_pressed(egui::Key::ArrowLeft) {
                    self.playhead_s = (self.playhead_s - 1.0 / fps).max(0.0);
                    self.playing = false;
                }
            });
        }

        if self.playing {
            let now = Instant::now();
            let dt = self.last_tick.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
            self.last_tick = Some(now);
            self.playhead_s += dt;
            if self.playhead_s >= duration {
                self.playhead_s = duration;
                self.playing = false;
            }
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("📂 Open video…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_file() {
                        self.open_file(p);
                    }
                }
                if let Some(path) = &self.path {
                    ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                }
                ui.separator();
                ui.checkbox(&mut self.force_reencode, "Force re-encode (frame-accurate)");
                if ui.button("Output folder…").clicked() {
                    self.out_dir = rfd::FileDialog::new().pick_folder();
                }
                if let Some(d) = &self.out_dir {
                    ui.weak(d.to_string_lossy());
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });

        egui::SidePanel::right("regions").min_width(240.0).show(ctx, |ui| {
            ui.heading("Regions");
            ui.small("I = set In · O = commit region · Space = play · ←/→ = step");
            ui.separator();
            let mut remove: Option<usize> = None;
            let mut export_one: Option<usize> = None;
            for (i, r) in self.regions.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!(
                        "{:>2}  {} → {}  ({:.2}s)",
                        i + 1,
                        fmt_ts(r.start_s),
                        fmt_ts(r.end_s),
                        r.end_s - r.start_s
                    ));
                    if ui.small_button("▶").on_hover_text("Jump to start").clicked() {
                        self.playhead_s = r.start_s;
                    }
                    if ui.add_enabled(!self.exporting, egui::Button::new("💾").small()).clicked() {
                        export_one = Some(i);
                    }
                    if ui.small_button("✕").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.regions.remove(i);
            }
            if let Some(i) = export_one {
                let r = vec![self.regions[i]];
                self.start_export(r);
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Set In (I)").clicked() {
                    self.pending_in = Some(self.playhead_s);
                }
                if ui.button("Add region (O)").clicked() {
                    self.commit_region();
                }
            });
            ui.add_space(6.0);
            let n = self.regions.len();
            if ui
                .add_enabled(!self.exporting && n > 0, egui::Button::new(format!("Export all ({n})")))
                .clicked()
            {
                self.start_export(self.regions.clone());
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.player.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.heading("Open a video to start clipping");
                });
                return;
            }

            if let Some(tex) = &self.tex {
                let avail = ui.available_size() - egui::vec2(0.0, 90.0);
                let size = tex.size_vec2();
                let scale = (avail.x / size.x).min(avail.y / size.y).min(2.0).max(0.05);
                ui.vertical_centered(|ui| {
                    ui.image((tex.id(), size * scale));
                });
            }

            ui.horizontal(|ui| {
                let icon = if self.playing { "⏸" } else { "▶" };
                if ui.button(icon).clicked() {
                    self.playing = !self.playing;
                    self.last_tick = None;
                }
                ui.monospace(format!("{} / {}", fmt_ts(self.playhead_s), fmt_ts(duration)));
                if let Some(a) = self.pending_in {
                    ui.colored_label(egui::Color32::YELLOW, format!("In @ {}", fmt_ts(a)));
                }
            });

            self.timeline_bar(ui, duration.max(0.001));
        });
    }
}
