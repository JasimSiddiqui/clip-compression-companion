//! Finding FFmpeg, reading what's inside a file, and working out which encoders
//! this machine can actually use.
//!
//! Everything goes through the single `ffmpeg` binary that ships next to the app
//! (no ffprobe, which would double the download): file details come from parsing
//! the stream summary FFmpeg prints for `ffmpeg -i <file>`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// The bundled FFmpeg (placed next to the app's executable by the installer and
/// by `tauri dev`), falling back to whatever `ffmpeg` is on the PATH.
pub fn ffmpeg_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
            .filter(|bundled| bundled.is_file())
            .unwrap_or_else(|| PathBuf::from(name))
    })
}

/// An FFmpeg command that never flashes a console window. Encodes run at
/// below-normal priority so the rest of the computer stays responsive.
pub fn ffmpeg(encode: bool) -> Command {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        let mut flags = CREATE_NO_WINDOW;
        if encode {
            flags |= BELOW_NORMAL_PRIORITY_CLASS;
        }
        cmd.creation_flags(flags);
    }
    #[cfg(not(windows))]
    let _ = encode;
    cmd
}

/// Ties an FFmpeg process to the app's lifetime. On Windows it joins a job
/// object that the OS closes when the app exits for any reason (a crash or
/// "End task" included), which ends FFmpeg too instead of leaving it running.
#[cfg(windows)]
pub fn tie_to_app(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // Stored as an address because raw handles aren't Sync. Never closed:
    // Windows closes it, and kills the job, when the app's process ends.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    if job != 0 {
        unsafe {
            AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE);
        }
    }
}

#[cfg(not(windows))]
pub fn tie_to_app(_child: &std::process::Child) {}

// ------------------------------------------------------------------ kinds

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    Audio,
    Image,
    Other,
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn kind_of(path: &Path) -> Kind {
    if path.is_dir() {
        return Kind::Other;
    }
    match extension(path).as_str() {
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" | "ts"
        | "mts" | "m2ts" | "3gp" | "ogv" | "gif" => Kind::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wma" | "aiff"
        | "aif" => Kind::Audio,
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" => Kind::Image,
        _ => Kind::Other,
    }
}

// ------------------------------------------------------------------ probing

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    /// Seconds; `None` for still images and files that don't report a length.
    pub duration: Option<f64>,
    /// Display size, with phone-style rotation already applied.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub pix_fmt: Option<String>,
    pub audio_codec: Option<String>,
    /// Input stream numbers, for `-map 0:N`. Cover art is never picked as the video.
    #[serde(skip)]
    pub video_stream: Option<usize>,
    #[serde(skip)]
    pub audio_stream: Option<usize>,
}

impl MediaInfo {
    pub fn has_video(&self) -> bool {
        self.video_stream.is_some()
    }
    pub fn has_audio(&self) -> bool {
        self.audio_stream.is_some()
    }
}

pub fn probe(path: &Path) -> Result<MediaInfo, String> {
    let out = ffmpeg(false)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Couldn't start FFmpeg: {e}"))?;
    let info = parse_probe(&String::from_utf8_lossy(&out.stderr));
    if !info.has_video() && !info.has_audio() {
        return Err("This file couldn't be read as video, audio, or an image.".into());
    }
    Ok(info)
}

