use chrono::Local;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};
use uuid::Uuid;

mod export_job;
use export_job::{ExportControl, ExportJobs};

struct AppState {
    recording: Mutex<Option<ActiveRecording>>,
    exports: ExportJobs,
    /// The open region selection on macOS: its generation (part of every
    /// selector window label) and where to deliver the result.
    #[cfg(target_os = "macos")]
    region_selection: Mutex<Option<(u64, mpsc::Sender<Option<RecordingRegion>>)>>,
}

// Windows draws the overlay from a PowerShell process; macOS uses a
// transparent webview window owned by this process.
#[cfg(not(target_os = "macos"))]
type RecordingOverlay = Child;
#[cfg(target_os = "macos")]
type RecordingOverlay = tauri::WebviewWindow;

struct ActiveRecording {
    child: Child,
    overlay_child: Option<RecordingOverlay>,
    audio: Option<LoopbackRecording>,
    audio_lead_seconds: f64,
    path: PathBuf,
}

impl Drop for AppState {
    fn drop(&mut self) {
        let active = self.recording.get_mut().ok().and_then(Option::take);
        if let Some(mut recording) = active {
            terminate_active_recording(&mut recording);
        }
    }
}

struct LoopbackRecording {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<Result<Option<PathBuf>, String>>,
    started_at: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct AppSettings {
    save_folder: String,
    ffmpeg_path: String,
    frame_rate: u32,
    capture_backend: CaptureBackend,
    max_megabytes: f64,
    #[serde(default = "default_true")]
    size_cap_enabled: bool,
    quality_target_kbps: u32,
    export_encoder_key: String,
    export_bitrate_scale: f64,
    audio_gain_db: f64,
    unsupported_encoder_keys: Vec<String>,
    encoder_benchmarks: Vec<EncoderBenchmark>,
    #[serde(default = "default_true")]
    include_video: bool,
    include_audio: bool,
    #[serde(default = "default_true")]
    best_audio: bool,
    #[serde(default = "default_audio_quality")]
    audio_quality: String,
    audio_device_name: String,
    start_with_windows: bool,
    #[serde(default = "default_true")]
    start_hidden_in_tray: bool,
    record_hotkey: String,
    reset_hotkey: String,
    github_repository_url: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        let save_folder = dirs::video_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
            .join("QuickClipper")
            .to_string_lossy()
            .to_string();

        Self {
            save_folder,
            ffmpeg_path: "ffmpeg".to_string(),
            frame_rate: 30,
            capture_backend: CaptureBackend::default(),
            max_megabytes: 9.8,
            size_cap_enabled: true,
            quality_target_kbps: 10000,
            export_encoder_key: "x264-medium".to_string(),
            export_bitrate_scale: 1.0,
            audio_gain_db: 0.0,
            unsupported_encoder_keys: Vec::new(),
            encoder_benchmarks: Vec::new(),
            include_video: true,
            include_audio: true,
            best_audio: true,
            audio_quality: "best".to_string(),
            audio_device_name: String::new(),
            start_with_windows: true,
            start_hidden_in_tray: true,
            record_hotkey: default_record_hotkey().to_string(),
            reset_hotkey: default_reset_hotkey().to_string(),
            github_repository_url: "https://github.com/daveranan/clipper".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum CaptureBackend {
    DesktopDuplication,
    Gdi,
}

impl Default for CaptureBackend {
    fn default() -> Self {
        Self::DesktopDuplication
    }
}

// Super+Shift+4 is the macOS screenshot shortcut, and Cmd+Shift+R reloads
// pages in browsers, so macOS gets Control-based defaults.
fn default_record_hotkey() -> &'static str {
    if cfg!(target_os = "macos") {
        "Control+Shift+R"
    } else {
        "Super+Shift+R"
    }
}

fn default_reset_hotkey() -> &'static str {
    if cfg!(target_os = "macos") {
        "Control+Shift+4"
    } else {
        "Super+Shift+4"
    }
}

fn default_audio_quality() -> String {
    "best".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderBenchmark {
    encoder_key: String,
    encoder_label: String,
    seconds: f64,
    bytes: u64,
    tested_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    id: String,
    path: String,
    original_path: String,
    width: u32,
    height: u32,
    duration_seconds: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewCache {
    folder: String,
    fps: u32,
    frames: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveformPeakData {
    duration_seconds: f64,
    sample_rate: u32,
    channels: Vec<WaveformPeakChannel>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveformPeakChannel {
    minimums: Vec<f32>,
    maximums: Vec<f32>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CutRange {
    start: f64,
    end: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    input_path: String,
    output_path: String,
    start: f64,
    end: f64,
    audio_start: f64,
    audio_end: f64,
    crop: Crop,
    output_width: u32,
    output_height: u32,
    auto_fit720: bool,
    cuts: Vec<CutRange>,
    audio_cuts: Vec<CutRange>,
    settings: AppSettings,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    path: String,
    bytes: u64,
    seconds: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkResult {
    encoder_key: String,
    encoder_label: String,
    path: String,
    bytes: u64,
    seconds: f64,
    success: bool,
    error: String,
}

struct ExportAttemptResult {
    path: PathBuf,
    bytes: u64,
    kbps: u32,
}

#[derive(Clone, Copy)]
struct AudioWavProfile {
    codec: &'static str,
    sample_rate: u32,
    channels: u32,
    kbps: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    width: u32,
    height: u32,
}

#[tauri::command]
async fn copy_video_frame(input_path: String, seconds_at: f64, settings: AppSettings) -> Result<ScreenshotResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let frame = extract_screenshot(&input_path, seconds_at, &settings)?;
        let result = ScreenshotResult { width: frame.width(), height: frame.height() };
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|error| format!("Could not open clipboard: {error}"))?;
        clipboard.set_image(arboard::ImageData {
            width: result.width as usize,
            height: result.height as usize,
            bytes: std::borrow::Cow::Owned(frame.into_raw()),
        }).map_err(|error| format!("Could not copy screenshot: {error}"))?;
        Ok(result)
    }).await.map_err(|error| format!("Screenshot worker failed: {error}"))?
}

fn extract_screenshot(input_path: &str, seconds_at: f64, settings: &AppSettings) -> Result<image::RgbaImage, String> {
    if !seconds_at.is_finite() {
        return Err("Invalid screenshot time.".into());
    }
    let source = Path::new(input_path);
    let info = probe_video_file(settings, source, source)?.ok_or("Could not read source video.")?;
    let time = seconds_at.clamp(0.0, info.duration_seconds);
    let output = hidden_command(&settings.ffmpeg_path).args([
        "-hide_banner", "-loglevel", "error", "-nostdin",
        "-ss", &format!("{time:.6}"), "-i", input_path,
        "-map", "0:v:0", "-an", "-sn", "-dn", "-frames:v", "1",
        "-c:v", "png", "-pix_fmt", "rgba", "-f", "image2pipe", "pipe:1",
    ]).output().map_err(|error| format!("Screenshot failed: {error}"))?;
    if !output.status.success() {
        return Err(command_error_message("Screenshot failed", &String::from_utf8_lossy(&output.stderr)));
    }
    let png = if output.stdout.is_empty() {
        // At the end of a clip there may be no frame at or after the playhead.
        // Decode the tail, retaining its last frame without buffering video in memory.
        let folder = tempfile::Builder::new().prefix("quickclipper-frame-").tempdir().map_err(|error| error.to_string())?;
        let path = folder.path().join("last.png");
        run_command(&settings.ffmpeg_path, &[
            "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
            "-sseof", "-1", "-noaccurate_seek", "-i", input_path,
            "-map", "0:v:0", "-an", "-sn", "-dn",
            "-c:v", "png", "-pix_fmt", "rgba", "-fps_mode", "passthrough", "-update", "1",
            &path.to_string_lossy(),
        ], "Screenshot failed")?;
        fs::read(path).map_err(|_| "No video frame was available at this position.".to_string())?
    } else {
        output.stdout
    };
    image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map(|image| image.into_rgba8())
        .map_err(|error| format!("Could not decode screenshot: {error}"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingRequest {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    settings: AppSettings,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[tauri::command]
fn load_settings() -> Result<AppSettings, String> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let json = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

#[tauri::command]
fn save_settings(settings: AppSettings) -> Result<(), String> {
    let path = settings_path()?;
    ensure_parent(&path)?;
    let json = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    fs::write(path, json).map_err(|error| error.to_string())
}

#[tauri::command]
fn open_video_dialog(settings: AppSettings) -> Result<Option<VideoInfo>, String> {
    let Some(source_path) = rfd::FileDialog::new()
        .add_filter(
            "Video",
            &["mp4", "mov", "mkv", "webm", "avi", "wmv", "m4v", "gif"],
        )
        .pick_file()
    else {
        return Ok(None);
    };

    let imported = import_to_workspace(&source_path)?;
    probe_video_file(&settings, &imported, &source_path)
}

#[tauri::command]
fn prepare_preview_cache(
    input_path: String,
    fps: u32,
    max_seconds: f64,
    settings: AppSettings,
) -> Result<PreviewCache, String> {
    let folder = preview_root()?.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let pattern = folder.join("frame_%06d.jpg");
    let fps = fps.clamp(4, 12);
    let max_seconds = max_seconds.clamp(1.0, 180.0);
    let filter = format!("fps={},scale=1280:-2", fps);
    run_command(
        &settings.ffmpeg_path,
        &[
            "-hide_banner",
            "-y",
            "-i",
            &input_path,
            "-t",
            &seconds(max_seconds),
            "-vf",
            &filter,
            "-q:v",
            "5",
            &pattern.to_string_lossy(),
        ],
        "Preview cache failed",
    )?;

    let mut frames: Vec<String> = fs::read_dir(&folder)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jpg"))
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    frames.sort();

    Ok(PreviewCache {
        folder: folder.to_string_lossy().to_string(),
        fps,
        frames,
    })
}

#[tauri::command]
fn prepare_playback_source(input_path: String, settings: AppSettings) -> Result<String, String> {
    if is_browser_safe_playback(&settings, Path::new(&input_path))? {
        return Ok(input_path);
    }

    let proxy = preview_root()?.join(format!("playback-{}.mp4", Uuid::new_v4()));
    let args = [
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        input_path,
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "veryfast".to_string(),
        "-crf".to_string(),
        "12".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        "128k".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        proxy.to_string_lossy().to_string(),
    ];
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_command(&settings.ffmpeg_path, &refs, "Playback proxy failed")?;
    Ok(proxy.to_string_lossy().to_string())
}

#[tauri::command]
fn extract_exact_frame(
    input_path: String,
    seconds_at: f64,
    settings: AppSettings,
) -> Result<String, String> {
    let probe = probe_video_file(&settings, Path::new(&input_path), Path::new(&input_path))?
        .ok_or_else(|| "Could not read media duration.".to_string())?;
    let safe_seconds = seconds_at
        .max(0.0)
        .min((probe.duration_seconds - 0.05).max(0.0));
    let folder = preview_root()?.join("exact");
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let frame_path = folder.join(format!(
        "frame-{}-{}.jpg",
        Uuid::new_v4(),
        millis(safe_seconds)
    ));
    let first = run_command(
        &settings.ffmpeg_path,
        &[
            "-hide_banner",
            "-y",
            "-ss",
            &seconds(safe_seconds),
            "-i",
            &input_path,
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-frames:v",
            "1",
            "-f",
            "image2",
            "-q:v",
            "2",
            &frame_path.to_string_lossy(),
        ],
        "Exact frame failed",
    );
    if let Err(first_error) = first {
        run_command(
            &settings.ffmpeg_path,
            &[
                "-hide_banner",
                "-y",
                "-i",
                &input_path,
                "-ss",
                &seconds(safe_seconds),
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-dn",
                "-frames:v",
                "1",
                "-f",
                "image2",
                "-q:v",
                "2",
                &frame_path.to_string_lossy(),
            ],
            &first_error,
        )?;
    }
    Ok(frame_path.to_string_lossy().to_string())
}

#[tauri::command]
fn generate_waveform(
    input_path: String,
    points: u32,
    settings: AppSettings,
) -> Result<WaveformPeakData, String> {
    let probe = probe_video_file(&settings, Path::new(&input_path), Path::new(&input_path))?
        .ok_or_else(|| "Could not read media duration.".to_string())?;
    let sample_rate = 24_000_u32;
    let points = points.clamp(512, 65_536);
    let sample_count = ((probe.duration_seconds.max(0.1) * sample_rate as f64).ceil() as usize)
        .max(points as usize);
    let samples_per_peak = (sample_count as f64 / points as f64).ceil().max(1.0) as usize;
    let mut command = hidden_command(&settings.ffmpeg_path);
    command.args([
        "-hide_banner",
        "-v",
        "error",
        "-i",
        &input_path,
        "-map",
        "0:a:0?",
        "-ac",
        "1",
        "-ar",
        &sample_rate.to_string(),
        "-f",
        "f32le",
        "-",
    ]);
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Waveform generation failed: {}", error))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(WaveformPeakData {
            duration_seconds: probe.duration_seconds,
            sample_rate,
            channels: Vec::new(),
        });
    }

    let samples: Vec<f32> = output
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).clamp(-1.0, 1.0))
        .collect();
    let mut minimums = Vec::new();
    let mut maximums = Vec::new();
    for chunk in samples.chunks(samples_per_peak) {
        let mut min_value = 0.0_f32;
        let mut max_value = 0.0_f32;
        for sample in chunk {
            let sample = *sample;
            min_value = min_value.min(sample);
            max_value = max_value.max(sample);
        }
        minimums.push(min_value);
        maximums.push(max_value);
    }

    Ok(WaveformPeakData {
        duration_seconds: probe.duration_seconds,
        sample_rate,
        channels: vec![WaveformPeakChannel { minimums, maximums }],
    })
}

#[tauri::command]
fn choose_export_path(save_folder: String, file_name: String) -> Result<Option<String>, String> {
    fs::create_dir_all(&save_folder).map_err(|error| error.to_string())?;
    let path = rfd::FileDialog::new()
        .set_directory(save_folder)
        .set_file_name(file_name)
        .add_filter("MP4 Video", &["mp4"])
        .save_file();
    Ok(path.map(|path| path.to_string_lossy().to_string()))
}

#[tauri::command]
fn choose_save_folder(current: String) -> Result<Option<String>, String> {
    let mut dialog = rfd::FileDialog::new();
    if !current.trim().is_empty() {
        dialog = dialog.set_directory(current);
    }
    Ok(dialog
        .pick_folder()
        .map(|path| path.to_string_lossy().to_string()))
}

#[tauri::command]
fn choose_ffmpeg_path() -> Result<Option<String>, String> {
    let dialog = rfd::FileDialog::new();
    #[cfg(windows)]
    let dialog = dialog.add_filter("FFmpeg", &["exe"]).set_file_name("ffmpeg.exe");
    Ok(dialog
        .pick_file()
        .map(|path| path.to_string_lossy().to_string()))
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn list_audio_devices(settings: AppSettings) -> Result<Vec<String>, String> {
    let output = hidden_command(&settings.ffmpeg_path)
        .args([
            "-hide_banner",
            "-f",
            "avfoundation",
            "-list_devices",
            "true",
            "-i",
            "",
        ])
        .output()
        .map_err(|error| format!("Audio device scan failed: {}", error))?;
    let text = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    let mut in_audio_section = false;
    for line in text.lines() {
        if line.contains("AVFoundation audio devices:") {
            in_audio_section = true;
            continue;
        }
        if line.contains("AVFoundation video devices:") {
            in_audio_section = false;
            continue;
        }
        if !in_audio_section {
            continue;
        }
        // Lines look like "[AVFoundation indev @ 0x...] [0] MacBook Pro Microphone".
        if let Some((_, rest)) = line.split_once("] [") {
            if let Some((_, name)) = rest.split_once("] ") {
                let name = name.trim().to_string();
                if !name.is_empty() && !devices.contains(&name) {
                    devices.push(name);
                }
            }
        }
    }
    Ok(devices)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn list_audio_devices(settings: AppSettings) -> Result<Vec<String>, String> {
    let output = hidden_command(&settings.ffmpeg_path)
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ])
        .output()
        .map_err(|error| format!("Audio device scan failed: {}", error))?;
    let text = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    for line in text.lines() {
        if !line.contains("(audio)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(end) = line[start + 1..].find('"') {
                let name = line[start + 1..start + 1 + end].to_string();
                if !devices.contains(&name) {
                    devices.push(name);
                }
            }
        }
    }
    Ok(devices)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn open_region_selector() -> Result<Option<RecordingRegion>, String> {
    let script = r#"
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NativeDpi {
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr dpiContext);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
'@
try { [NativeDpi]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null } catch { try { [NativeDpi]::SetProcessDPIAware() | Out-Null } catch {} }
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()
$bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
$script:start = $null
$script:current = $null
$script:lastRect = $null
$script:result = $null
$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Bounds = $bounds
$form.TopMost = $true
$form.ShowInTaskbar = $false
$form.BackColor = [System.Drawing.Color]::Black
$form.Opacity = 0.42
$form.Cursor = [System.Windows.Forms.Cursors]::Cross
$form.KeyPreview = $true
$bindingFlags = [System.Reflection.BindingFlags]::Instance -bor [System.Reflection.BindingFlags]::NonPublic
$doubleBufferedProperty = $form.GetType().GetProperty('DoubleBuffered', $bindingFlags)
if ($doubleBufferedProperty) { $doubleBufferedProperty.SetValue($form, $true, $null) }
$form.Add_KeyDown({ if ($_.KeyCode -eq [System.Windows.Forms.Keys]::Escape) { $script:result = 'cancel'; $form.Close() } })
$form.Add_MouseDown({
  if ($_.Button -eq [System.Windows.Forms.MouseButtons]::Right) {
    $script:result = 'cancel'
    $form.Close()
    return
  }
  if ($_.Button -ne [System.Windows.Forms.MouseButtons]::Left) { return }
  $script:start = New-Object System.Drawing.Point -ArgumentList ($_.X + $bounds.Left), ($_.Y + $bounds.Top)
  $script:current = $script:start
  $script:lastRect = $null
  $form.Invalidate()
})
$form.Add_MouseMove({
  if ($script:start -eq $null) { return }
  $oldRect = $script:lastRect
  $script:current = New-Object System.Drawing.Point -ArgumentList ($_.X + $bounds.Left), ($_.Y + $bounds.Top)
  $left = [Math]::Min($script:start.X, $script:current.X) - $bounds.Left
  $top = [Math]::Min($script:start.Y, $script:current.Y) - $bounds.Top
  $width = [Math]::Abs($script:current.X - $script:start.X)
  $height = [Math]::Abs($script:current.Y - $script:start.Y)
  $newRect = New-Object System.Drawing.Rectangle -ArgumentList ([int]$left - 4), ([int]$top - 4), ([int]$width + 8), ([int]$height + 8)
  if ($oldRect -ne $null) {
    $form.Invalidate([System.Drawing.Rectangle]::Union($oldRect, $newRect))
  } else {
    $form.Invalidate($newRect)
  }
  $script:lastRect = $newRect
})
$form.Add_MouseUp({
  if ($script:start -eq $null) { return }
  if ($_.Button -ne [System.Windows.Forms.MouseButtons]::Left) { return }
  $end = New-Object System.Drawing.Point -ArgumentList ($_.X + $bounds.Left), ($_.Y + $bounds.Top)
  $x = [Math]::Min($script:start.X, $end.X)
  $y = [Math]::Min($script:start.Y, $end.Y)
  $w = [Math]::Abs($end.X - $script:start.X)
  $h = [Math]::Abs($end.Y - $script:start.Y)
  if ($w -ge 8 -and $h -ge 8) {
    $script:result = @{ x = $x; y = $y; width = $w; height = $h } | ConvertTo-Json -Compress
  } else {
    $script:result = 'cancel'
  }
  $form.Close()
})
$form.Add_Paint({
  if ($script:start -eq $null -or $script:current -eq $null) { return }
  $left = [Math]::Min($script:start.X, $script:current.X) - $bounds.Left
  $top = [Math]::Min($script:start.Y, $script:current.Y) - $bounds.Top
  $width = [Math]::Abs($script:current.X - $script:start.X)
  $height = [Math]::Abs($script:current.Y - $script:start.Y)
  if ($width -le 0 -or $height -le 0) { return }
  $selection = New-Object System.Drawing.Rectangle -ArgumentList $left, $top, $width, $height
  $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(90, 83, 199, 255))
  $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 65, 214, 195), 2)
  $_.Graphics.FillRectangle($brush, $selection)
  $_.Graphics.DrawRectangle($pen, $selection)
  $brush.Dispose()
  $pen.Dispose()
})
$form.Add_Shown({ $form.Activate(); $form.Focus() })
[void]$form.ShowDialog()
if ($script:result -and $script:result -ne 'cancel') { Write-Output $script:result }
"#;

    let output = hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .map_err(|error| format!("Region selector failed: {}", error))?;
    if !output.status.success() {
        return Err(format!(
            "Region selector failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<RecordingRegion>(&text)
        .map(Some)
        .map_err(|error| format!("Region selector returned invalid data: {}", error))
}

#[cfg(target_os = "macos")]
const REGION_SELECTOR_LABEL: &str = "region-selector";

/// Covers every display with a transparent selector window and waits for the
/// user to drag a region on one of them. The region is returned in global
/// display points (top-left origin of the main display), which is what
/// CoreGraphics and window positioning on macOS use.
#[cfg(target_os = "macos")]
#[tauri::command]
async fn open_region_selector(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<RecordingRegion>, String> {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let (sender, receiver) = mpsc::channel();
    {
        let mut pending = state
            .region_selection
            .lock()
            .map_err(|_| "Region selector lock failed.".to_string())?;
        if let Some((_, previous)) = pending.replace((generation, sender)) {
            let _ = previous.send(None);
        }
    }
    close_region_selectors(&app);

    let monitors = app
        .available_monitors()
        .map_err(|error| format!("Could not list displays: {error}"))?;
    if monitors.is_empty() {
        return Err("No displays are available for recording.".to_string());
    }
    // Started from the hotkey or the menu bar while another app (usually a
    // game) is in front. Activating the selector would bring the editor into
    // view behind it, so keep the editor hidden until the recording stops, as
    // on Windows, and give focus back to that app once a region is chosen.
    let previous_app = on_main_thread(&app, || unsafe { frontmost_other_app() }).flatten();
    if previous_app.is_some() {
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.hide();
        }
    }
    let restore = |app: &tauri::AppHandle| {
        if let Some(pid) = previous_app {
            on_main_thread(app, move || unsafe { activate_app(pid) });
        }
    };

    if let Err(error) = open_region_selector_windows(&app, generation, &monitors) {
        let _ = state.region_selection.lock().map(|mut pending| pending.take());
        close_region_selectors(&app);
        restore(&app);
        return Err(error);
    }
    // Take keyboard focus so Escape reaches the selector instead of the game.
    on_main_thread(&app, || unsafe { activate_current_app() });

    let selection = tauri::async_runtime::spawn_blocking(move || receiver.recv().ok().flatten())
        .await
        .map_err(|error| format!("Region selector failed: {error}"))?;
    close_region_selectors(&app);
    restore(&app);
    Ok(selection)
}

/// Runs `task` on the AppKit main thread and waits for its result. Must not
/// be called from the main thread itself.
#[cfg(target_os = "macos")]
fn on_main_thread<T: Send + 'static>(
    app: &tauri::AppHandle,
    task: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = sender.send(task());
    })
    .ok()?;
    receiver.recv_timeout(Duration::from_secs(2)).ok()
}

