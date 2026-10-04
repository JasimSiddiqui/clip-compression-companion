//! Tauri command surface for Clip Compression Companion.

mod archive;
mod compress;
mod media;

use compress::{Control, Job, JobResult, Jobs};
use media::{Capabilities, Kind, MediaInfo};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tauri_plugin_opener::OpenerExt;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileInfo {
    path: String,
    name: String,
    kind: Kind,
    is_dir: bool,
    /// Bytes; for a folder, everything inside it.
    size: u64,
    media: Option<MediaInfo>,
}

/// Runs blocking work (FFmpeg, disk I/O) off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn probe(path: String) -> Result<FileInfo, String> {
    blocking(move || {
        let p = PathBuf::from(&path);
        let meta = std::fs::metadata(&p).map_err(|_| "This file couldn't be opened.".to_string())?;
        let kind = media::kind_of(&p);
        let media = match kind {
            Kind::Other => None,
            _ => Some(media::probe(&p)?),
        };
        Ok(FileInfo {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
            path,
            kind,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { archive::size_of(&p) } else { meta.len() },
            media,
        })
    })
    .await
}

#[tauri::command]
async fn capabilities() -> Result<Capabilities, String> {
    blocking(|| Ok(media::capabilities().clone())).await
}

#[tauri::command]
async fn compress(app: AppHandle, jobs: State<'_, Jobs>, job: Job) -> Result<JobResult, String> {
    let id = job.id;
    let control = Arc::new(Control::default());
    jobs.0.lock().unwrap().insert(id, control.clone());
    let result = blocking(move || compress::run(&app, &control, &job)).await;
    jobs.0.lock().unwrap().remove(&id);
    result
}

#[tauri::command]
fn cancel(jobs: State<'_, Jobs>, id: u32) {
    if let Some(control) = jobs.0.lock().unwrap().get(&id) {
        control.cancel();
    }
}

#[tauri::command]
fn reveal(app: AppHandle, path: String) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| format!("Couldn't open the folder: {e}"))
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("Couldn't open link: {e}"))
}

/// Files passed on the command line, e.g. dropped onto the app's icon.
#[tauri::command]
fn launch_files() -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    existing_paths(std::env::args().skip(1), &cwd)
}

fn existing_paths(args: impl IntoIterator<Item = String>, cwd: &Path) -> Vec<String> {
    args.into_iter()
        .filter(|a| !a.starts_with('-'))
        .filter_map(|a| std::path::absolute(cwd.join(a)).ok())
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // A second launch (say, files dropped on the icon while the app is open)
        // hands its files to the window that's already running.
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            let files = existing_paths(argv.into_iter().skip(1), Path::new(&cwd));
            if !files.is_empty() {
                let _ = app.emit("open-files", files);
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Jobs::default())
        .invoke_handler(tauri::generate_handler![
            probe,
            capabilities,
            compress,
            cancel,
            reveal,
            open_url,
            launch_files
        ])
        .build(tauri::generate_context!())
        .expect("error while building Clip Compression Companion")
        .run(|app, event| {
            // Quitting mid-job: stop FFmpeg and don't leave a half-written file behind.
            if let RunEvent::Exit = event {
                for control in app.state::<Jobs>().0.lock().unwrap().values() {
                    control.abandon();
                }
            }
        });
}
