//! Compression jobs. Video, audio, and images go through FFmpeg; anything else
//! (and whole folders) is packed into a .zip by [`crate::archive`].
//!
//! Every job writes a brand-new file next to the original (or into the chosen
//! folder) and never touches the original. Partial output is deleted if a job
//! fails or is cancelled, and output that came out larger than the original is
//! thrown away rather than kept.

use crate::archive;
use crate::media::{self, GpuEncoder, MediaInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// The error string a cancelled job returns; the UI treats it as "Cancelled".
pub const CANCELLED: &str = "cancelled";

/// Targets count megabytes in thousands, the stricter reading, so "10 MB" also
/// fits sites that count in 1024s.
const MB: f64 = 1_000_000.0;
/// Aim a little under the target to leave room for the container.
const HEADROOM: f64 = 0.97;
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

// ------------------------------------------------------------------ job types

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: u32,
    pub path: String,
    /// `None` saves next to the original.
    pub output_dir: Option<String>,
    /// Appended to the file name, e.g. "-compressed".
    pub suffix: String,
    pub settings: Settings,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Settings {
    Video(VideoSettings),
    Audio(AudioSettings),
    Image(ImageSettings),
    Other(ArchiveSettings),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSettings {
    /// "size" (hit a target file size) or "quality" (constant quality).
    pub mode: String,
    pub target_mb: f64,
    /// 1 (smallest file) to 5 (best quality).
    pub quality: u8,
    /// "h264" | "hevc" | "av1"
    pub codec: String,
    /// "auto" | "original" | a short-side height such as "1080"
    pub resolution: String,
    /// "original" | "60" | "30" | "24"
    pub fps: String,
    /// "none" or a bitrate in kbps such as "128"
    pub audio: String,
    /// "fast" | "balanced" | "small"
    pub speed: String,
    pub gpu: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSettings {
    /// "mp3" | "m4a" | "opus"
    pub format: String,
    pub bitrate: u32,
    pub mono: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    /// "keep" | "jpg" | "webp" | "png"
    pub format: String,
    /// 1 to 100, for JPEG and WebP.
    pub quality: u8,
    /// Longest side in pixels; 0 keeps the original size.
    pub max_size: u32,
    /// PNG only: quantize to a 256-colour palette.
    pub reduce_colors: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSettings {
    /// "fast" | "normal" | "max"
    pub level: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    /// "done" or "skipped" (nothing was saved; the original is untouched).
    pub status: &'static str,
    pub output: Option<String>,
    pub input_size: u64,
    pub output_size: u64,
    pub note: Option<String>,
}

impl JobResult {
    fn skipped(input_size: u64, note: impl Into<String>) -> Self {
        JobResult {
            status: "skipped",
            output: None,
            input_size,
            output_size: input_size,
            note: Some(note.into()),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress<'a> {
    id: u32,
    /// `None` while the length of the work isn't known.
    percent: Option<f64>,
    phase: &'a str,
    /// Seconds left, when it can be estimated.
    eta: Option<f64>,
}

// ------------------------------------------------------------------ control

/// Lets the UI cancel a running job: sets a flag checked between steps and
/// kills the FFmpeg process that's currently running for it.
#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
    partial: Mutex<Option<PathBuf>>,
}

impl Control {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Used when the app quits mid-job: stop FFmpeg and remove its half-written file.
    pub fn abandon(&self) {
        self.cancel();
        if let Some(path) = self.partial.lock().unwrap().take() {
            for _ in 0..10 {
                if fs::remove_file(&path).is_ok() || !path.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Running jobs by id.
#[derive(Default)]
pub struct Jobs(pub Mutex<HashMap<u32, Arc<Control>>>);

pub struct Ctx<'a> {
    app: &'a AppHandle,
    id: u32,
    control: &'a Control,
}

impl Ctx<'_> {
    pub fn progress(&self, percent: Option<f64>, phase: &str, eta: Option<f64>) {
        let _ = self.app.emit("progress", Progress { id: self.id, percent, phase, eta });
    }

    pub fn check(&self) -> Result<(), String> {
        if self.control.is_cancelled() {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    }
}

/// An output file that is deleted on drop unless the job finished with it.
struct Output<'a> {
    path: PathBuf,
    control: &'a Control,
    keep: bool,
}

impl<'a> Output<'a> {
    fn new(control: &'a Control, path: PathBuf) -> Self {
        *control.partial.lock().unwrap() = Some(path.clone());
        Output { path, control, keep: false }
    }

    fn size(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        self.control.partial.lock().unwrap().take();
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ------------------------------------------------------------------ entry point

pub fn run(app: &AppHandle, control: &Control, job: &Job) -> Result<JobResult, String> {
    let ctx = Ctx { app, id: job.id, control };
    let input = PathBuf::from(&job.path);
    if !input.exists() {
        return Err("The original file isn't there any more.".into());
    }
    let input_size = archive::size_of(&input);

    let out_dir = match job.output_dir.as_deref() {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Couldn't use the output folder {}: {e}", out_dir.display()))?;

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let named = |ext: &str| unique_path(&out_dir, &format!("{stem}{}", job.suffix), ext);

    match &job.settings {
        Settings::Video(s) => video(&ctx, &input, input_size, s, named("mp4")),
        Settings::Audio(s) => audio(&ctx, &input, input_size, s, &named),
        Settings::Image(s) => image(&ctx, &input, input_size, s, &named),
        Settings::Other(s) => {
            // A .zip is named after the original: "Photos" -> "Photos.zip".
            let name = input
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive".into());
            let base = if input.is_dir() { name } else { stem };
            other(&ctx, &input, input_size, s, unique_path(&out_dir, &base, "zip"))
        }
    }
}

/// `dir/base.ext`, or `dir/base (2).ext` and so on if that's taken.
fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    candidate
}

/// Wraps up a job whose output was written: drops output that's no smaller
/// than the original unless `keep_if_larger`.
fn finish(
    mut out: Output,
    input_size: u64,
    notes: Vec<String>,
    keep_if_larger: bool,
    not_smaller: &str,
) -> Result<JobResult, String> {
    let output_size = out.size();
    if output_size == 0 {
        return Err("Nothing was written. The file may be damaged or in an unusual format.".into());
    }
    if output_size >= input_size && !keep_if_larger {
        return Ok(JobResult::skipped(input_size, not_smaller));
    }
    out.keep = true;
    Ok(JobResult {
        status: "done",
        output: Some(out.path.to_string_lossy().into_owned()),
        input_size,
        output_size,
        note: (!notes.is_empty()).then(|| notes.join(" ")),
    })
}

// ------------------------------------------------------------------ running FFmpeg

/// Runs one FFmpeg pass, reporting progress across `span` (percent of the whole job).
fn run_ffmpeg(
    ctx: &Ctx,
    args: &[OsString],
    cwd: Option<&Path>,
    duration: Option<f64>,
    phase: &str,
    span: (f64, f64),
) -> Result<(), String> {
    ctx.check()?;
    let mut cmd = media::ffmpeg(true);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Couldn't start FFmpeg ({e}). Try reinstalling the app."))?;
    media::tie_to_app(&child);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    *ctx.control.child.lock().unwrap() = Some(child);
    if ctx.control.is_cancelled() {
        // Cancelled between the check above and storing the child.
        ctx.control.cancel();
    }

    let errors = thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut text);
        text
    });

    ctx.progress(duration.map(|_| span.0), phase, None);
    let (mut out_time, mut speed) = (0.0f64, None::<f64>);
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<f64>() {
                out_time = us / 1e6;
            }
        } else if let Some(v) = line.strip_prefix("speed=") {
            speed = v.trim().trim_end_matches('x').parse().ok().filter(|s: &f64| *s > 0.0);
        } else if line.starts_with("progress=") {
            if let Some(total) = duration {
                let frac = (out_time / total).clamp(0.0, 1.0);
                let percent = span.0 + (span.1 - span.0) * frac;
                // Only the last pass knows how long is really left.
                let eta = speed
                    .filter(|_| span.1 >= 100.0)
                    .map(|s| (total - out_time).max(0.0) / s);
                ctx.progress(Some(percent), phase, eta);
            }
        }
    }

    let status = ctx.control.child.lock().unwrap().take().map(|mut c| c.wait());
    let stderr = errors.join().unwrap_or_default();
    ctx.check()?;
    match status {
        Some(Ok(s)) if s.success() => Ok(()),
        _ => Err(ffmpeg_error(&stderr)),
    }
}

/// The last few meaningful lines FFmpeg printed, for showing in the UI.
fn ffmpeg_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("x265 [info]") && !l.starts_with("Svt["))
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" ");
    if tail.is_empty() {
        "FFmpeg stopped unexpectedly.".into()
    } else {
        tail.chars().take(400).collect()
    }
}

/// A scratch folder for two-pass statistics, removed when dropped.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(id: u32) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("ccc-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create a temporary folder: {e}"))?;
        Ok(ScratchDir(dir))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Builds an argument list from a mix of string-ish things.
macro_rules! args {
    ($($x:expr),* $(,)?) => { vec![$(OsString::from($x)),*] };
}

// ------------------------------------------------------------------ video

#[derive(Clone)]
enum Encoder {
    /// "libx264", "libx265", "libsvtav1", or "libaom-av1"
    Software(String),
    Gpu(GpuEncoder),
}

impl Encoder {
    /// x264 and x265 get two passes for an accurate size. AV1 on the CPU is slow
    /// enough already; it runs once and relies on the refit check instead.
    fn two_pass(&self) -> bool {
        matches!(self, Encoder::Software(n) if n == "libx264" || n == "libx265")
    }

    /// Codec options. `q` is the quality step 0 (smallest) to 4 (best);
    /// `kbps` switches from constant quality to a bitrate.
    fn args(&self, codec: &str, speed: usize, q: usize, kbps: Option<f64>) -> Vec<OsString> {
        let rate = |k: f64| format!("{}k", k.round().max(1.0));
        let mut a: Vec<OsString> = Vec::new();
        match self {
            Encoder::Software(name) => {
                a.extend(args!["-c:v", name.as_str(), "-pix_fmt", "yuv420p"]);
                let crf = match name.as_str() {
                    "libx264" => {
                        a.extend(args!["-preset", ["veryfast", "medium", "slow"][speed]]);
                        [30, 27, 24, 21, 18][q]
                    }
                    "libx265" => {
                        a.extend(args!["-preset", ["veryfast", "medium", "slow"][speed], "-tag:v", "hvc1"]);
                        [32, 29, 26, 23, 20][q]
                    }
                    "libsvtav1" => {
                        a.extend(args!["-preset", ["10", "8", "6"][speed]]);
                        [44, 39, 34, 29, 24][q]
                    }
                    _ => {
                        // libaom-av1: its real-time mode is ~14x faster but less efficient,
                        // so only "Faster" uses it. Row threading and tiles use more cores.
                        if speed == 0 {
                            a.extend(args!["-usage", "realtime", "-cpu-used", "8"]);
                        } else {
                            a.extend(args!["-cpu-used", if speed == 2 { "4" } else { "6" }]);
                        }
                        a.extend(args!["-row-mt", "1", "-tiles", "2x2"]);
                        [44, 39, 34, 29, 24][q]
                    }
                };
                match kbps {
                    Some(k) => a.extend(args!["-b:v", rate(k)]),
                    None if name == "libaom-av1" => a.extend(args!["-crf", crf.to_string(), "-b:v", "0"]),
                    None => a.extend(args!["-crf", crf.to_string()]),
                }
            }
            Encoder::Gpu(g) => {
                a.extend(args!["-c:v", g.encoder.as_str(), "-pix_fmt", "nv12"]);
                if codec == "hevc" {
                    a.extend(args!["-tag:v", "hvc1"]);
                }
                let level = match codec {
                    "hevc" => [33, 30, 27, 24, 21][q],
                    "av1" => [34, 31, 28, 25, 22][q],
                    _ => [32, 28, 25, 22, 19][q],
                };
                // AMF's constant-QP mode has no rate-distortion tuning, so it needs a
                // higher QP for a size comparable to the CPU encoders (and AV1 is on
                // a 0-255 scale). P-frames get a slightly higher QP than keyframes.
                let (amf_i, amf_p) = match codec {
                    "av1" => (level * 5, level * 5 + 8),
                    "hevc" => (level + 4, level + 6),
                    _ => (level + 6, level + 8),
                };
                let level = level.to_string();
                match (g.family.as_str(), kbps) {
                    ("nvenc", Some(k)) => a.extend(args![
                        "-preset", ["p2", "p5", "p7"][speed], "-rc", "vbr",
                        "-b:v", rate(k), "-maxrate", rate(k), "-bufsize", rate(k * 2.0)
                    ]),
                    ("nvenc", None) => a.extend(args![
                        "-preset", ["p2", "p5", "p7"][speed], "-rc", "vbr", "-cq", level, "-b:v", "0"
                    ]),
                    ("qsv", Some(k)) => a.extend(args![
                        "-preset", ["veryfast", "medium", "veryslow"][speed],
                        "-b:v", rate(k), "-maxrate", rate(k)
                    ]),
                    ("qsv", None) => a.extend(args![
                        "-preset", ["veryfast", "medium", "veryslow"][speed], "-global_quality", level
                    ]),
                    (_, Some(k)) => a.extend(args![
                        "-quality", ["speed", "balanced", "quality"][speed], "-rc", "vbr_peak",
                        "-b:v", rate(k), "-maxrate", rate(k), "-bufsize", rate(k * 2.0)
                    ]),
                    (_, None) => a.extend(args![
                        "-quality", ["speed", "balanced", "quality"][speed], "-rc", "cqp",
                        "-qp_i", amf_i.to_string(), "-qp_p", amf_p.to_string()
                    ]),
                }
            }
        }
        a
    }

    /// Two-pass options (`pass` = 1 or 2), or the single-pass extras when `None`.
    /// Stats files are relative because FFmpeg runs inside the scratch folder
    /// (x265 can't take a Windows path: it splits its options on ':').
    fn pass_args(&self, pass: Option<u8>) -> Vec<OsString> {
        match (self, pass) {
            (Encoder::Software(n), Some(p)) if n == "libx265" => {
                args!["-x265-params", format!("log-level=error:pass={p}:stats=ccc-x265.log")]
            }
            (Encoder::Software(n), None) if n == "libx265" => args!["-x265-params", "log-level=error"],
            (Encoder::Software(_), Some(p)) => args!["-pass", p.to_string(), "-passlogfile", "ccc-pass"],
            _ => Vec::new(),
        }
    }
}

fn video(ctx: &Ctx, input: &Path, input_size: u64, s: &VideoSettings, out_path: PathBuf) -> Result<JobResult, String> {
    let info = media::probe(input)?;
    let Some(video_stream) = info.video_stream else {
        return Err("There's no video in this file.".into());
    };
    let caps = media::capabilities();
    let mut notes = Vec::new();

    let codec = if caps.software.contains_key(&s.codec) { s.codec.as_str() } else { "h264" };
    let software = caps
        .software
        .get(codec)
        .cloned()
        .ok_or("No video encoder is available in this copy of FFmpeg.")?;
    let gpu = caps.gpu.get(codec).filter(|_| s.gpu).cloned();

    let duration = info.duration.filter(|d| *d > 0.05);
    let mut audio_kbps = info.audio_stream.and(s.audio.parse::<u32>().ok());
    let size_mode = s.mode == "size";
    let target_bytes = s.target_mb * MB;

    // In size mode, split the byte budget between audio and video.
    let mut video_kbps = None;
    if size_mode {
        if s.target_mb <= 0.0 {
            return Err("Set a target size above 0 MB.".into());
        }
        let Some(secs) = duration else {
            return Err("Couldn't read how long this video is, so a target size can't be worked out. Try Quality mode instead.".into());
        };
        if input_size as f64 <= target_bytes {
            return Ok(JobResult::skipped(
                input_size,
                format!("Already under {}, so there was nothing to do.", format_mb(s.target_mb)),
            ));
        }
        let total = target_bytes * 8.0 * HEADROOM / secs / 1000.0;
        if let Some(a) = audio_kbps {
            // Audio shouldn't eat more than a fifth of a tight budget.
            let cap = total * 0.2;
            if a as f64 > cap {
                audio_kbps = Some(((cap / 16.0).floor() * 16.0).max(32.0) as u32);
            }
        }
        const MIN_VIDEO_KBPS: f64 = 80.0;
        let v = total - audio_kbps.unwrap_or(0) as f64;
        if v < MIN_VIDEO_KBPS {
            let need = (MIN_VIDEO_KBPS + audio_kbps.unwrap_or(0) as f64) * 1000.0 * secs / 8.0 / HEADROOM / MB;
            return Err(format!(
                "{} is too small for a video {} long. Try at least {}.",
                format_mb(s.target_mb),
                format_clock(secs),
                format_mb(need.ceil())
            ));
        }
        video_kbps = Some(v);
    }

    // Picture size and frame rate. Never upscale.
    let out_fps = s
        .fps
        .parse::<f64>()
        .ok()
        .filter(|f| info.fps.is_none_or(|src| src > f + 0.5));
    let short_side = match s.resolution.as_str() {
        "original" => None,
        "auto" => video_kbps.map(|k| auto_short_side(k, codec, out_fps.or(info.fps).unwrap_or(30.0))),
        h => h.parse().ok(),
    };
    let mut filters = Vec::new();
    if let (Some(w), Some(h)) = (info.width, info.height) {
        let (nw, nh) = fit_short_side(w, h, short_side);
        if (nw, nh) != (w, h) {
            filters.push(format!("scale={nw}:{nh}:flags=lanczos"));
        }
    }
    if let Some(f) = out_fps {
        filters.push(format!("fps={f}"));
    }

    let out = Output::new(ctx.control, out_path);
    let scratch = ScratchDir::new(ctx.id)?;
    let speed = match s.speed.as_str() {
        "fast" => 0,
        "small" => 2,
        _ => 1,
    };
    let plan = VideoPlan {
        input,
        info: &info,
        video_stream,
        out: &out.path,
        filters: filters.join(","),
        audio_kbps,
        codec,
        speed,
        quality: s.quality.clamp(1, 5) as usize - 1,
        duration,
        scratch: &scratch.0,
    };

    let mut encoder = gpu.map(Encoder::Gpu).unwrap_or(Encoder::Software(software.clone()));
    let mut kbps = video_kbps;
    let mut refits = 0;
    loop {
        match plan.encode(ctx, &encoder, kbps, refits > 0) {
            Ok(()) => {}
            Err(e) if e != CANCELLED && matches!(encoder, Encoder::Gpu(_)) => {
                if let Encoder::Gpu(g) = &encoder {
                    notes.push(format!("The {} encoder couldn't handle this video, so the CPU was used instead.", g.label));
                }
                encoder = Encoder::Software(software.clone());
                continue;
            }
            Err(e) => return Err(e),
        }
        if !size_mode {
            break;
        }
        // Single-pass and hardware encoders can overshoot; shrink and try again.
        let actual = out.size() as f64;
        if actual <= target_bytes {
            break;
        }
        if refits == 2 {
            notes.push(format!("It came out at {}, a little over the target.", format_mb(actual / MB)));
            break;
        }
        refits += 1;
        kbps = kbps.map(|k| k * target_bytes / actual * 0.95);
    }

    finish(
        out,
        input_size,
        notes,
        false,
        "This video is already compressed about as well as these settings allow, so the original was kept.",
    )
}

struct VideoPlan<'a> {
    input: &'a Path,
    info: &'a MediaInfo,
    video_stream: usize,
    out: &'a Path,
    filters: String,
    audio_kbps: Option<u32>,
    codec: &'a str,
    speed: usize,
    quality: usize,
    duration: Option<f64>,
    scratch: &'a Path,
}