/// Parses the "Input #0" summary FFmpeg prints, e.g.
/// `Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709), 1920x1080 [SAR 1:1 DAR 16:9], 4999 kb/s, 30 fps, ...`
fn parse_probe(text: &str) -> MediaInfo {
    let mut info = MediaInfo::default();
    let mut rotation = 0.0f64;
    let mut in_video = false;

    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Duration: ") {
            info.duration = parse_clock(rest.split(',').next().unwrap_or(""));
        } else if let Some(rest) = line.strip_prefix("Stream #0:") {
            in_video = false;
            let index = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<usize>()
                .ok();
            if let Some(at) = line.find("Video: ") {
                if line.contains("(attached pic)") || info.has_video() {
                    continue;
                }
                in_video = true;
                info.video_stream = index;
                let parts = split_top_level(&line[at + 7..]);
                info.video_codec = parts.first().and_then(|p| p.split_whitespace().next()).map(str::to_string);
                info.pix_fmt = parts
                    .get(1)
                    .map(|p| p.split('(').next().unwrap_or("").trim().to_string());
                for part in &parts {
                    let first = part.split_whitespace().next().unwrap_or("");
                    if info.width.is_none() {
                        if let Some((w, h)) = first.split_once('x') {
                            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                                info.width = Some(w);
                                info.height = Some(h);
                            }
                        }
                    }
                    if let Some(fps) = part.strip_suffix(" fps") {
                        info.fps = parse_rate(fps);
                    } else if info.fps.is_none() {
                        if let Some(tbr) = part.strip_suffix(" tbr") {
                            info.fps = parse_rate(tbr);
                        }
                    }
                }
            } else if let Some(at) = line.find("Audio: ") {
                if info.has_audio() {
                    continue;
                }
                info.audio_stream = index;
                info.audio_codec = line[at + 7..]
                    .split([' ', ','])
                    .next()
                    .map(str::to_string);
            }
        } else if in_video && line.contains("rotation of") {
            // "displaymatrix: rotation of -90.00 degrees"
            rotation = line
                .split("rotation of")
                .nth(1)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
        }
    }

    let quarter_turn = (rotation.abs() % 180.0 - 90.0).abs() < 1.0;
    if quarter_turn {
        std::mem::swap(&mut info.width, &mut info.height);
    }
    info
}

/// "00:01:02.34" -> 62.34
fn parse_clock(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// "29.97" / "30" / "30k" -> fps; FFmpeg prints "k" for thousands.
fn parse_rate(s: &str) -> Option<f64> {
    let s = s.trim();
    let value = match s.strip_suffix('k') {
        Some(k) => k.parse::<f64>().ok()? * 1000.0,
        None => s.parse().ok()?,
    };
    (value > 0.0 && value < 1000.0).then_some(value)
}

/// Splits on commas that aren't inside parentheses or brackets.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim());
    parts
}

// ------------------------------------------------------------------ encoders

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GpuEncoder {
    /// FFmpeg encoder name, e.g. "hevc_nvenc".
    pub encoder: String,
    /// "nvenc" | "qsv" | "amf"
    pub family: String,
    /// Shown in the UI, e.g. "NVIDIA NVENC".
    pub label: String,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub ffmpeg: bool,
    pub version: String,
    /// Software video encoder per codec ("h264" -> "libx264", "av1" -> "libsvtav1" or "libaom-av1").
    pub software: HashMap<String, String>,
    /// Working hardware encoder per codec, if any.
    pub gpu: HashMap<String, GpuEncoder>,
    pub webp: bool,
    pub mp3: bool,
    pub opus: bool,
}

/// Detected once per run. Hardware encoders are only reported if a tiny test
/// encode actually succeeds, since FFmpeg lists them whether or not the GPU,
/// driver, or runtime is present.
pub fn capabilities() -> &'static Capabilities {
    static CAPS: OnceLock<Capabilities> = OnceLock::new();
    CAPS.get_or_init(detect)
}

