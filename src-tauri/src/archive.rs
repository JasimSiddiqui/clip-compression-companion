//! Packing any other file, or a whole folder, into a standard .zip that opens
//! anywhere (Deflate, the format Windows, macOS, and Linux all read natively).

use crate::compress::Ctx;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Total size in bytes of a file, or of everything inside a folder.
pub fn size_of(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| size_of(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

struct Entry {
    path: PathBuf,
    /// Path inside the archive, '/'-separated, starting with the folder's own name.
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

fn collect(root: &Path) -> Vec<Entry> {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "files".into());
    let mut entries = Vec::new();
    push(root, name, &mut entries);
    entries
}

fn push(path: &Path, name: String, entries: &mut Vec<Entry>) {
    // Symlinks are skipped rather than followed, so a link can't pull in half the disk.
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    entries.push(Entry {
        path: path.to_path_buf(),
        name: name.clone(),
        is_dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified: meta.modified().ok(),
    });
    if meta.is_dir() {
        let Ok(children) = fs::read_dir(path) else { return };
        let mut children: Vec<_> = children.flatten().collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
            push(&child.path(), child_name, entries);
        }
    }
}

/// Zips `input` into `out`, reporting progress by bytes read.
pub fn zip_to(ctx: &Ctx, input: &Path, total: u64, level: i64, out: &Path) -> Result<(), String> {
    let entries = collect(input);
    let file = File::create(out).map_err(|e| format!("Couldn't create {}: {e}", out.display()))?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, file));
    let offset = time::UtcOffset::current_local_offset().ok();
    let failed = |e: &dyn std::fmt::Display| format!("Couldn't write the .zip: {e}");

    let started = Instant::now();
    let mut last_report = started;
    let mut done = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    ctx.progress(Some(0.0), "Zipping", None);

    for entry in &entries {
        ctx.check()?;
        let mut options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(level))
            .large_file(entry.size >= u32::MAX as u64);
        if let Some(t) = entry.modified.and_then(|t| dos_time(t, offset)) {
            options = options.last_modified_time(t);
        }
        if entry.is_dir {
            zip.add_directory(entry.name.as_str(), options).map_err(|e| failed(&e))?;
            continue;
        }

        zip.start_file(entry.name.as_str(), options).map_err(|e| failed(&e))?;
        let mut source = File::open(&entry.path)
            .map_err(|e| format!("Couldn't read {}: {e}", entry.path.display()))?;
        loop {
            let n = source
                .read(&mut buf)
                .map_err(|e| format!("Couldn't read {}: {e}", entry.path.display()))?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n]).map_err(|e| failed(&e))?;
            done += n as u64;

            if last_report.elapsed().as_millis() >= 200 {
                last_report = Instant::now();
                ctx.check()?;
                let rate = done as f64 / started.elapsed().as_secs_f64().max(0.001);
                let eta = (rate > 0.0).then(|| total.saturating_sub(done) as f64 / rate);
                ctx.progress(Some(done as f64 / total.max(1) as f64 * 100.0), "Zipping", eta);
            }
        }
    }

    zip.finish()
        .map_err(|e| failed(&e))?
        .flush()
        .map_err(|e| failed(&e))?;
    Ok(())
}

/// Zip stores local times with no time zone, the way file managers expect.
fn dos_time(t: SystemTime, offset: Option<time::UtcOffset>) -> Option<zip::DateTime> {
    let utc = time::OffsetDateTime::from(t);
    let local = offset.map_or(utc, |o| utc.to_offset(o));
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        local.month() as u8,
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
    )
    .ok()
}