impl VideoPlan<'_> {
    fn encode(&self, ctx: &Ctx, encoder: &Encoder, kbps: Option<f64>, refit: bool) -> Result<(), String> {
        let mut base: Vec<OsString> = Vec::new();
        if matches!(encoder, Encoder::Gpu(_)) {
            base.extend(args!["-hwaccel", "auto"]);
        }
        base.extend(args!["-i", self.input, "-map", format!("0:{}", self.video_stream)]);
        if !self.filters.is_empty() {
            base.extend(args!["-vf", self.filters.as_str()]);
        }
        base.extend(encoder.args(self.codec, self.speed, self.quality, kbps));

        let mut output: Vec<OsString> = match (self.audio_kbps, self.info.audio_stream) {
            (Some(a), Some(idx)) => args!["-map", format!("0:{idx}"), "-c:a", "aac", "-b:a", format!("{a}k")],
            _ => args!["-an"],
        };
        output.extend(args!["-map_metadata", "0", "-movflags", "+faststart", self.out]);

        let phase = if refit { "Refitting" } else { "Compressing" };
        if kbps.is_some() && encoder.two_pass() {
            let mut first = base.clone();
            first.extend(encoder.pass_args(Some(1)));
            first.extend(args!["-an", "-f", "null", NULL_DEVICE]);
            let first_phase = if refit { "Refitting" } else { "Analyzing" };
            run_ffmpeg(ctx, &first, Some(self.scratch), self.duration, first_phase, (0.0, 40.0))?;

            let mut second = base;
            second.extend(encoder.pass_args(Some(2)));
            second.extend(output);
            run_ffmpeg(ctx, &second, Some(self.scratch), self.duration, phase, (40.0, 100.0))
        } else {
            let mut single = base;
            single.extend(encoder.pass_args(None));
            single.extend(output);
            run_ffmpeg(ctx, &single, Some(self.scratch), self.duration, phase, (0.0, 100.0))
        }
    }
}