/// The process id of the frontmost app, unless that is QuickClipper.
#[cfg(target_os = "macos")]
unsafe fn frontmost_other_app() -> Option<i32> {
    use objc2::{class, msg_send, runtime::AnyObject};
    let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
    let frontmost: *mut AnyObject = msg_send![workspace, frontmostApplication];
    if frontmost.is_null() {
        return None;
    }
    let pid: i32 = msg_send![frontmost, processIdentifier];
    (pid != std::process::id() as i32).then_some(pid)
}

#[cfg(target_os = "macos")]
unsafe fn activate_app(pid: i32) {
    use objc2::{class, msg_send, runtime::AnyObject};
    let app: *mut AnyObject =
        msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
    if !app.is_null() {
        let _: bool = msg_send![app, activateWithOptions: 0_usize];
    }
}

#[cfg(target_os = "macos")]
unsafe fn activate_current_app() {
    use objc2::{class, msg_send, runtime::AnyObject};
    let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![ns_app, activateIgnoringOtherApps: true];
}

#[cfg(target_os = "macos")]
fn open_region_selector_windows(
    app: &tauri::AppHandle,
    generation: u64,
    monitors: &[tauri::Monitor],
) -> Result<(), String> {
    let cursor = app.cursor_position().ok();
    for (index, monitor) in monitors.iter().enumerate() {
        let scale = monitor.scale_factor();
        let origin = monitor.position().to_logical::<f64>(scale);
        let size = monitor.size().to_logical::<f64>(scale);
        let url = format!(
            "index.html?selector=1&originX={}&originY={}",
            origin.x, origin.y
        );
        let contains_cursor = cursor.is_some_and(|point| {
            let point = point.to_logical::<f64>(scale);
            point.x >= origin.x
                && point.y >= origin.y
                && point.x < origin.x + size.width
                && point.y < origin.y + size.height
        });
        let window = tauri::WebviewWindowBuilder::new(
            app,
            format!("{REGION_SELECTOR_LABEL}-{generation}-{index}"),
            tauri::WebviewUrl::App(url.into()),
        )
        .title("QuickClipper Region")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .focused(contains_cursor || index == 0)
        .inner_size(size.width, size.height)
        .position(origin.x, origin.y)
        .build()
        .map_err(|error| format!("Region selector failed: {error}"))?;
        raise_above_menu_bar(&window);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn finish_region_selection(
    region: Option<RecordingRegion>,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<(), String> {
    deliver_region_selection(&state, window.label(), region);
    Ok(())
}

/// Hands the result to the selection that opened `label`, ignoring selector
/// windows left over from an earlier selection.
#[cfg(target_os = "macos")]
fn deliver_region_selection(state: &AppState, label: &str, region: Option<RecordingRegion>) {
    let Some(generation) = label
        .strip_prefix(REGION_SELECTOR_LABEL)
        .and_then(|rest| rest.split('-').nth(1))
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    let Ok(mut pending) = state.region_selection.lock() else {
        return;
    };
    if pending.as_ref().is_some_and(|(current, _)| *current == generation) {
        if let Some((_, sender)) = pending.take() {
            let _ = sender.send(region);
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn finish_region_selection(_region: Option<RecordingRegion>) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn close_region_selectors(app: &tauri::AppHandle) {
    for (label, window) in app.webview_windows() {
        if label.starts_with(REGION_SELECTOR_LABEL) {
            let _ = window.destroy();
        }
    }
}

/// Floating windows sit below the menu bar and Dock, and stay out of other
/// apps' full-screen Spaces. The selector and the recording overlay have to
/// reach every pixel a region can include, including full-screen games (this
/// also relies on QuickClipper being an accessory, menu-bar app).
#[cfg(target_os = "macos")]
fn raise_above_menu_bar(window: &tauri::WebviewWindow) {
    const NS_STATUS_WINDOW_LEVEL: isize = 25;
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const STATIONARY: usize = 1 << 4;
    const IGNORES_CYCLE: usize = 1 << 6;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    let Ok(ns_window) = window.ns_window() else {
        return;
    };
    let ns_window = ns_window as usize;
    let _ = window.run_on_main_thread(move || unsafe {
        let ns_window = ns_window as *mut objc2::runtime::AnyObject;
        let _: () = objc2::msg_send![ns_window, setLevel: NS_STATUS_WINDOW_LEVEL];
        let behavior = CAN_JOIN_ALL_SPACES | STATIONARY | IGNORES_CYCLE | FULL_SCREEN_AUXILIARY;
        let _: () = objc2::msg_send![ns_window, setCollectionBehavior: behavior];
        // Re-order so the new behavior moves the window into the Space that
        // is showing on its display right now.
        let _: () = objc2::msg_send![ns_window, orderFrontRegardless];
    });
}

#[tauri::command]
fn begin_export(state: State<'_, AppState>) -> Result<String, String> {
    state.exports.reserve()
}

#[tauri::command]
fn cancel_export(job_id: String, state: State<'_, AppState>) -> Result<bool, String> {
    state.exports.cancel(&job_id)
}

#[tauri::command]
async fn export_clip(request: ExportRequest, job_id: String, state: State<'_, AppState>) -> Result<ExportResult, String> {
    let job = state.exports.claim(&job_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let encoder_key = request.settings.export_encoder_key.clone();
        let output_path = request.output_path.clone();
        export_with_encoder(&request, &encoder_key, &output_path, &job.control)
    })
    .await
    .map_err(|error| format!("Export worker failed: {}", error))?
}

fn effective_audio_quality(settings: &AppSettings) -> &str {
    match settings.audio_quality.as_str() {
        "best" | "optimized" | "standard" => settings.audio_quality.as_str(),
        _ if settings.best_audio => "best",
        _ => "standard",
    }
}

fn determine_audio_kbps(settings: &AppSettings, duration: f64, has_audio_output: bool) -> u32 {
    if !has_audio_output {
        return 0;
    }
    let quality = effective_audio_quality(settings);
    let target = if quality == "standard" { 160 } else { 320 };
    if !settings.size_cap_enabled {
        return target;
    }
    let total_kbps =
        ((settings.max_megabytes.max(0.1) * 8192.0 * 0.985) / duration.max(0.5)) as u32;
    let cap_target = if quality == "standard" { 128 } else { 320 };
    cap_target.min(total_kbps.saturating_sub(250)).max(64)
}

fn export_with_encoder(
    request: &ExportRequest,
    encoder_key: &str,
    output_path: &str,
    control: &ExportControl,
) -> Result<ExportResult, String> {
    control.check()?;
    let output = Path::new(output_path);
    ensure_parent(output)?;
    let parent = output.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    // Keep attempts on the destination volume, then publish only a complete export.
    let staging = tempfile::Builder::new().prefix(".quickclipper-export-").tempdir_in(parent)
        .map_err(|error| error.to_string())?;
    let staged_path = staging.path().join(output.file_name().ok_or("Invalid export filename")?);
    let mut result = export_to_path(request, encoder_key, &staged_path.to_string_lossy(), control)?;
    control.publish(&staged_path, output)?;
    result.path = output_path.to_string();
    Ok(result)
}

fn export_to_path(
    request: &ExportRequest,
    encoder_key: &str,
    output_path: &str,
    control: &ExportControl,
) -> Result<ExportResult, String> {
    let started = Instant::now();
    ensure_parent(Path::new(output_path))?;
    if !request.settings.include_video {
        let bytes = export_audio_only(request, Path::new(output_path), control)?;
        return Ok(ExportResult {
            path: output_path.to_string(),
            bytes,
            seconds: started.elapsed().as_secs_f64(),
        });
    }

    let duration = kept_duration(request.start, request.end, &request.cuts);
    if duration <= 0.01 {
        return Err("Trim range is empty.".to_string());
    }

    let source_path = Path::new(&request.input_path);
    let source_info = probe_video_file_with_control(&request.settings, source_path, source_path, control)?
        .ok_or_else(|| "Could not read source video.".to_string())?;
    let source_width = source_info.width;
    let source_height = source_info.height;
    let source_duration = source_info.duration_seconds;
    let source_has_audio = source_has_audio_with_control(&request.settings, source_path, control)?;
    let export_crop = clamp_export_crop(&request.crop, source_width, source_height);

    let audio_kept = if request.settings.include_audio {
        kept_segments(request.audio_start, request.audio_end, &request.audio_cuts)
    } else {
        Vec::new()
    };
    let has_audio_output = source_has_audio && !audio_kept.is_empty();
    let audio_kbps = determine_audio_kbps(&request.settings, duration, has_audio_output);

    let bytes = if request.settings.size_cap_enabled {
        export_size_capped(
            request,
            encoder_key,
            Path::new(output_path),
            &export_crop,
            source_width,
            source_height,
            source_has_audio,
            duration,
            audio_kbps,
            control,
        )?
    } else if can_copy_source_export(
        request,
        &export_crop,
        source_width,
        source_height,
        source_duration,
        source_has_audio,
    ) {
        control.copy(source_path, Path::new(output_path))?
    } else {
        export_once(
            request,
            encoder_key,
            Path::new(output_path),
            &export_crop,
            source_width,
            source_height,
            source_has_audio,
            None,
            audio_kbps,
            control,
        )?
    };

    Ok(ExportResult {
        path: output_path.to_string(),
        bytes,
        seconds: started.elapsed().as_secs_f64(),
    })
}

fn export_audio_only(request: &ExportRequest, output_path: &Path, control: &ExportControl) -> Result<u64, String> {
    if !request.settings.include_audio {
        return Err("Audio-only export needs Audio enabled.".to_string());
    }
    let source_path = Path::new(&request.input_path);
    if !source_has_audio_with_control(&request.settings, source_path, control)? {
        return Err("Source has no audio stream.".to_string());
    }
    let audio_kept = kept_segments(request.audio_start, request.audio_end, &request.audio_cuts);
    let duration: f64 = audio_kept
        .iter()
        .map(|(start, end)| end - start)
        .sum();
    if duration <= 0.01 {
        return Err("Audio trim range is empty.".to_string());
    }

    let profile = choose_audio_wav_profile(&request.settings, duration);
    let args = build_audio_only_export_args(request, output_path, &audio_kept, profile, duration);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(error) = control.run(&request.settings.ffmpeg_path, &arg_refs, "Audio export failed") {
        remove_empty_file(output_path);
        return Err(error);
    }
    let bytes = fs::metadata(output_path)
        .map(|metadata| metadata.len())
        .map_err(|error| error.to_string())?;
    if request.settings.size_cap_enabled && bytes > target_size_bytes(request.settings.max_megabytes) {
        let _ = fs::remove_file(output_path);
        return Err(format!(
            "Audio WAV exceeds {:.1} MB. Trim audio or raise Max MB.",
            request.settings.max_megabytes
        ));
    }
    Ok(bytes)
}

fn build_audio_only_export_args(
    request: &ExportRequest,
    output_path: &Path,
    audio_kept: &[(f64, f64)],
    profile: AudioWavProfile,
    duration: f64,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        request.input_path.clone(),
    ];
    if request.audio_cuts.is_empty() {
        args.extend([
            "-ss".to_string(),
            seconds(request.audio_start),
            "-t".to_string(),
            seconds(duration),
            "-map".to_string(),
            "0:a:0".to_string(),
        ]);
        if request.settings.audio_gain_db.abs() > 0.01 {
            args.extend([
                "-af".to_string(),
                format!("volume={:.6}", db_to_linear(request.settings.audio_gain_db)),
            ]);
        }
    } else {
        args.extend([
            "-filter_complex".to_string(),
            build_audio_only_filter(request, audio_kept),
            "-map".to_string(),
            "[outa]".to_string(),
        ]);
    }
    args.extend([
        "-vn".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
        "-c:a".to_string(),
        profile.codec.to_string(),
        "-ar".to_string(),
        profile.sample_rate.to_string(),
        "-ac".to_string(),
        profile.channels.to_string(),
        output_path.to_string_lossy().to_string(),
    ]);
    args
}

fn build_audio_only_filter(request: &ExportRequest, audio_kept: &[(f64, f64)]) -> String {
    let mut parts = Vec::new();
    for (index, (start, end)) in audio_kept.iter().enumerate() {
        let output = if audio_kept.len() == 1 {
            "[outa]".to_string()
        } else {
            format!("[a{}]", index)
        };
        let mut filter = format!(
            "[0:a]atrim=start={}:end={},asetpts=PTS-STARTPTS",
            seconds(*start),
            seconds(*end)
        );
        if request.settings.audio_gain_db.abs() > 0.01 {
            filter.push_str(&format!(
                ",volume={:.6}",
                db_to_linear(request.settings.audio_gain_db)
            ));
        }
        filter.push_str(&output);
        parts.push(filter);
    }
    if audio_kept.len() > 1 {
        let inputs = (0..audio_kept.len())
            .map(|index| format!("[a{}]", index))
            .collect::<Vec<_>>()
            .join("");
        parts.push(format!("{}concat=n={}:v=0:a=1[outa]", inputs, audio_kept.len()));
    }
    parts.join(";")
}

fn export_size_capped(
    request: &ExportRequest,
    encoder_key: &str,
    output_path: &Path,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
    source_has_audio: bool,
    duration: f64,
    audio_kbps: u32,
    control: &ExportControl,
) -> Result<u64, String> {
    let target_bytes = target_size_bytes(request.settings.max_megabytes);
    let minimum_target_bytes = (target_bytes as f64 * 0.97).round() as u64;
    let max_quality_kbps = request.settings.quality_target_kbps.clamp(500, 50_000);
    let mut low = 250_u32;
    let mut high = max_quality_kbps.max(low);
    let mut next_kbps =
        calculate_video_bitrate(request.settings.max_megabytes, duration, audio_kbps, high);
    let mut best_under: Option<ExportAttemptResult> = None;
    let mut smallest_over: Option<ExportAttemptResult> = None;
    let mut last_error: Option<String> = None;
    let mut attempts = Vec::new();

    for attempt in 0..5 {
        let attempt_path = attempt_output_path(output_path, attempt);
        attempts.push(attempt_path.clone());
        match export_once(
            request,
            encoder_key,
            &attempt_path,
            crop,
            source_width,
            source_height,
            source_has_audio,
            Some(next_kbps),
            audio_kbps,
            control,
        ) {
            Ok(bytes) => {
                let result = ExportAttemptResult {
                    path: attempt_path,
                    bytes,
                    kbps: next_kbps,
                };
                if bytes <= target_bytes {
                    if best_under.as_ref().map_or(true, |best| bytes > best.bytes) {
                        best_under = Some(result);
                    }
                    if bytes >= minimum_target_bytes {
                        break;
                    }
                    low = next_kbps.saturating_add(1).min(high);
                    if low >= high {
                        break;
                    }
                    let ratio = target_bytes as f64 / bytes.max(1) as f64;
                    next_kbps =
                        ((next_kbps as f64 * ratio * 0.995).round() as u32).clamp(low, high);
                } else {
                    if smallest_over
                        .as_ref()
                        .map_or(true, |best| bytes < best.bytes)
                    {
                        smallest_over = Some(result);
                    }
                    high = next_kbps.saturating_sub(1).max(low);
                    if low >= high {
                        break;
                    }
                    let ratio = target_bytes as f64 / bytes.max(1) as f64;
                    next_kbps =
                        ((next_kbps as f64 * ratio * 0.985).round() as u32).clamp(low, high);
                }
            }
            Err(error) => {
                last_error = Some(error);
                break;
            }
        }
    }

    // Cancellation must discard even a successful earlier size-cap attempt.
    control.check()?;
    if let Some(best) = best_under {
        control.copy(&best.path, output_path)?;
        cleanup_attempts(&attempts, Some(&best.path));
        return Ok(fs::metadata(output_path)
            .map_err(|error| error.to_string())?
            .len());
    }

    cleanup_attempts(&attempts, None);
    if let Some(over) = smallest_over {
        return Err(format!(
            "Export could not fit under {:.1} MB after 5 tries. Smallest result was {} at {} kbps.",
            request.settings.max_megabytes,
            format_bytes_raw(over.bytes),
            over.kbps
        ));
    }
    Err(last_error
        .unwrap_or_else(|| "Export failed before producing a size-capped file.".to_string()))
}

fn export_once(
    request: &ExportRequest,
    encoder_key: &str,
    output_path: &Path,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
    source_has_audio: bool,
    video_kbps: Option<u32>,
    audio_kbps: u32,
    control: &ExportControl,
) -> Result<u64, String> {
    let args = build_export_args(
        request,
        encoder_key,
        &output_path.to_string_lossy(),
        crop,
        source_width,
        source_height,
        source_has_audio,
        video_kbps,
        audio_kbps,
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(error) = control.run(&request.settings.ffmpeg_path, &arg_refs, "Export failed") {
        remove_empty_file(output_path);
        return Err(error);
    }
    fs::metadata(output_path)
        .map(|metadata| metadata.len())
        .map_err(|error| error.to_string())
}

fn build_export_args(
    request: &ExportRequest,
    encoder_key: &str,
    output_path: &str,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
    source_has_audio: bool,
    video_kbps: Option<u32>,
    audio_kbps: u32,
) -> Vec<String> {
    let duration = kept_duration(request.start, request.end, &request.cuts);
    let video_kept = kept_segments(request.start, request.end, &request.cuts);
    let audio_kept = if request.settings.include_audio {
        kept_segments(request.audio_start, request.audio_end, &request.audio_cuts)
    } else {
        Vec::new()
    };
    let has_audio_output = source_has_audio && !audio_kept.is_empty();
    let audio_edit_matches_video = ranges_equal(&video_kept, &audio_kept);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        request.input_path.clone(),
    ];

    if request.cuts.is_empty() && (!has_audio_output || audio_edit_matches_video) {
        args.extend([
            "-ss".to_string(),
            seconds(request.start),
            "-t".to_string(),
            seconds(duration),
        ]);
        let filters = build_filters(request, crop, source_width, source_height);
        if !filters.is_empty() {
            args.extend(["-vf".to_string(), filters]);
        }
        args.extend(["-map".to_string(), "0:v:0".to_string()]);
        if has_audio_output {
            args.extend([
                "-map".to_string(),
                "0:a?".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                format!("{}k", audio_kbps.max(64)),
            ]);
            if request.settings.audio_gain_db.abs() > 0.01 {
                args.extend([
                    "-af".to_string(),
                    format!("volume={:.6}", db_to_linear(request.settings.audio_gain_db)),
                ]);
            }
        } else {
            args.push("-an".to_string());
        }
    } else {
        let (filter_complex, has_audio) =
            build_cut_filter(request, crop, source_width, source_height, source_has_audio);
        args.extend(["-filter_complex".to_string(), filter_complex]);
        if has_audio {
            args.extend([
                "-map".to_string(),
                "[outv]".to_string(),
                "-map".to_string(),
                "[outa]".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                format!("{}k", audio_kbps.max(64)),
            ]);
        } else {
            args.extend(["-map".to_string(), "[outv]".to_string(), "-an".to_string()]);
        }
    }

    args.extend([
        "-r".to_string(),
        request.settings.frame_rate.clamp(5, 60).to_string(),
    ]);
    args.extend(encoder_args(encoder_key));
    if let Some(kbps) = video_kbps {
        args.extend([
            "-b:v".to_string(),
            format!("{}k", kbps),
            "-maxrate".to_string(),
            format!("{}k", kbps),
            "-bufsize".to_string(),
            format!("{}k", kbps.saturating_mul(2)),
        ]);
    } else {
        args.extend(preserve_quality_args(encoder_key));
    }
    args.extend([
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        output_path.to_string(),
    ]);
    args
}

fn can_copy_source_export(
    request: &ExportRequest,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
    source_duration: f64,
    source_has_audio: bool,
) -> bool {
    let video_is_full_source =
        request.start <= 0.001 && request.cuts.is_empty() && (request.end - source_duration).abs() <= 0.05;
    let audio_is_full_source = !source_has_audio
        || (request.settings.include_audio
            && request.audio_start <= 0.001
            && request.audio_cuts.is_empty()
            && (request.audio_end - request.end).abs() <= 0.05
            && request.settings.audio_gain_db.abs() <= 0.01);
    let transform_is_source = crop.x == 0
        && crop.y == 0
        && crop.width == source_width
        && crop.height == source_height
        && !request.auto_fit720
        && make_even(request.output_width) == source_width
        && make_even(request.output_height) == source_height;
    video_is_full_source && audio_is_full_source && transform_is_source
}

#[tauri::command]
async fn benchmark_encoders(request: ExportRequest, job_id: String, state: State<'_, AppState>) -> Result<Vec<BenchmarkResult>, String> {
    let job = state.exports.claim(&job_id)?;
    tauri::async_runtime::spawn_blocking(move || benchmark_encoders_sync(request, &job.control))
        .await
        .map_err(|error| format!("Benchmark worker failed: {}", error))?
}

fn benchmark_encoders_sync(request: ExportRequest, control: &ExportControl) -> Result<Vec<BenchmarkResult>, String> {
    let mut results = Vec::new();
    let output = PathBuf::from(&request.output_path);
    let folder = output
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("benchmark");

    for (encoder_key, encoder_label) in encoder_presets() {
        control.check()?;
        if request
            .settings
            .unsupported_encoder_keys
            .iter()
            .any(|key| key == encoder_key)
        {
            continue;
        }
        let path = folder.join(format!("{}-{}.mp4", stem, encoder_key));
        match export_with_encoder(&request, encoder_key, &path.to_string_lossy(), control) {
            Ok(result) => results.push(BenchmarkResult {
                encoder_key: encoder_key.to_string(),
                encoder_label: encoder_label.to_string(),
                path: result.path,
                bytes: result.bytes,
                seconds: result.seconds,
                success: true,
                error: String::new(),
            }),
            Err(error) => results.push(BenchmarkResult {
                encoder_key: encoder_key.to_string(),
                encoder_label: encoder_label.to_string(),
                path: path.to_string_lossy().to_string(),
                bytes: 0,
                seconds: 0.0,
                success: false,
                error,
            }),
        }
    }

    control.check()?;
    if results.iter().all(|result| !result.success) {
        return Err(results
            .iter()
            .map(|result| format!("{}: {}", result.encoder_label, result.error))
            .collect::<Vec<_>>()
            .join("\n"));
    }

    Ok(results)
}

#[tauri::command]
fn start_recording(
    request: RecordingRequest,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut recording = state
        .recording
        .lock()
        .map_err(|_| "Recording lock failed.".to_string())?;
    if recording.is_some() {
        return Err("Recording is already running.".to_string());
    }

    let active = spawn_recording(request, &app)?;
    let path = active.path.clone();
    *recording = Some(active);
    Ok(path.to_string_lossy().to_string())
}

fn spawn_recording(request: RecordingRequest, app: &tauri::AppHandle) -> Result<ActiveRecording, String> {
    let path = media_root()?.join(format!("recording-{}.mp4", timestamp()));
    let width = make_even(request.width).max(2);
    let height = make_even(request.height).max(2);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-stats_period".to_string(),
        "0.1".to_string(),
        "-y".to_string(),
    ];
    args.extend(recording_input_args(&request, width, height)?);

    args.push("-an".to_string());

    args.extend([
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "ultrafast".to_string(),
        "-crf".to_string(),
        "12".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "-tune".to_string(),
        "zerolatency".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        path.to_string_lossy().to_string(),
    ]);

    // Make audio readiness part of recording startup. This both guarantees that
    // every master has audio and gives muxing a measured lead to trim.
    let audio = start_loopback_recording()
        .map_err(|error| format!("Could not start system audio capture: {}", error))?;
    let mut command = hidden_command(&request.settings.ffmpeg_path);
    let mut child = match command
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = stop_loopback_recording(Some(audio));
            return Err(format!("Could not start FFmpeg: {}", error));
        }
    };
    let video_started_at = Instant::now();
    let audio_lead_seconds = video_started_at
        .checked_duration_since(audio.started_at)
        .unwrap_or_default()
        .as_secs_f64();
    let audio = Some(audio);

    thread::sleep(Duration::from_millis(250));
    if child
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        let _ = stop_loopback_recording(audio);
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(format!("Recording failed to start: {}", stderr.trim()));
    }

    let overlay_child =
        start_recording_overlay(app, std::process::id(), request.x, request.y, width, height).ok();
    Ok(ActiveRecording {
        child,
        overlay_child,
        audio,
        audio_lead_seconds,
        path,
    })
}