fn detect() -> Capabilities {
    let Ok(out) = ffmpeg(false)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Capabilities::default();
    };
    let listing = String::from_utf8_lossy(&out.stdout);
    let encoders: HashSet<&str> = listing
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();

    let version = ffmpeg(false)
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(2)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut software = HashMap::new();
    for (codec, candidates) in [
        ("h264", &["libx264"][..]),
        ("hevc", &["libx265"][..]),
        ("av1", &["libsvtav1", "libaom-av1"][..]),
    ] {
        if let Some(enc) = candidates.iter().find(|e| encoders.contains(**e)) {
            software.insert(codec.to_string(), enc.to_string());
        }
    }

    // Test every listed hardware encoder in parallel; keep the first working one
    // per codec in order of preference.
    let families = [("nvenc", "NVIDIA NVENC"), ("qsv", "Intel Quick Sync"), ("amf", "AMD AMF")];
    let mut tests = Vec::new();
    for codec in ["h264", "hevc", "av1"] {
        for (rank, (family, _)) in families.iter().enumerate() {
            let name = format!("{codec}_{family}");
            if encoders.contains(name.as_str()) {
                tests.push((codec, rank, thread::spawn(move || test_encoder(&name))));
            }
        }
    }
    let mut gpu: HashMap<String, (usize, GpuEncoder)> = HashMap::new();
    for (codec, rank, handle) in tests {
        if !handle.join().unwrap_or(false) {
            continue;
        }
        if gpu.get(codec).is_some_and(|(best, _)| *best < rank) {
            continue;
        }
        let (family, label) = families[rank];
        gpu.insert(
            codec.to_string(),
            (
                rank,
                GpuEncoder {
                    encoder: format!("{codec}_{family}"),
                    family: family.to_string(),
                    label: label.to_string(),
                },
            ),
        );
    }

    Capabilities {
        ffmpeg: true,
        version,
        software,
        gpu: gpu.into_iter().map(|(k, (_, v))| (k, v)).collect(),
        webp: encoders.contains("libwebp"),
        mp3: encoders.contains("libmp3lame"),
        opus: encoders.contains("libopus"),
    }
}

fn test_encoder(encoder: &str) -> bool {
    ffmpeg(false)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi"])
        .args(["-i", "color=black:s=320x240:r=30:d=0.2"])
        .args(["-c:v", encoder, "-pix_fmt", "nv12", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rotated_phone_video() {
        let text = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'clip.mp4':
  Duration: 00:00:42.50, start: 0.000000, bitrate: 9000 kb/s
  Stream #0:0[0x1](und): Video: hevc (Main) (hvc1 / 0x31637668), yuv420p(tv, bt709), 1920x1080, 8800 kb/s, 29.97 fps, 29.97 tbr, 600 tbn (default)
      Side data:
        displaymatrix: rotation of -90.00 degrees
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo, fltp, 192 kb/s (default)";
        let info = parse_probe(text);
        assert_eq!(info.duration, Some(42.5));
        assert_eq!((info.width, info.height), (Some(1080), Some(1920)));
        assert_eq!(info.fps, Some(29.97));
        assert_eq!(info.video_codec.as_deref(), Some("hevc"));
        assert_eq!(info.pix_fmt.as_deref(), Some("yuv420p"));
        assert_eq!((info.video_stream, info.audio_stream), (Some(0), Some(1)));
    }

    #[test]
    fn ignores_cover_art_in_audio_files() {
        let text = "  Duration: 00:03:12.00, start: 0.025057, bitrate: 320 kb/s
  Stream #0:0: Audio: mp3 (mp3float), 44100 Hz, stereo, fltp, 320 kb/s
  Stream #0:1: Video: mjpeg (Baseline), yuvj420p(pc, bt470bg/unknown/unknown), 500x500, 90k tbr, 90k tbn (attached pic)";
        let info = parse_probe(text);
        assert!(!info.has_video());
        assert_eq!(info.audio_stream, Some(0));
        assert_eq!(info.duration, Some(192.0));
    }

    #[test]
    fn parses_a_png() {
        let text = "  Duration: N/A, bitrate: N/A
  Stream #0:0: Video: png, rgba(pc, gbr/unknown/unknown), 800x600, 25 fps, 25 tbr, 25 tbn";
        let info = parse_probe(text);
        assert_eq!(info.duration, None);
        assert_eq!(info.pix_fmt.as_deref(), Some("rgba"));
        assert_eq!((info.width, info.height), (Some(800), Some(600)));
    }
}