/// The largest standard picture size that still looks good at this bitrate.
fn auto_short_side(kbps: f64, codec: &str, fps: f64) -> u32 {
    let efficiency = match codec {
        "hevc" => 0.65,
        "av1" => 0.5,
        _ => 1.0,
    };
    let motion = if fps > 40.0 { 1.4 } else { 1.0 };
    [(2160, 14000.0), (1440, 7000.0), (1080, 3500.0), (720, 1600.0), (540, 900.0), (480, 650.0), (360, 350.0)]
        .into_iter()
        .find(|(_, need)| kbps >= need * efficiency * motion)
        .map_or(240, |(side, _)| side)
}

/// Scales so the shorter side is at most `short` (portrait clips keep their
/// orientation), rounding to even numbers as 4:2:0 video needs.
fn fit_short_side(w: u32, h: u32, short: Option<u32>) -> (u32, u32) {
    let current = w.min(h);
    let scale = match short {
        Some(t) if t < current => t as f64 / current as f64,
        _ => 1.0,
    };
    let even = |v: f64| (((v + 0.01) as u32) / 2 * 2).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

// ------------------------------------------------------------------ audio

fn audio(
    ctx: &Ctx,
    input: &Path,
    input_size: u64,
    s: &AudioSettings,
    named: &dyn Fn(&str) -> PathBuf,
) -> Result<JobResult, String> {
    let info = media::probe(input)?;
    let Some(stream) = info.audio_stream else {
        return Err("There's no audio in this file.".into());
    };
    let caps = media::capabilities();
    let format = match s.format.as_str() {
        "opus" if caps.opus => "opus",
        "mp3" if caps.mp3 => "mp3",
        _ => "m4a",
    };
    let out = Output::new(ctx.control, named(format));

    let mut a = args!["-i", input, "-map", format!("0:{stream}"), "-vn"];
    a.extend(match format {
        "opus" => args!["-c:a", "libopus"],
        "mp3" => args!["-c:a", "libmp3lame", "-id3v2_version", "3"],
        _ => args!["-c:a", "aac", "-movflags", "+faststart"],
    });
    a.extend(args!["-b:a", format!("{}k", s.bitrate.clamp(16, 320))]);
    if s.mono {
        a.extend(args!["-ac", "1"]);
    }
    a.extend(args!["-map_metadata", "0", out.path.as_path()]);
    run_ffmpeg(ctx, &a, None, info.duration, "Compressing", (0.0, 100.0))?;

    finish(
        out,
        input_size,
        Vec::new(),
        false,
        "This audio is already smaller than these settings would make it, so the original was kept.",
    )
}

// ------------------------------------------------------------------ images

fn image(
    ctx: &Ctx,
    input: &Path,
    input_size: u64,
    s: &ImageSettings,
    named: &dyn Fn(&str) -> PathBuf,
) -> Result<JobResult, String> {
    let info = media::probe(input)?;
    let Some(stream) = info.video_stream else {
        return Err("This image couldn't be read.".into());
    };
    let caps = media::capabilities();
    let format = match (s.format.as_str(), media::extension(input).as_str()) {
        ("keep", "png") | ("png", _) => "png",
        ("keep", "webp") | ("webp", _) if caps.webp => "webp",
        _ => "jpg",
    };

    let mut scale = String::new();
    let mut size = None;
    if let (Some(w), Some(h)) = (info.width, info.height) {
        let (nw, nh) = fit_long_side(w, h, s.max_size);
        if (nw, nh) != (w, h) {
            scale = format!("scale={nw}:{nh}:flags=lanczos,");
        }
        size = Some((nw, nh));
    }
    let out = Output::new(ctx.control, named(format));
    let quality = s.quality.clamp(1, 100) as f64;
    let src = format!("[0:{stream}]");

    let mut a = args!["-i", input];
    match format {
        "jpg" => {
            // JPEG has no transparency: flatten anything that might have it onto white.
            if let (true, Some((w, h))) = (may_have_alpha(info.pix_fmt.as_deref()), size) {
                a.extend(args![
                    "-filter_complex",
                    format!("{src}{scale}format=rgba[fg];color=c=white:s={w}x{h}[bg];[bg][fg]overlay=shortest=1:format=auto,format=yuvj420p[out]"),
                    "-map",
                    "[out]"
                ]);
            } else {
                a.extend(args!["-filter_complex", format!("{src}{scale}format=yuvj420p[out]"), "-map", "[out]"]);
            }
            // FFmpeg's JPEG scale runs 2 (best) to 31 (worst).
            let qv = ((100.0 - quality) / 4.0).round() + 1.0;
            a.extend(args!["-c:v", "mjpeg", "-q:v", qv.clamp(2.0, 31.0).to_string()]);
        }
        "webp" => {
            a.extend(args!["-filter_complex", format!("{src}{scale}null[out]"), "-map", "[out]"]);
            a.extend(args!["-c:v", "libwebp", "-quality", quality.to_string(), "-compression_level", "6"]);
        }
        _ => {
            if s.reduce_colors {
                a.extend(args![
                    "-filter_complex",
                    format!("{src}{scale}split[a][b];[a]palettegen=max_colors=256:reserve_transparent=1:stats_mode=single[p];[b][p]paletteuse=dither=sierra2_4a:alpha_threshold=128[out]"),
                    "-map",
                    "[out]"
                ]);
            } else {
                a.extend(args!["-filter_complex", format!("{src}{scale}null[out]"), "-map", "[out]"]);
            }
            a.extend(args!["-c:v", "png", "-compression_level", "9", "-pred", "mixed"]);
        }
    }
    a.extend(args!["-frames:v", "1", "-update", "1", "-map_metadata", "-1", out.path.as_path()]);
    run_ffmpeg(ctx, &a, None, None, "Compressing", (0.0, 100.0))?;

    finish(
        out,
        input_size,
        Vec::new(),
        false,
        "This image is already smaller than these settings would make it, so the original was kept.",
    )
}

fn may_have_alpha(pix_fmt: Option<&str>) -> bool {
    let Some(f) = pix_fmt else { return true };
    f.starts_with("rgba") || f.starts_with("bgra") || f.starts_with("argb") || f.starts_with("abgr")
        || f.starts_with("ya") || f.starts_with("yuva") || f.starts_with("gbrap") || f == "pal8"
}

/// Scales so the longer side is at most `max` (0 = no limit). Never upscales.
fn fit_long_side(w: u32, h: u32, max: u32) -> (u32, u32) {
    let longest = w.max(h);
    if max == 0 || longest <= max {
        return (w, h);
    }
    let scale = max as f64 / longest as f64;
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

// ------------------------------------------------------------------ everything else

fn other(ctx: &Ctx, input: &Path, input_size: u64, s: &ArchiveSettings, out_path: PathBuf) -> Result<JobResult, String> {
    let level = match s.level.as_str() {
        "fast" => 1,
        "max" => 9,
        _ => 6,
    };
    let out = Output::new(ctx.control, out_path);
    archive::zip_to(ctx, input, input_size, level, &out.path)?;
    // A folder is worth zipping even if it doesn't shrink; a single file isn't.
    let is_dir = input.is_dir();
    let mut notes = Vec::new();
    if is_dir && out.size() >= input_size {
        notes.push("Its contents were already compressed, so the .zip is about the same size.".to_string());
    }
    finish(
        out,
        input_size,
        notes,
        is_dir,
        "This file is already compressed (zipping wouldn't make it smaller), so it was left as is.",
    )
}

// ------------------------------------------------------------------ formatting

fn format_mb(mb: f64) -> String {
    if mb >= 10.0 || mb.fract() == 0.0 {
        format!("{} MB", mb.round())
    } else {
        format!("{mb:.1} MB")
    }
}

fn format_clock(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_by_the_short_side_and_keeps_even_sizes() {
        assert_eq!(fit_short_side(1920, 1080, Some(720)), (1280, 720));
        assert_eq!(fit_short_side(1080, 1920, Some(720)), (720, 1280));
        assert_eq!(fit_short_side(1280, 720, Some(1080)), (1280, 720));
        assert_eq!(fit_short_side(1281, 721, None), (1280, 720));
    }

    #[test]
    fn scales_images_by_the_long_side() {
        assert_eq!(fit_long_side(4032, 3024, 1920), (1920, 1440));
        assert_eq!(fit_long_side(800, 600, 1920), (800, 600));
        assert_eq!(fit_long_side(800, 600, 0), (800, 600));
    }

    #[test]
    fn picks_a_smaller_picture_for_tight_budgets() {
        assert_eq!(auto_short_side(5000.0, "h264", 30.0), 1080);
        assert_eq!(auto_short_side(1000.0, "h264", 30.0), 540);
        assert_eq!(auto_short_side(1000.0, "h264", 60.0), 480);
        assert_eq!(auto_short_side(1000.0, "av1", 30.0), 720);
    }
}