#[derive(Debug, Clone, Copy)]
struct CaptureDisplay {
    output_idx: u32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(target_os = "macos")]
fn recording_input_args(
    request: &RecordingRequest,
    _width: u32,
    _height: u32,
) -> Result<Vec<String>, String> {
    avfoundation_input_args(request, request.settings.frame_rate.clamp(5, 60))
}

#[cfg(not(target_os = "macos"))]
fn recording_input_args(
    request: &RecordingRequest,
    width: u32,
    height: u32,
) -> Result<Vec<String>, String> {
    let fps = request.settings.frame_rate.clamp(5, 60);
    match request.settings.capture_backend {
        CaptureBackend::Gdi => Ok(vec![
            "-f".into(),
            "gdigrab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-offset_x".into(),
            request.x.to_string(),
            "-offset_y".into(),
            request.y.to_string(),
            "-video_size".into(),
            format!("{width}x{height}"),
            "-i".into(),
            "desktop".into(),
        ]),
        CaptureBackend::DesktopDuplication => {
            ensure_desktop_duplication_supported(&request.settings.ffmpeg_path)?;
            let displays = desktop_duplication_displays()?;
            let (output_idx, offset_x, offset_y) = select_desktop_duplication_display(
                request.x,
                request.y,
                width,
                height,
                &displays,
            )?;
            Ok(vec![
                "-filter_complex".into(),
                format!(
                    "ddagrab=output_idx={output_idx}:framerate={fps}:video_size={width}x{height}:offset_x={offset_x}:offset_y={offset_y},hwdownload,format=bgra"
                ),
            ])
        }
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn ensure_desktop_duplication_supported(ffmpeg_path: &str) -> Result<(), String> {
    let output = hidden_command(ffmpeg_path)
        .args(["-hide_banner", "-h", "filter=ddagrab"])
        .output()
        .map_err(|error| format!("Could not check FFmpeg Desktop Duplication support: {error}"))?;
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() && help.contains("Filter ddagrab") {
        Ok(())
    } else {
        Err("This FFmpeg build does not support Desktop Duplication (ddagrab). Choose GDI in Capture Method or select a newer FFmpeg build.".to_string())
    }
}

fn select_desktop_duplication_display(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    displays: &[CaptureDisplay],
) -> Result<(u32, i32, i32), String> {
    let right = i64::from(x) + i64::from(width);
    let bottom = i64::from(y) + i64::from(height);
    for display in displays {
        if x >= display.left
            && y >= display.top
            && right <= i64::from(display.right)
            && bottom <= i64::from(display.bottom)
        {
            let offset_x = i32::try_from(i64::from(x) - i64::from(display.left))
                .map_err(|_| "Recording region coordinates are too large.".to_string())?;
            let offset_y = i32::try_from(i64::from(y) - i64::from(display.top))
                .map_err(|_| "Recording region coordinates are too large.".to_string())?;
            return Ok((
                display.output_idx,
                offset_x,
                offset_y,
            ));
        }
    }
    Err("Desktop Duplication needs the recording region to fit within one display. Select a region on one monitor or choose GDI in Capture Method.".to_string())
}

#[cfg(windows)]
fn desktop_duplication_displays() -> Result<Vec<CaptureDisplay>, String> {
    use windows::{
        core::Interface,
        Win32::{
            Graphics::{
                Direct3D::D3D_DRIVER_TYPE_HARDWARE,
                Direct3D11::{
                    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, ID3D11Device,
                },
                Dxgi::IDXGIDevice,
            },
            UI::HiDpi::{
                SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
        },
    };

    struct RestoreDpiContext(DPI_AWARENESS_CONTEXT);
    impl Drop for RestoreDpiContext {
        fn drop(&mut self) {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }

    // The region selector and FFmpeg use physical pixels. DXGI output bounds
    // are virtualized for DPI-unaware callers unless this thread is DPI aware.
    let previous_dpi_context =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _restore_dpi_context = RestoreDpiContext(previous_dpi_context);

    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|error| format!("Could not identify the Desktop Duplication display: {error}"))?;
    let device = device.ok_or("Direct3D did not return a capture device.")?;
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| format!("Could not identify the graphics adapter: {error}"))?;
    let adapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|error| format!("Could not identify the graphics adapter: {error}"))?;
    let mut displays = Vec::new();
    for output_idx in 0..32 {
        let output = match unsafe { adapter.EnumOutputs(output_idx) } {
            Ok(output) => output,
            Err(_) => break,
        };
        let desc = unsafe { output.GetDesc() }
            .map_err(|error| format!("Could not read a display's bounds: {error}"))?;
        if desc.AttachedToDesktop.as_bool() {
            let rect = desc.DesktopCoordinates;
            displays.push(CaptureDisplay {
                output_idx,
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            });
        }
    }
    if displays.is_empty() {
        return Err("Desktop Duplication found no active displays on the capture adapter.".to_string());
    }
    Ok(displays)
}

#[cfg(not(windows))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn desktop_duplication_displays() -> Result<Vec<CaptureDisplay>, String> {
    Err("Desktop Duplication is only available on Windows.".to_string())
}

/// macOS records through FFmpeg's AVFoundation screen input. Regions arrive in
/// global display points; the capture runs at the display's backing pixel
/// resolution, so the crop is scaled into pixels of the chosen screen.
#[cfg(target_os = "macos")]
fn avfoundation_input_args(request: &RecordingRequest, fps: u32) -> Result<Vec<String>, String> {
    let screens = macos_capture_screens()?;
    let displays: Vec<CaptureDisplay> = screens.iter().map(|screen| screen.bounds).collect();
    let (screen_index, offset_x, offset_y) = select_desktop_duplication_display(
        request.x,
        request.y,
        request.width,
        request.height,
        &displays,
    )
    .map_err(|_| "The recording region has to fit within one display. Select a region on one screen.".to_string())?;
    let screen = screens
        .iter()
        .find(|screen| screen.bounds.output_idx == screen_index)
        .ok_or("The selected display is no longer available.")?;
    let to_pixels = |points: f64| (points * screen.scale).round().max(0.0) as u32;
    let crop_x = to_pixels(f64::from(offset_x)).min(screen.pixel_width.saturating_sub(2));
    let crop_y = to_pixels(f64::from(offset_y)).min(screen.pixel_height.saturating_sub(2));
    let crop_width = make_even(to_pixels(f64::from(request.width)).min(screen.pixel_width - crop_x)).max(2);
    let crop_height = make_even(to_pixels(f64::from(request.height)).min(screen.pixel_height - crop_y)).max(2);
    Ok(vec![
        "-f".into(),
        "avfoundation".into(),
        "-capture_cursor".into(),
        "1".into(),
        "-framerate".into(),
        fps.to_string(),
        "-pixel_format".into(),
        "nv12".into(),
        "-i".into(),
        format!("Capture screen {screen_index}:none"),
        "-vf".into(),
        format!("crop={crop_width}:{crop_height}:{crop_x}:{crop_y}"),
    ])
}

/// Reads the host-clock time of the first captured frame from FFmpeg's input
/// summary ("Input #0, avfoundation, ..." followed by "Duration: N/A, start: 1234.5, ...").
#[cfg(target_os = "macos")]
fn avfoundation_start_seconds(stderr: &str) -> Option<f64> {
    let mut lines = stderr.lines().skip_while(|line| !line.contains("Input #0, avfoundation"));
    lines.next()?;
    lines.take(3).find_map(|line| {
        let (_, rest) = line.split_once("start: ")?;
        rest.split(',').next()?.trim().parse().ok()
    })
}

#[cfg(target_os = "macos")]
struct MacCaptureScreen {
    /// Bounds in global points, indexed like FFmpeg's "Capture screen N".
    bounds: CaptureDisplay,
    scale: f64,
    pixel_width: u32,
    pixel_height: u32,
}

#[cfg(target_os = "macos")]
fn macos_capture_screens() -> Result<Vec<MacCaptureScreen>, String> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
        fn CGDisplayModeRelease(mode: *mut c_void);
    }

    // FFmpeg numbers "Capture screen N" in CGGetActiveDisplayList order.
    let mut ids = [0_u32; 32];
    let mut count = 0_u32;
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 {
        return Err(format!("Could not list displays (CoreGraphics error {status})."));
    }
    let mut screens = Vec::new();
    for (index, id) in ids.iter().take(count as usize).enumerate() {
        let bounds = unsafe { CGDisplayBounds(*id) };
        let (pixel_width, pixel_height) = unsafe {
            let mode = CGDisplayCopyDisplayMode(*id);
            if mode.is_null() {
                (bounds.size.width as usize, bounds.size.height as usize)
            } else {
                let size = (CGDisplayModeGetPixelWidth(mode), CGDisplayModeGetPixelHeight(mode));
                CGDisplayModeRelease(mode);
                size
            }
        };
        if bounds.size.width <= 0.0 || pixel_width == 0 || pixel_height == 0 {
            continue;
        }
        let left = bounds.origin.x.round() as i32;
        let top = bounds.origin.y.round() as i32;
        screens.push(MacCaptureScreen {
            bounds: CaptureDisplay {
                output_idx: index as u32,
                left,
                top,
                right: left + bounds.size.width.round() as i32,
                bottom: top + bounds.size.height.round() as i32,
            },
            scale: pixel_width as f64 / bounds.size.width,
            pixel_width: pixel_width as u32,
            pixel_height: pixel_height as u32,
        });
    }
    if screens.is_empty() {
        return Err("No active displays are available for recording.".to_string());
    }
    Ok(screens)
}

#[tauri::command]
fn stop_recording(settings: AppSettings, state: State<'_, AppState>) -> Result<VideoInfo, String> {
    let mut guard = state
        .recording
        .lock()
        .map_err(|_| "Recording lock failed.".to_string())?;
    let Some(mut recording) = guard.take() else {
        return Err("No recording is running.".to_string());
    };

    stop_recording_overlay(&mut recording.overlay_child);
    #[cfg(target_os = "macos")]
    let audio_host_started_at = recording
        .audio
        .as_ref()
        .map(|audio| host_time_seconds() - audio.started_at.elapsed().as_secs_f64());
    let mut stop_signal_error = None;
    if let Some(stdin) = recording.child.stdin.as_mut() {
        if let Err(error) = stdin.write_all(b"q\n") {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                stop_signal_error = Some(error.to_string());
            }
        }
    }
    let output = recording.child.wait_with_output();
    let audio_result = stop_loopback_recording(recording.audio.take());

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            if let Ok(Some(audio_path)) = audio_result {
                let _ = fs::remove_file(audio_path);
            }
            return Err(error.to_string());
        }
    };
    if !output.status.success() {
        if let Ok(Some(audio_path)) = audio_result {
            let _ = fs::remove_file(audio_path);
        }
        return Err(format!(
            "Recording failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if let Some(error) = stop_signal_error {
        if let Ok(Some(audio_path)) = audio_result {
            let _ = fs::remove_file(audio_path);
        }
        return Err(error);
    }
    // AVFoundation takes most of a second to deliver its first frame. Its
    // timestamps share the host clock with the audio capture, so measure the
    // lead from the first frame instead of from process start.
    #[cfg(target_os = "macos")]
    let audio_lead_seconds = audio_host_started_at
        .zip(avfoundation_start_seconds(&String::from_utf8_lossy(&output.stderr)))
        .map(|(audio_start, video_start)| (video_start - audio_start).max(0.0))
        .unwrap_or(recording.audio_lead_seconds);
    #[cfg(not(target_os = "macos"))]
    let audio_lead_seconds = recording.audio_lead_seconds;
    if let Some(audio_path) = audio_result? {
        mux_recording_audio(
            &settings.ffmpeg_path,
            &recording.path,
            &audio_path,
            audio_lead_seconds,
        )?;
    }

    probe_video_file(&settings, &recording.path, &recording.path)?
        .ok_or_else(|| "Recording did not produce a usable video stream.".to_string())
}

#[tauri::command]
fn reset_recording(
    request: RecordingRequest,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut guard = state
        .recording
        .lock()
        .map_err(|_| "Recording lock failed.".to_string())?;
    if let Some(mut recording) = guard.take() {
        let path = recording.path.clone();
        stop_recording_overlay(&mut recording.overlay_child);
        if let Some(stdin) = recording.child.stdin.as_mut() {
            let _ = stdin.write_all(b"q\n");
        }
        let _ = recording.child.wait();
        if let Ok(Some(audio_path)) = stop_loopback_recording(recording.audio.take()) {
            let _ = fs::remove_file(audio_path);
        }
        let _ = fs::remove_file(path);
    }

    let active = spawn_recording(request, &app)?;
    let path = active.path.clone();
    *guard = Some(active);
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    let target = PathBuf::from(path);
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        if target.is_file() {
            command.arg("-R");
        }
        command.arg(&target);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("explorer.exe");
    #[cfg(not(target_os = "macos"))]
    if target.is_file() {
        command.arg(format!("/select,{}", target.to_string_lossy()));
    } else {
        command.arg(target);
    }
    command.spawn().map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn copy_file_to_clipboard(path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err("Export file does not exist.".to_string());
    }
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| format!("Could not open clipboard: {}", error))?;
    clipboard
        .set()
        .file_list(&[path])
        .map_err(|error| format!("Could not copy file to clipboard: {}", error))
}

fn probe_video_file(
    settings: &AppSettings,
    media_path: &Path,
    original_path: &Path,
) -> Result<Option<VideoInfo>, String> {
    probe_video_file_with_control(settings, media_path, original_path, &ExportControl::default())
}

fn probe_video_file_with_control(
    settings: &AppSettings,
    media_path: &Path,
    original_path: &Path,
    control: &ExportControl,
) -> Result<Option<VideoInfo>, String> {
    let ffprobe = sibling_tool(&settings.ffmpeg_path, FFPROBE);
    let output = control.output(hidden_command(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:format=duration",
            "-of",
            "default=noprint_wrappers=1",
            &media_path.to_string_lossy(),
        ]), "Could not read video")?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut width = 0;
    let mut height = 0;
    let mut duration_seconds = 0.0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("width=") {
            width = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("height=") {
            height = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("duration=") {
            duration_seconds = value.parse().unwrap_or(0.0);
        }
    }

    if width == 0 || height == 0 || duration_seconds <= 0.0 {
        return Ok(None);
    }

    Ok(Some(VideoInfo {
        id: Uuid::new_v4().to_string(),
        path: media_path.to_string_lossy().to_string(),
        original_path: original_path.to_string_lossy().to_string(),
        width,
        height,
        duration_seconds,
    }))
}

fn is_browser_safe_playback(settings: &AppSettings, media_path: &Path) -> Result<bool, String> {
    let extension = media_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension != "mp4" && extension != "m4v" {
        return Ok(false);
    }

    let ffprobe = sibling_tool(&settings.ffmpeg_path, FFPROBE);
    let output = hidden_command(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt",
            "-of",
            "default=noprint_wrappers=1",
            &media_path.to_string_lossy(),
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Ok(false);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut codec = String::new();
    let mut pix_fmt = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("codec_name=") {
            codec = value.to_string();
        } else if let Some(value) = line.strip_prefix("pix_fmt=") {
            pix_fmt = value.to_string();
        }
    }
    Ok(codec == "h264" && pix_fmt == "yuv420p")
}

fn source_has_audio_with_control(settings: &AppSettings, media_path: &Path, control: &ExportControl) -> Result<bool, String> {
    let ffprobe = sibling_tool(&settings.ffmpeg_path, FFPROBE);
    control.output(hidden_command(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            &media_path.to_string_lossy(),
        ]), "Could not read audio")
        .map(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
}

fn import_to_workspace(source: &Path) -> Result<PathBuf, String> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mp4");
    let destination = media_root()?.join(format!("source-{}.{}", timestamp(), extension));
    fs::copy(source, &destination).map_err(|error| error.to_string())?;
    Ok(destination)
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(app_root()?.join("settings.json"))
}

fn app_root() -> Result<PathBuf, String> {
    let path = dirs::data_local_dir()
        .ok_or_else(|| "Could not resolve local app data directory.".to_string())?
        .join("QuickClipper");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn media_root() -> Result<PathBuf, String> {
    let path = app_root()?.join("Media");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn preview_root() -> Result<PathBuf, String> {
    let path = app_root()?.join("Preview");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    Ok(())
}

const FFPROBE: &str = if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" };

fn sibling_tool(ffmpeg_path: &str, tool: &str) -> String {
    let ffmpeg = Path::new(ffmpeg_path);
    if ffmpeg.is_absolute() {
        if let Some(parent) = ffmpeg.parent() {
            let sibling = parent.join(tool);
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
        }
    }
    tool.to_string()
}

fn hidden_command(program: &str) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = Command::new(resolve_program(program));
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
        let program_name = Path::new(program)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let flags = if program_name.contains("ffmpeg") {
            CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
        } else {
            CREATE_NO_WINDOW
        };
        command.creation_flags(flags);
    }
    command
}

/// Apps started from Finder or at login get a minimal PATH without Homebrew,
/// so bare tool names such as "ffmpeg" fall back to the Homebrew locations.
#[cfg(target_os = "macos")]
fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.components().count() != 1 {
        return path.to_path_buf();
    }
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search_path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|folder| folder.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn resolve_program(program: &str) -> PathBuf {
    PathBuf::from(program)
}

#[cfg(any(windows, target_os = "macos"))]
fn start_loopback_recording() -> Result<LoopbackRecording, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let path = std::env::temp_dir().join(format!("quickclipper-audio-{}.wav", timestamp()));
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || capture_loopback_to_wav(worker_stop, path, ready_sender));
    match ready_receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(started_at)) => Ok(LoopbackRecording {
            stop,
            thread,
            started_at,
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            Err("Timed out while starting system audio capture.".to_string())
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn start_loopback_recording() -> Result<LoopbackRecording, String> {
    Err("System audio capture is only available on Windows and macOS.".to_string())
}

fn stop_loopback_recording(
    recording: Option<LoopbackRecording>,
) -> Result<Option<PathBuf>, String> {
    let Some(recording) = recording else {
        return Ok(None);
    };
    recording.stop.store(true, Ordering::SeqCst);
    recording
        .thread
        .join()
        .map_err(|_| "Audio capture thread panicked.".to_string())?
}

#[cfg(windows)]
fn capture_loopback_to_wav(
    stop: Arc<AtomicBool>,
    path: PathBuf,
    ready_sender: mpsc::SyncSender<Result<Instant, String>>,
) -> Result<Option<PathBuf>, String> {
    use std::{ptr::null_mut, slice};
    use windows::Win32::{
        Media::Audio::{
            eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
            IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
            AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
                COINIT_MULTITHREADED,
            },
            Threading::Sleep,
        },
    };

    let mut ready_sender = Some(ready_sender);
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|error| error.to_string())?;
        let result = (|| -> Result<Option<PathBuf>, String> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|error| error.to_string())?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|error| error.to_string())?;
            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|error| error.to_string())?;
            let mix_format = audio_client
                .GetMixFormat()
                .map_err(|error| error.to_string())?;
            if mix_format.is_null() {
                return Err("Windows returned an empty audio mix format.".to_string());
            }

            let format = *mix_format;
            let frame_bytes = format.nBlockAlign as usize;
            if frame_bytes == 0 {
                CoTaskMemFree(Some(mix_format.cast()));
                return Err("Windows returned an invalid audio block alignment.".to_string());
            }

            // Keep-alive silent render stream: keeps Windows WASAPI audio engine mixer active
            // even when no other applications are playing sound.
            let keepalive_render: Option<(IAudioClient, IAudioRenderClient, u32)> = (|| {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        0,
                        1_000_000,
                        0,
                        mix_format,
                        None,
                    )
                    .ok()?;
                let render_service: IAudioRenderClient = client.GetService().ok()?;
                let buffer_frames = client.GetBufferSize().ok()?;
                let data = render_service.GetBuffer(buffer_frames).ok()?;
                std::ptr::write_bytes(data, 0, buffer_frames as usize * frame_bytes);
                render_service
                    .ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .ok()?;
                client.Start().ok()?;
                Some((client, render_service, buffer_frames))
            })();

            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    1_000_000,
                    0,
                    mix_format,
                    None,
                )
                .map_err(|error| error.to_string())?;

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .map_err(|error| error.to_string())?;
            let mut file = fs::File::create(&path).map_err(|error| error.to_string())?;
            let format_bytes = slice::from_raw_parts(
                mix_format.cast::<u8>(),
                std::mem::size_of::<WAVEFORMATEX>() + format.cbSize as usize,
            )
            .to_vec();
            write_wave_header(&mut file, &format_bytes, 0)?;

            let start_instant = Instant::now();
            audio_client.Start().map_err(|error| error.to_string())?;
            if let Some(sender) = ready_sender.take() {
                let _ = sender.send(Ok(start_instant));
            }

            let sample_rate = format.nSamplesPerSec as u64;
            let mut total_frames_written: u64 = 0;
            // The endpoint's device position can predate this capture stream.
            // Anchor the file timeline to the first packet instead of turning
            // that pre-existing device time into leading silence.
            let mut next_device_position: Option<u64> = None;
            let mut data_bytes: u32 = 0;

            loop {
                // Top up silent keep-alive buffer if running
                if let Some((ref render_cl, ref render_srv, buf_size)) = keepalive_render {
                    if let Ok(padding) = render_cl.GetCurrentPadding() {
                        let to_write = buf_size.saturating_sub(padding);
                        if to_write > 0 {
                            if let Ok(data) = render_srv.GetBuffer(to_write) {
                                std::ptr::write_bytes(data, 0, to_write as usize * frame_bytes);
                                let _ = render_srv
                                    .ReleaseBuffer(to_write, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
                            }
                        }
                    }
                }

                let mut next_packet_frames = capture_client
                    .GetNextPacketSize()
                    .map_err(|error| error.to_string())?;

                if next_packet_frames == 0 {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    Sleep(5);
                    continue;
                }

                while next_packet_frames > 0 {
                    let mut data = null_mut();
                    let mut frames = 0u32;
                    let mut flags = Default::default();
                    let mut device_position = 0_u64;
                    let mut _qpc_position = 0_u64;
                    capture_client
                        .GetBuffer(
                            &mut data,
                            &mut frames,
                            &mut flags,
                            Some(&mut device_position),
                            Some(&mut _qpc_position),
                        )
                        .map_err(|error| error.to_string())?;

                    let timestamp_valid =
                        (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) == 0;
                    let (gap_frames, overlap_frames, following_device_position) =
                        capture_packet_alignment(
                            next_device_position,
                            device_position,
                            frames,
                            timestamp_valid,
                        );
                    if gap_frames > 0 {
                        write_silence_frames(
                            &mut file,
                            frame_bytes,
                            gap_frames,
                            sample_rate,
                            &mut data_bytes,
                        )?;
                        total_frames_written = total_frames_written.saturating_add(gap_frames);
                    }

                    let writable_frames = frames as u64 - overlap_frames;
                    let bytes = writable_frames as usize * frame_bytes;
                    if bytes > 0 {
                        if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                            file.write_all(&vec![0u8; bytes])
                                .map_err(|error| error.to_string())?;
                        } else {
                            let byte_offset = overlap_frames as usize * frame_bytes;
                            file.write_all(slice::from_raw_parts(
                                data.cast::<u8>().add(byte_offset),
                                bytes,
                            ))
                                .map_err(|error| error.to_string())?;
                        }
                        data_bytes = data_bytes.saturating_add(bytes as u32);
                        total_frames_written = total_frames_written.saturating_add(writable_frames);
                    }
                    next_device_position = following_device_position;
                    capture_client
                        .ReleaseBuffer(frames)
                        .map_err(|error| error.to_string())?;
                    next_packet_frames = capture_client
                        .GetNextPacketSize()
                        .map_err(|error| error.to_string())?;
                }
            }

            // All queued packets have been drained. Wall-clock padding is safe
            // only now, because no delayed packet can overlap the inserted tail.
            let final_elapsed = start_instant.elapsed().as_secs_f64();
            let final_expected = (final_elapsed * sample_rate as f64) as u64;
            if final_expected > total_frames_written {
                write_silence_frames(
                    &mut file,
                    frame_bytes,
                    final_expected - total_frames_written,
                    sample_rate,
                    &mut data_bytes,
                )?;
            }

            let _ = audio_client.Stop();
            if let Some((render_cl, _, _)) = keepalive_render {
                let _ = render_cl.Stop();
            }
            finalize_wave_header(&mut file, data_bytes)?;
            CoTaskMemFree(Some(mix_format.cast()));
            if data_bytes == 0 {
                let _ = fs::remove_file(&path);
                Ok(None)
            } else {
                Ok(Some(path))
            }
        })();
        if let Some(sender) = ready_sender.take() {
            let error = result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "System audio capture stopped before startup completed.".to_string());
            let _ = sender.send(Err(error));
        }
        CoUninitialize();
        result
    }
}

/// Captures system audio through ScreenCaptureKit and writes it as 32-bit
/// float WAV on the same timeline contract as the Windows loopback capture:
/// the file starts at the moment capture is reported ready, gaps are filled
/// with silence, and the tail is padded to wall-clock length.
#[cfg(target_os = "macos")]
fn capture_loopback_to_wav(
    stop: Arc<AtomicBool>,
    path: PathBuf,
    ready_sender: mpsc::SyncSender<Result<Instant, String>>,
) -> Result<Option<PathBuf>, String> {
    use screencapturekit::{cm::CMSampleBuffer, prelude::*};

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: usize = 2;
    // Packet timestamps within this many frames of the expected position are
    // treated as contiguous, so timestamp rounding never becomes a click.
    const JITTER_FRAMES: u64 = 96;

    let mut ready_sender = Some(ready_sender);
    let result = (|| -> Result<Option<PathBuf>, String> {
        let content = SCShareableContent::get()
            .map_err(|error| screen_capture_permission_error(&error.to_string()))?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or("No display is available for system audio capture.")?;
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();
        // Only the audio of this stream is used; keep its video tiny and slow.
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_minimum_frame_interval(&CMTime::new(1, 1))
            .with_captures_audio(true)
            .with_sample_rate(SAMPLE_RATE as i32)
            .with_channel_count(CHANNELS as i32);

        let (packet_sender, packets) = mpsc::channel::<(f64, Vec<f32>)>();
        let mut stream = SCStream::new(&filter, &config);
        stream
            .add_output_handler(
                move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                    if !matches!(of_type, SCStreamOutputType::Audio) {
                        return;
                    }
                    if let Some(packet) = interleaved_audio_packet(&sample, CHANNELS) {
                        let _ = packet_sender.send(packet);
                    }
                },
                SCStreamOutputType::Audio,
            )
            .ok_or("Could not attach the system audio output.")?;

        let mut file = fs::File::create(&path).map_err(|error| error.to_string())?;
        write_wave_header(&mut file, &float_wave_format(SAMPLE_RATE, CHANNELS as u16), 0)?;

        let start_host_seconds = host_time_seconds();
        let start_instant = Instant::now();
        stream
            .start_capture()
            .map_err(|error| screen_capture_permission_error(&error.to_string()))?;
        if let Some(sender) = ready_sender.take() {
            let _ = sender.send(Ok(start_instant));
        }

        let frame_bytes = CHANNELS * std::mem::size_of::<f32>();
        let sample_rate = u64::from(SAMPLE_RATE);
        let mut next_position = Some(0_u64);
        let mut total_frames_written = 0_u64;
        let mut data_bytes = 0_u32;
        let mut write_packet = |file: &mut fs::File, (pts, samples): (f64, Vec<f32>)| -> Result<(), String> {
            let frames = (samples.len() / CHANNELS) as u64;
            if frames == 0 {
                return Ok(());
            }
            let mut position = ((pts - start_host_seconds) * sample_rate as f64).round().max(0.0) as u64;
            if let Some(expected) = next_position {
                if position.abs_diff(expected) <= JITTER_FRAMES {
                    position = expected;
                }
            }
            let (gap_frames, overlap_frames, following_position) =
                capture_packet_alignment(next_position, position, frames as u32, true);
            if gap_frames > 0 {
                write_silence_frames(file, frame_bytes, gap_frames, sample_rate, &mut data_bytes)?;
                total_frames_written = total_frames_written.saturating_add(gap_frames);
            }
            let writable = &samples[overlap_frames as usize * CHANNELS..];
            if !writable.is_empty() {
                let bytes: Vec<u8> = writable.iter().flat_map(|sample| sample.to_le_bytes()).collect();
                file.write_all(&bytes).map_err(|error| error.to_string())?;
                data_bytes = data_bytes.saturating_add(bytes.len() as u32);
                total_frames_written = total_frames_written.saturating_add((writable.len() / CHANNELS) as u64);
            }
            next_position = following_position;
            Ok(())
        };

        while !stop.load(Ordering::SeqCst) {
            match packets.recv_timeout(Duration::from_millis(20)) {
                Ok(packet) => write_packet(&mut file, packet)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let final_elapsed = start_instant.elapsed().as_secs_f64();
        let _ = stream.stop_capture();
        while let Ok(packet) = packets.try_recv() {
            write_packet(&mut file, packet)?;
        }

        let final_expected = (final_elapsed * sample_rate as f64) as u64;
        if final_expected > total_frames_written {
            write_silence_frames(
                &mut file,
                frame_bytes,
                final_expected - total_frames_written,
                sample_rate,
                &mut data_bytes,
            )?;
        }
        finalize_wave_header(&mut file, data_bytes)?;
        if data_bytes == 0 {
            let _ = fs::remove_file(&path);
            Ok(None)
        } else {
            Ok(Some(path))
        }
    })();
    if let Some(sender) = ready_sender.take() {
        let error = result
            .as_ref()
            .err()
            .cloned()
            .unwrap_or_else(|| "System audio capture stopped before startup completed.".to_string());
        let _ = sender.send(Err(error));
    }
    result
}

/// Returns the packet's presentation time in host-clock seconds and its
/// samples interleaved as `channels`-channel float frames.
#[cfg(target_os = "macos")]
fn interleaved_audio_packet(
    sample: &screencapturekit::cm::CMSampleBuffer,
    channels: usize,
) -> Option<(f64, Vec<f32>)> {
    let pts = sample.presentation_timestamp().as_seconds()?;
    let list = sample.audio_buffer_list()?;
    let floats = |data: &[u8]| -> Vec<f32> {
        data.chunks_exact(4)
            .map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect()
    };
    let first = list.get(0)?;
    if list.num_buffers() == 1 && first.number_channels as usize == channels {
        return Some((pts, floats(first.data())));
    }
    // ScreenCaptureKit delivers non-interleaved float32: one buffer per channel.
    let planes: Vec<Vec<f32>> = (0..list.num_buffers())
        .filter_map(|index| list.get(index))
        .map(|buffer| floats(buffer.data()))
        .collect();
    let frames = planes.iter().map(Vec::len).min()?;
    let mut interleaved = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        for channel in 0..channels {
            let plane = &planes[channel.min(planes.len() - 1)];
            interleaved.push(plane[frame]);
        }
    }
    Some((pts, interleaved))
}

/// WAVEFORMATEX for IEEE float PCM.
#[cfg(target_os = "macos")]
fn float_wave_format(sample_rate: u32, channels: u16) -> Vec<u8> {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    let block_align = channels * 4;
    let mut format = Vec::with_capacity(18);
    format.extend_from_slice(&WAVE_FORMAT_IEEE_FLOAT.to_le_bytes());
    format.extend_from_slice(&channels.to_le_bytes());
    format.extend_from_slice(&sample_rate.to_le_bytes());
    format.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    format.extend_from_slice(&block_align.to_le_bytes());
    format.extend_from_slice(&32_u16.to_le_bytes());
    format.extend_from_slice(&0_u16.to_le_bytes());
    format
}

/// Seconds on the host clock that ScreenCaptureKit timestamps samples with.
#[cfg(target_os = "macos")]
fn host_time_seconds() -> f64 {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    let ticks = unsafe {
        mach_timebase_info(&mut info);
        mach_absolute_time()
    };
    if info.denom == 0 {
        return 0.0;
    }
    ticks as f64 * f64::from(info.numer) / f64::from(info.denom) / 1_000_000_000.0
}

#[cfg(target_os = "macos")]
fn screen_capture_permission_error(detail: &str) -> String {
    format!(
        "QuickClipper needs Screen & System Audio Recording permission. Allow it in System Settings > Privacy & Security > Screen & System Audio Recording, then reopen QuickClipper. ({detail})"
    )
}

fn write_silence_frames(
    file: &mut fs::File,
    frame_bytes: usize,
    frames: u64,
    sample_rate: u64,
    data_bytes: &mut u32,
) -> Result<(), String> {
    let chunk_frames = sample_rate.max(1);
    let silence = vec![0_u8; chunk_frames as usize * frame_bytes];
    let mut remaining = frames;
    while remaining > 0 {
        let frames_now = remaining.min(chunk_frames);
        let bytes_now = frames_now as usize * frame_bytes;
        file.write_all(&silence[..bytes_now])
            .map_err(|error| error.to_string())?;
        *data_bytes = data_bytes.saturating_add(bytes_now as u32);
        remaining -= frames_now;
    }
    Ok(())
}

fn capture_packet_alignment(
    next_device_position: Option<u64>,
    packet_device_position: u64,
    packet_frames: u32,
    timestamp_valid: bool,
) -> (u64, u64, Option<u64>) {
    let Some(next_device_position) = next_device_position else {
        let following_device_position = timestamp_valid
            .then(|| packet_device_position.saturating_add(packet_frames as u64));
        return (0, 0, following_device_position);
    };
    if !timestamp_valid {
        return (
            0,
            0,
            Some(next_device_position.saturating_add(packet_frames as u64)),
        );
    }

    let gap_frames = packet_device_position.saturating_sub(next_device_position);
    let overlap_frames = next_device_position
        .saturating_sub(packet_device_position)
        .min(packet_frames as u64);
    let following_device_position = next_device_position.max(
        packet_device_position.saturating_add(packet_frames as u64),
    );
    (gap_frames, overlap_frames, Some(following_device_position))
}

fn write_wave_header(
    file: &mut fs::File,
    format_bytes: &[u8],
    data_bytes: u32,
) -> Result<(), String> {
    let riff_bytes = 4u32
        .saturating_add(8)
        .saturating_add(format_bytes.len() as u32)
        .saturating_add(8)
        .saturating_add(data_bytes);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    file.write_all(b"RIFF").map_err(|error| error.to_string())?;
    file.write_all(&riff_bytes.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(b"WAVEfmt ")
        .map_err(|error| error.to_string())?;
    file.write_all(&(format_bytes.len() as u32).to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(format_bytes)
        .map_err(|error| error.to_string())?;
    file.write_all(b"data").map_err(|error| error.to_string())?;
    file.write_all(&data_bytes.to_le_bytes())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn finalize_wave_header(file: &mut fs::File, data_bytes: u32) -> Result<(), String> {
    file.flush().map_err(|error| error.to_string())?;
    let current = file.stream_position().map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(4))
        .map_err(|error| error.to_string())?;
    file.write_all(&(current.saturating_sub(8) as u32).to_le_bytes())
        .map_err(|error| error.to_string())?;
    let data_size_offset = current.saturating_sub(data_bytes as u64).saturating_sub(4);
    file.seek(SeekFrom::Start(data_size_offset))
        .map_err(|error| error.to_string())?;
    file.write_all(&data_bytes.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    Ok(())
}

fn mux_recording_audio(
    ffmpeg_path: &str,
    video_path: &Path,
    audio_path: &Path,
    audio_lead_seconds: f64,
) -> Result<(), String> {
    if fs::metadata(audio_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        <= 64
    {
        let _ = fs::remove_file(audio_path);
        return Ok(());
    }
    let muxed = video_path.with_file_name(format!("quickclipper-muxed-{}.mp4", timestamp()));
    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().to_string(),
    ];
    if audio_lead_seconds > 0.000_5 {
        args.extend(["-ss".to_string(), seconds(audio_lead_seconds)]);
    }
    args.extend([
        "-i".to_string(),
        audio_path.to_string_lossy().to_string(),
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "1:a:0".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        "320k".to_string(),
        "-af".to_string(),
        "apad".to_string(),
        "-shortest".to_string(),
        "-avoid_negative_ts".to_string(),
        "make_zero".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        muxed.to_string_lossy().to_string(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_command(ffmpeg_path, &refs, "Audio mux failed")?;
    fs::copy(&muxed, video_path).map_err(|error| error.to_string())?;
    let _ = fs::remove_file(muxed);
    let _ = fs::remove_file(audio_path);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn start_recording_overlay(
    _app: &tauri::AppHandle,
    parent_pid: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<RecordingOverlay, String> {
    let script = r#"
$ParentPid = __PARENT_PID__
$X = __X__
$Y = __Y__
$W = __W__
$H = __H__
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NativeWindowStyles {
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr hWnd, int nIndex);
  [DllImport("user32.dll")] public static extern int SetWindowLong(IntPtr hWnd, int nIndex, int dwNewLong);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, UInt32 uFlags);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowDisplayAffinity(IntPtr hWnd, UInt32 dwAffinity);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr dpiContext);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
'@
try { [NativeWindowStyles]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null } catch { try { [NativeWindowStyles]::SetProcessDPIAware() | Out-Null } catch {} }
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()
$script:parentProcess = $null
try {
  $script:parentProcess = [System.Diagnostics.Process]::GetProcessById($ParentPid)
  $null = $script:parentProcess.Handle
} catch {
  exit 0
}
$transparent = [System.Drawing.Color]::FromArgb(255, 255, 0, 255)
$record = [System.Drawing.Color]::FromArgb(229, 72, 77)
$window = New-Object System.Windows.Forms.Form
$window.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
$window.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$window.Bounds = New-Object System.Drawing.Rectangle -ArgumentList ($X - 3), ($Y - 36), ($W + 6), ($H + 42)
$window.TopMost = $true
$window.ShowInTaskbar = $false
$window.BackColor = $transparent
$window.TransparencyKey = $transparent
$timerText = New-Object System.Windows.Forms.Label
$timerText.Text = '00:00'
$timerText.AutoSize = $true
$timerText.Padding = New-Object System.Windows.Forms.Padding -ArgumentList 8, 4, 8, 4
$timerText.BackColor = [System.Drawing.Color]::FromArgb(204, 17, 17, 17)
$timerText.ForeColor = [System.Drawing.Color]::White
$timerText.Font = New-Object System.Drawing.Font -ArgumentList $timerText.Font, ([System.Drawing.FontStyle]::Bold)
$timerText.Location = New-Object System.Drawing.Point -ArgumentList 0, 0
$window.Controls.Add($timerText)
foreach ($panel in @(
  @{ X = 0; Y = 33; W = $W + 6; H = 3 },
  @{ X = 0; Y = 33; W = 3; H = $H + 6 },
  @{ X = $W + 3; Y = 33; W = 3; H = $H + 6 },
  @{ X = 0; Y = $H + 36; W = $W + 6; H = 3 }
)) {
  $edge = New-Object System.Windows.Forms.Panel
  $edge.BackColor = $record
  $edge.Bounds = New-Object System.Drawing.Rectangle -ArgumentList $panel.X, $panel.Y, $panel.W, $panel.H
  $window.Controls.Add($edge)
}
$script:HWND_TOPMOST = [IntPtr](-1)
$script:SWP_NOSIZE = 0x0001
$script:SWP_NOMOVE = 0x0002
$script:SWP_NOACTIVATE = 0x0010
$script:SWP_FRAMECHANGED = 0x0020
$script:SWP_SHOWWINDOW = 0x0040
$script:SWP_NOOWNERZORDER = 0x0200
$script:SW_SHOWNOACTIVATE = 4
$script:WDA_EXCLUDEFROMCAPTURE = [uint32]0x11
$script:overlayEnabled = $true
function Test-ParentAlive {
  if ($null -eq $script:parentProcess) { return $false }
  try { return -not $script:parentProcess.HasExited } catch { return $false }
}
function Set-OverlayTopMost {
  if ($script:overlayEnabled -and $window -and -not $window.IsDisposed -and $window.IsHandleCreated) {
    [NativeWindowStyles]::ShowWindow($window.Handle, $script:SW_SHOWNOACTIVATE) | Out-Null
    $flags = [uint32]($script:SWP_NOSIZE -bor $script:SWP_NOMOVE -bor $script:SWP_NOACTIVATE -bor $script:SWP_SHOWWINDOW -bor $script:SWP_NOOWNERZORDER)
    [NativeWindowStyles]::SetWindowPos($window.Handle, $script:HWND_TOPMOST, 0, 0, 0, 0, $flags) | Out-Null
  }
}
$started = [DateTime]::Now
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 100
$timer.Add_Tick({
  if (-not (Test-ParentAlive)) {
    $window.Close()
    return
  }
  $elapsed = [DateTime]::Now - $started
  if ($elapsed.TotalHours -ge 1) { $timerText.Text = $elapsed.ToString('hh\:mm\:ss') } else { $timerText.Text = $elapsed.ToString('mm\:ss') }
  Set-OverlayTopMost
})
$window.Add_Shown({
  $hwnd = $window.Handle
  $style = [NativeWindowStyles]::GetWindowLong($hwnd, -20)
  # WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE.
  # This keeps the overlay click-through, out of Alt-Tab, and unable to lose
  # focus on behalf of the application beneath it.
  [NativeWindowStyles]::SetWindowLong($hwnd, -20, $style -bor 0x20 -bor 0x80 -bor 0x80000 -bor 0x08000000) | Out-Null
  # The overlay is positioned over the capture region, so a desktop capture
  # would otherwise record its timer and border. Hide it if this Windows
  # version cannot exclude the window from screen capture.
  if (-not [NativeWindowStyles]::SetWindowDisplayAffinity($hwnd, $script:WDA_EXCLUDEFROMCAPTURE)) {
    $script:overlayEnabled = $false
    $window.Close()
    return
  }
  $frameFlags = [uint32]($script:SWP_NOSIZE -bor $script:SWP_NOMOVE -bor $script:SWP_NOACTIVATE -bor $script:SWP_FRAMECHANGED -bor $script:SWP_SHOWWINDOW -bor $script:SWP_NOOWNERZORDER)
  [NativeWindowStyles]::SetWindowPos($hwnd, $script:HWND_TOPMOST, 0, 0, 0, 0, $frameFlags) | Out-Null
  Set-OverlayTopMost
  $timer.Start()
})
$window.Add_Deactivate({ Set-OverlayTopMost })
$window.Add_VisibleChanged({ if ($window.Visible) { Set-OverlayTopMost } })
$window.Add_Closed({ $timer.Stop(); if ($script:parentProcess) { $script:parentProcess.Dispose() } })
[void]$window.ShowDialog()
"#
    .replace("__PARENT_PID__", &parent_pid.to_string())
    .replace("__X__", &x.to_string())
    .replace("__Y__", &y.to_string())
    .replace("__W__", &width.to_string())
    .replace("__H__", &height.to_string());
    hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .spawn()
        .map_err(|error| format!("Recording overlay failed: {}", error))
}

#[cfg(not(target_os = "macos"))]
fn stop_recording_overlay(child: &mut Option<RecordingOverlay>) {
    if let Some(mut overlay) = child.take() {
        let _ = overlay.kill();
        let _ = overlay.wait();
    }
}

#[cfg(target_os = "macos")]
const RECORDING_OVERLAY_LABEL: &str = "recording-overlay";

/// The timer and border sit just outside the region, in a click-through
/// window that is also excluded from screen capture.
#[cfg(target_os = "macos")]
fn start_recording_overlay(
    app: &tauri::AppHandle,
    _parent_pid: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<RecordingOverlay, String> {
    if let Some(previous) = app.get_webview_window(RECORDING_OVERLAY_LABEL) {
        let _ = previous.destroy();
    }
    let window = tauri::WebviewWindowBuilder::new(
        app,
        RECORDING_OVERLAY_LABEL,
        tauri::WebviewUrl::App("index.html?overlay=1".into()),
    )
    .title("QuickClipper Recording")
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible_on_all_workspaces(true)
    .content_protected(true)
    .focused(false)
    .inner_size(f64::from(width) + 6.0, f64::from(height) + 42.0)
    .position(f64::from(x) - 3.0, f64::from(y) - 36.0)
    .build()
    .map_err(|error| format!("Recording overlay failed: {error}"))?;
    let _ = window.set_ignore_cursor_events(true);
    raise_above_menu_bar(&window);
    Ok(window)
}

#[cfg(target_os = "macos")]
fn stop_recording_overlay(overlay: &mut Option<RecordingOverlay>) {
    if let Some(window) = overlay.take() {
        let _ = window.destroy();
    }
}

fn terminate_active_recording(recording: &mut ActiveRecording) {
    stop_recording_overlay(&mut recording.overlay_child);
    // AppState is dropped during application shutdown. Do not leave FFmpeg or
    // the loopback capture thread running after the UI process has exited.
    let _ = recording.child.kill();
    let _ = recording.child.wait();
    let _ = stop_loopback_recording(recording.audio.take());
    let _ = fs::remove_file(&recording.path);
}

fn run_command(exe: &str, args: &[&str], label: &str) -> Result<(), String> {
    let output = hidden_command(exe)
        .args(args)
        .output()
        .map_err(|error| format!("{}: {}", label, error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error_message(
            label,
            &String::from_utf8_lossy(&output.stderr),
        ))
    }
}

fn command_error_message(label: &str, stderr: &str) -> String {
    let lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let preferred = lines
        .iter()
        .rev()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("invalid")
                || lower.contains("error")
                || lower.contains("failed")
                || lower.contains("could not")
                || lower.contains("nothing was written")
                || lower.contains("conversion failed")
        })
        .or_else(|| {
            lines.iter().rev().find(|line| {
                !line.starts_with("Input #")
                    && !line.starts_with("Metadata:")
                    && !line.starts_with("Duration:")
                    && !line.starts_with("Stream #")
                    && !line.starts_with("Press [q]")
                    && !line.starts_with("frame=")
            })
        })
        .copied()
        .unwrap_or("process exited with an error");
    format!("{}: {}", label, preferred)
}

fn remove_empty_file(path: &Path) {
    if fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(1)
        == 0
    {
        let _ = fs::remove_file(path);
    }
}

fn clamp_export_crop(crop: &Crop, source_width: u32, source_height: u32) -> Crop {
    let x = even_floor(crop.x.min(source_width.saturating_sub(2)));
    let y = even_floor(crop.y.min(source_height.saturating_sub(2)));
    let max_width = source_width.saturating_sub(x).max(2);
    let max_height = source_height.saturating_sub(y).max(2);
    Crop {
        x,
        y,
        width: make_even(crop.width.min(max_width).max(2)),
        height: make_even(crop.height.min(max_height).max(2)),
    }
}

fn build_filters(
    request: &ExportRequest,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
) -> String {
    let mut filters = Vec::new();
    let cropped =
        crop.x > 0 || crop.y > 0 || crop.width != source_width || crop.height != source_height;
    let mut current_width = source_width;
    let mut current_height = source_height;
    if cropped && crop.width > 0 && crop.height > 0 {
        filters.push(format!(
            "crop={}:{}:{}:{}",
            make_even(crop.width),
            make_even(crop.height),
            crop.x,
            crop.y
        ));
        current_width = make_even(crop.width);
        current_height = make_even(crop.height);
    }

    if request.auto_fit720 {
        filters.push("crop=min(iw\\,ih*16/9):min(ih\\,iw*9/16):(iw-ow)/2:(ih-oh)/2".to_string());
        filters.push("scale=1280:720".to_string());
    } else if request.output_width > 0
        && request.output_height > 0
        && (make_even(request.output_width) != current_width
            || make_even(request.output_height) != current_height)
    {
        filters.push(format!(
            "scale={}:{}",
            make_even(request.output_width),
            make_even(request.output_height)
        ));
    }

    filters.join(",")
}

fn build_cut_filter(
    request: &ExportRequest,
    crop: &Crop,
    source_width: u32,
    source_height: u32,
    source_has_audio: bool,
) -> (String, bool) {
    let video_kept = kept_segments(request.start, request.end, &request.cuts);
    let audio_kept = if request.settings.include_audio && source_has_audio {
        kept_segments(request.audio_start, request.audio_end, &request.audio_cuts)
    } else {
        Vec::new()
    };
    let filters = build_filters(request, crop, source_width, source_height);
    let has_audio = !audio_kept.is_empty();
    let mut parts = Vec::new();
    for (index, (start, end)) in video_kept.iter().enumerate() {
        let mut video = format!(
            "[0:v]trim=start={}:end={},setpts=PTS-STARTPTS",
            seconds(*start),
            seconds(*end)
        );
        if !filters.is_empty() {
            video.push(',');
            video.push_str(&filters);
        }
        video.push_str(&format!("[v{}]", index));
        parts.push(video);
    }

    let video_inputs = (0..video_kept.len())
        .map(|index| format!("[v{}]", index))
        .collect::<Vec<_>>()
        .join("");
    parts.push(format!(
        "{}concat=n={}:v=1:a=0[outv]",
        video_inputs,
        video_kept.len()
    ));

    if has_audio {
        for (index, (start, end)) in audio_kept.iter().enumerate() {
            parts.push(format!(
                "[0:a]atrim=start={}:end={},asetpts=PTS-STARTPTS,volume={:.6}[a{}]",
                seconds(*start),
                seconds(*end),
                db_to_linear(request.settings.audio_gain_db),
                index
            ));
        }
        let audio_inputs = (0..audio_kept.len())
            .map(|index| format!("[a{}]", index))
            .collect::<Vec<_>>()
            .join("");
        parts.push(format!(
            "{}concat=n={}:v=0:a=1[outa]",
            audio_inputs,
            audio_kept.len()
        ));
    }
    (parts.join(";"), has_audio)
}

fn kept_segments(start: f64, end: f64, cuts: &[CutRange]) -> Vec<(f64, f64)> {
    let mut ranges = cuts.to_vec();
    ranges.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut cursor = start;
    let mut kept = Vec::new();
    for cut in ranges {
        let cut_start = cut.start.max(start);
        let cut_end = cut.end.min(end);
        if cut_end <= cut_start {
            continue;
        }
        if cut_start > cursor {
            kept.push((cursor, cut_start));
        }
        cursor = cursor.max(cut_end);
    }
    if cursor < end {
        kept.push((cursor, end));
    }
    kept
}

fn kept_duration(start: f64, end: f64, cuts: &[CutRange]) -> f64 {
    kept_segments(start, end, cuts)
        .iter()
        .map(|(segment_start, segment_end)| segment_end - segment_start)
        .sum()
}

fn ranges_equal(left: &[(f64, f64)], right: &[(f64, f64)]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(
            |((left_start, left_end), (right_start, right_end))| {
                (left_start - right_start).abs() < 0.001 && (left_end - right_end).abs() < 0.001
            },
        )
}

fn target_size_bytes(max_mb: f64) -> u64 {
    (max_mb.max(0.1) * 1024.0 * 1024.0).round() as u64
}

fn choose_audio_wav_profile(settings: &AppSettings, duration: f64) -> AudioWavProfile {
    const PROFILES: [AudioWavProfile; 5] = [
        AudioWavProfile {
            codec: "pcm_s24le",
            sample_rate: 48_000,
            channels: 2,
            kbps: 2304,
        },
        AudioWavProfile {
            codec: "pcm_s16le",
            sample_rate: 48_000,
            channels: 2,
            kbps: 1536,
        },
        AudioWavProfile {
            codec: "pcm_s16le",
            sample_rate: 44_100,
            channels: 2,
            kbps: 1412,
        },
        AudioWavProfile {
            codec: "pcm_s16le",
            sample_rate: 48_000,
            channels: 1,
            kbps: 768,
        },
        AudioWavProfile {
            codec: "pcm_s16le",
            sample_rate: 44_100,
            channels: 1,
            kbps: 706,
        },
    ];
    let first_profile = match effective_audio_quality(settings) {
        "best" => 0,
        "optimized" => 1,
        _ => 2,
    };
    if first_profile == 0 {
        return PROFILES[0];
    }
    if !settings.size_cap_enabled {
        return PROFILES[first_profile];
    }
    let target = target_size_bytes(settings.max_megabytes);
    PROFILES[first_profile..]
        .iter()
        .copied()
        .find(|profile| estimate_audio_wav_bytes(*profile, duration) <= target)
        .unwrap_or(PROFILES[PROFILES.len() - 1])
}

fn estimate_audio_wav_bytes(profile: AudioWavProfile, duration: f64) -> u64 {
    ((profile.kbps as f64 * 1000.0 / 8.0) * duration.max(0.0) + 4096.0).ceil() as u64
}

fn attempt_output_path(output_path: &Path, attempt: usize) -> PathBuf {
    let parent = output_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = output_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("clip");
    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mp4");
    parent.join(format!("{}.sizecap-{}.{}", stem, attempt + 1, extension))
}

fn cleanup_attempts(paths: &[PathBuf], keep: Option<&Path>) {
    for path in paths {
        if keep.is_some_and(|kept| kept == path) {
            continue;
        }
        let _ = fs::remove_file(path);
    }
    if let Some(path) = keep {
        let _ = fs::remove_file(path);
    }
}

fn format_bytes_raw(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    format!("{:.2} MB", mb)
}

fn calculate_video_bitrate(
    max_mb: f64,
    duration: f64,
    audio_kbps: u32,
    max_video_kbps: u32,
) -> u32 {
    let total_kbits = max_mb.max(0.1) * 8192.0 * 0.985;
    ((total_kbits / duration.max(0.5)) as i32 - audio_kbps as i32).clamp(250, max_video_kbps as i32)
        as u32
}

fn db_to_linear(db: f64) -> f64 {
    10_f64.powf(db.clamp(-24.0, 36.0) / 20.0)
}

fn encoder_args(key: &str) -> Vec<String> {
    match key {
        "x264-slow" => vec!["-c:v", "libx264", "-preset", "slow"],
        "x264-veryslow" => vec!["-c:v", "libx264", "-preset", "veryslow"],
        "x265-medium" => vec!["-c:v", "libx265", "-preset", "medium"],
        "x265-slow" => vec!["-c:v", "libx265", "-preset", "slow"],
        "h264-nvenc-fast" => vec!["-c:v", "h264_nvenc", "-preset", "p1"],
        "h264-nvenc" => vec!["-c:v", "h264_nvenc", "-preset", "p5"],
        "h264-nvenc-max" => vec!["-c:v", "h264_nvenc", "-preset", "p7"],
        "hevc-nvenc-fast" => vec!["-c:v", "hevc_nvenc", "-preset", "p1"],
        "hevc-nvenc" => vec!["-c:v", "hevc_nvenc", "-preset", "p5"],
        "hevc-nvenc-max" => vec!["-c:v", "hevc_nvenc", "-preset", "p7"],
        "h264-videotoolbox" => vec!["-c:v", "h264_videotoolbox"],
        // hvc1 tagging lets QuickTime and Safari play the file.
        "hevc-videotoolbox" => vec!["-c:v", "hevc_videotoolbox", "-tag:v", "hvc1"],
        _ => vec!["-c:v", "libx264", "-preset", "medium"],
    }
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn preserve_quality_args(key: &str) -> Vec<String> {
    let args = if key.contains("nvenc") {
        vec!["-rc", "vbr", "-cq", "18", "-b:v", "0"]
    } else if key.contains("videotoolbox") {
        vec!["-q:v", "70"]
    } else if key.contains("x265") {
        vec!["-crf", "20"]
    } else {
        vec!["-crf", "18"]
    };
    args.into_iter().map(str::to_string).collect()
}

fn encoder_presets() -> Vec<(&'static str, &'static str)> {
    let mut presets = vec![
        ("x264-medium", "H.264 Medium"),
        ("x264-slow", "H.264 Slow"),
        ("x264-veryslow", "H.264 Veryslow"),
        ("x265-medium", "H.265 Medium"),
        ("x265-slow", "H.265 Slow"),
    ];
    if cfg!(target_os = "macos") {
        presets.extend([
            ("h264-videotoolbox", "H.264 VideoToolbox"),
            ("hevc-videotoolbox", "H.265 VideoToolbox"),
        ]);
    } else {
        presets.extend([
            ("h264-nvenc-fast", "H.264 NVENC Fast"),
            ("h264-nvenc", "H.264 NVENC Quality"),
            ("h264-nvenc-max", "H.264 NVENC Max"),
            ("hevc-nvenc-fast", "H.265 NVENC Fast"),
            ("hevc-nvenc", "H.265 NVENC Quality"),
            ("hevc-nvenc-max", "H.265 NVENC Max"),
        ]);
    }
    presets
}

fn make_even(value: u32) -> u32 {
    if value % 2 == 0 {
        value
    } else {
        value.saturating_sub(1).max(2)
    }
}

fn even_floor(value: u32) -> u32 {
    value.saturating_sub(value % 2)
}

fn seconds(value: f64) -> String {
    format!("{:.3}", value.max(0.0))
}

fn millis(value: f64) -> u64 {
    (value.max(0.0) * 1000.0).round() as u64
}

fn timestamp() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

fn create_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let open = MenuItem::with_id(app, "open", "Open QuickClipper", true, None::<&str>)?;
    let record = MenuItem::with_id(app, "record", "Record / Stop", true, None::<&str>)?;
    let reset = MenuItem::with_id(app, "reset", "Reset Recording", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &record, &reset, &quit])?;

    let mut tray = TrayIconBuilder::with_id("quickclipper")
        .tooltip("QuickClipper")
        .menu(&menu);
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    tray.on_menu_event(|app, event| match event.id.as_ref() {
        "open" => show_main_window(app),
        "record" => {
            let _ = app.emit("tray-record-toggle", ());
        }
        "reset" => {
            let _ = app.emit("tray-record-reset", ());
        }
        "quit" => app.exit(0),
        _ => {}
    })
    .on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            show_main_window(tray.app_handle());
        }
    })
    .build(app)?;

    Ok(())
}

#[cfg(target_os = "macos")]
const HIDE_TO_MENU_BAR_ID: &str = "hide-to-menu-bar";

/// The menu bar stays hidden for a menu-bar app, but its key equivalents
/// still work. This is Tauri's default macOS menu, except that Cmd+Q puts
/// QuickClipper back in the menu bar so its hotkeys keep working; Quit in the
/// menu-bar icon's menu (and logging out) still exit.
#[cfg(target_os = "macos")]
fn background_app_menu(app: &tauri::App) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri::menu::{PredefinedMenuItem, Submenu};
    let close_app = MenuItem::with_id(
        app,
        HIDE_TO_MENU_BAR_ID,
        "Close QuickClipper",
        true,
        Some("CmdOrCtrl+Q"),
    )?;
    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                "QuickClipper",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &close_app,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::close_window(app, None)?,
                ],
            )?,
        ],
    )
}

fn focus_webview_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    window.show().map_err(|error| error.to_string())?;
    let _ = window.unminimize();
    let _ = window.set_always_on_top(true);
    thread::sleep(Duration::from_millis(80));
    let _ = window.set_always_on_top(false);
    window.set_focus().map_err(|error| error.to_string())
}

#[tauri::command]
fn focus_main_window(app: tauri::AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("Main window is not available.".to_string());
    };
    focus_webview_window(&window)
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = focus_webview_window(&window);
    }
}

fn should_start_hidden_in_tray() -> bool {
    if !std::env::args().any(|arg| arg == "--quickclipper-startup") {
        return false;
    }
    load_settings()
        .map(|settings| settings.start_hidden_in_tray)
        .unwrap_or(true)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[cfg(test)]
mod canvas_acceptance_tests;

#[cfg(test)]
mod export_capture_tests;

pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            recording: Mutex::new(None),
            exports: ExportJobs::default(),
            #[cfg(target_os = "macos")]
            region_selection: Mutex::new(None),
        })
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("QuickClipper")
                .args(["--quickclipper-startup"])
                .build(),
        )
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            create_tray(app)?;
            // On macOS QuickClipper is a menu-bar app: no Dock icon, and it
            // keeps running (with its hotkeys) when its window is closed.
            // Accessory apps are also the only ones macOS lets draw the
            // selector and overlay over another app's full-screen Space.
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                app.set_menu(background_app_menu(app)?)?;
                app.on_menu_event(|app, event| {
                    if event.id() == HIDE_TO_MENU_BAR_ID {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                });
                // Accessory apps are not brought forward at launch.
                if !should_start_hidden_in_tray() {
                    show_main_window(app.handle());
                }
            }
            if should_start_hidden_in_tray() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            open_video_dialog,
            prepare_preview_cache,
            prepare_playback_source,
            extract_exact_frame,
            choose_export_path,
            choose_save_folder,
            choose_ffmpeg_path,
            list_audio_devices,
            generate_waveform,
            open_region_selector,
            finish_region_selection,
            export_clip,
            begin_export,
            cancel_export,
            copy_video_frame,
            benchmark_encoders,
            start_recording,
            stop_recording,
            reset_recording,
            focus_main_window,
            copy_file_to_clipboard,
            reveal_path
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
            #[cfg(target_os = "macos")]
            if let WindowEvent::Destroyed = event {
                // A selector closed without a selection (for example with
                // Cmd+W) cancels the pending selection.
                if window.label().starts_with(REGION_SELECTOR_LABEL) {
                    deliver_region_selection(&window.state::<AppState>(), window.label(), None);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Clicking the Dock icon brings back a window hidden to the tray.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = event {
                show_main_window(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determine_audio_kbps_matches_profiles() {
        let mut settings = AppSettings::default();
        settings.size_cap_enabled = false;

        settings.audio_quality = "optimized".to_string();
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 320);

        settings.audio_quality = "best".to_string();
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 320);

        settings.audio_quality = "standard".to_string();
        settings.best_audio = true;
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 160);

        settings.audio_quality = "legacy-value".to_string();
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 320);

        settings.best_audio = false;
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 160);

        assert_eq!(determine_audio_kbps(&settings, 10.0, false), 0);

        // Size cap allocates 320kbps first for optimized if budget allows
        settings.size_cap_enabled = true;
        settings.max_megabytes = 10.0;
        settings.audio_quality = "optimized".to_string();
        assert_eq!(determine_audio_kbps(&settings, 10.0, true), 320);
    }

    #[test]
    fn app_settings_deserializes_missing_audio_quality() {
        let json = r#"{"saveFolder":"C:\\","ffmpegPath":"ffmpeg","frameRate":30,"maxMegabytes":10.0,"sizeCapEnabled":true,"qualityTargetKbps":10000,"exportEncoderKey":"x264-medium","exportBitrateScale":1.0,"audioGainDb":0.0,"unsupportedEncoderKeys":[],"encoderBenchmarks":[],"includeVideo":true,"includeAudio":true,"audioDeviceName":"","startWithWindows":true,"startHiddenInTray":true,"recordHotkey":"Super+Shift+R","resetHotkey":"Super+Shift+4","githubRepositoryUrl":""}"#;
        let settings: AppSettings = serde_json::from_str(json).expect("deserialize AppSettings");
        assert_eq!(settings.audio_quality, "best");
        assert!(settings.best_audio);
        assert_eq!(settings.capture_backend, CaptureBackend::DesktopDuplication);
    }

    #[test]
    fn desktop_duplication_uses_physical_display_coordinates() {
        let displays = [
            CaptureDisplay {
                output_idx: 0,
                left: 0,
                top: 0,
                right: 3440,
                bottom: 1440,
            },
            CaptureDisplay {
                output_idx: 1,
                left: 349,
                top: 1440,
                right: 2909,
                bottom: 2880,
            },
        ];
        assert_eq!(
            select_desktop_duplication_display(400, 1500, 1174, 1114, &displays).unwrap(),
            (1, 51, 60)
        );
        assert_eq!(
            select_desktop_duplication_display(100, 200, 640, 360, &displays).unwrap(),
            (0, 100, 200)
        );
        assert!(select_desktop_duplication_display(100, 1300, 640, 360, &displays).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an interactive Windows graphics device"]
    fn desktop_duplication_lists_live_outputs() {
        let displays = desktop_duplication_displays().unwrap();
        eprintln!("DXGI capture outputs: {displays:?}");
        assert!(!displays.is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "captures the interactive Windows desktop with FFmpeg"]
    fn desktop_duplication_records_live_region() {
        let display = desktop_duplication_displays().unwrap()[0];
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("capture.mp4");
        let mut settings = AppSettings::default();
        settings.ffmpeg_path = std::env::var("QUICKCLIPPER_TEST_FFMPEG")
            .unwrap_or_else(|_| "ffmpeg".to_string());
        let request = RecordingRequest {
            x: display.left + 20,
            y: display.top + 20,
            width: 320,
            height: 180,
            settings: settings.clone(),
        };
        let mut args = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into()];
        args.extend(recording_input_args(&request, 320, 180).unwrap());
        args.extend([
            "-t".into(),
            "1".into(),
            "-an".into(),
            "-c:v".into(),
            "libx264".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            video.to_string_lossy().to_string(),
        ]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_command(&settings.ffmpeg_path, &refs, "Desktop Duplication test failed").unwrap();
        let info = probe_video_file(&settings, &video, &video).unwrap().unwrap();
        assert_eq!((info.width, info.height), (320, 180));
        assert!(info.duration_seconds >= 0.9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn region_selection_ignores_windows_from_earlier_selections() {
        let state = AppState {
            recording: Mutex::new(None),
            exports: ExportJobs::default(),
            region_selection: Mutex::new(None),
        };
        let (sender, receiver) = mpsc::channel();
        *state.region_selection.lock().unwrap() = Some((2, sender));
        let region = || Some(RecordingRegion { x: 1, y: 2, width: 30, height: 40 });

        deliver_region_selection(&state, "region-selector-1-0", region());
        assert!(receiver.try_recv().is_err());
        assert!(state.region_selection.lock().unwrap().is_some());

        deliver_region_selection(&state, "region-selector-2-1", region());
        let delivered = receiver.try_recv().unwrap().unwrap();
        assert_eq!((delivered.x, delivered.y, delivered.width, delivered.height), (1, 2, 30, 40));
        assert!(state.region_selection.lock().unwrap().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn avfoundation_start_is_read_from_the_input_summary() {
        let stderr = "Input #0, avfoundation, from 'Capture screen 0:none':\n  Duration: N/A, start: 1146052.732084, bitrate: N/A\n  Stream #0:0: Video: rawvideo";
        assert_eq!(avfoundation_start_seconds(stderr), Some(1146052.732084));
        assert_eq!(avfoundation_start_seconds("Input #0, lavfi, from 'x':\n  Duration: N/A, start: 1.0"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "captures the interactive macOS desktop with FFmpeg"]
    fn avfoundation_records_live_region() {
        let screens = macos_capture_screens().unwrap();
        eprintln!(
            "capture screens: {:?}",
            screens
                .iter()
                .map(|screen| (screen.bounds, screen.scale, screen.pixel_width, screen.pixel_height))
                .collect::<Vec<_>>()
        );
        let screen = &screens[0];
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("capture.mp4");
        let mut settings = AppSettings::default();
        settings.ffmpeg_path = std::env::var("QUICKCLIPPER_TEST_FFMPEG")
            .unwrap_or_else(|_| "ffmpeg".to_string());
        let request = RecordingRequest {
            x: screen.bounds.left + 20,
            y: screen.bounds.top + 20,
            width: 320,
            height: 180,
            settings: settings.clone(),
        };
        let mut args = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into()];
        args.extend(recording_input_args(&request, 320, 180).unwrap());
        args.extend([
            "-t".into(),
            "1".into(),
            "-an".into(),
            "-c:v".into(),
            "libx264".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            video.to_string_lossy().to_string(),
        ]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_command(&settings.ffmpeg_path, &refs, "AVFoundation test failed").unwrap();
        let info = probe_video_file(&settings, &video, &video).unwrap().unwrap();
        let expected = (
            make_even((320.0 * screen.scale).round() as u32),
            make_even((180.0 * screen.scale).round() as u32),
        );
        assert_eq!((info.width, info.height), expected);
        assert!(info.duration_seconds >= 0.9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "records live system audio through ScreenCaptureKit"]
    fn screencapturekit_records_system_audio() {
        let recording = start_loopback_recording().unwrap();
        let mut sound = Command::new("afplay")
            .arg("/System/Library/Sounds/Glass.aiff")
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_secs(2));
        let _ = sound.wait();
        let path = stop_loopback_recording(Some(recording)).unwrap().unwrap();
        let output = Command::new(resolve_program("ffprobe"))
            .args(["-v", "error", "-show_entries", "stream=codec_name,sample_rate,channels:format=duration", "-of", "default=noprint_wrappers=1"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        eprintln!("{text}");
        let samples: Vec<f32> = fs::read(&path).unwrap()[58..]
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect();
        let peak = samples.iter().fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        let _ = fs::remove_file(&path);
        eprintln!("peak {peak}");
        assert!(text.contains("codec_name=pcm_f32le"));
        assert!(text.contains("sample_rate=48000"));
        let duration: f64 = text
            .lines()
            .find_map(|line| line.strip_prefix("duration="))
            .unwrap()
            .parse()
            .unwrap();
        assert!(duration >= 1.9, "duration {duration}");
        assert!(peak > 0.001, "captured audio was silent");
    }

    #[test]
    fn wav_profiles_match_the_quality_selection() {
        let mut settings = AppSettings::default();
        settings.size_cap_enabled = false;

        settings.audio_quality = "best".to_string();
        let best = choose_audio_wav_profile(&settings, 60.0);
        assert_eq!((best.codec, best.sample_rate, best.channels), ("pcm_s24le", 48_000, 2));

        settings.audio_quality = "optimized".to_string();
        let optimized = choose_audio_wav_profile(&settings, 60.0);
        assert_eq!(
            (optimized.codec, optimized.sample_rate, optimized.channels),
            ("pcm_s16le", 48_000, 2)
        );

        settings.audio_quality = "standard".to_string();
        let standard = choose_audio_wav_profile(&settings, 60.0);
        assert_eq!(
            (standard.codec, standard.sample_rate, standard.channels),
            ("pcm_s16le", 44_100, 2)
        );
    }

    #[test]
    fn capture_packet_alignment_preserves_device_timeline() {
        assert_eq!(
            capture_packet_alignment(Some(1_000), 1_100, 50, true),
            (100, 0, Some(1_150))
        );
        assert_eq!(
            capture_packet_alignment(Some(1_000), 950, 100, true),
            (0, 50, Some(1_050))
        );
        assert_eq!(
            capture_packet_alignment(Some(1_000), 0, 50, false),
            (0, 0, Some(1_050))
        );
        assert_eq!(
            capture_packet_alignment(None, 168_000, 480, true),
            (0, 0, Some(168_480))
        );
        assert_eq!(
            capture_packet_alignment(None, 0, 480, false),
            (0, 0, None)
        );
    }
}
