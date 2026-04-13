/**
 * Video processing utilities.
 * project: Lap
 * author:  julyx10
 * date:    2024-08-08
 */
use ffmpeg_next as ffmpeg;
use image::{DynamicImage, ImageFormat, RgbImage};
use std::collections::HashMap;
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

/// Upper bound on demuxed packets while extracting one video thumbnail.
const MAX_THUMB_DECODE_PACKETS: usize = 24_000;

/// Prefix for errors emitted only when Rust panics inside FFmpeg wrappers.
const FFMPEG_PANIC_ERR_PREFIX: &str = "LAP_FFMPEG_PANIC:";

/// Cache version to invalidate all cached media if logic changes significantly.
const CACHE_VERSION: &str = "v1";

/// Video preparation errors
pub mod error {
    pub const DURATION_EXCEEDED: &str = "DurationExceeded";
    pub const FFMPEG_FAILED: &str = "FfmpegFailed";
    pub const FILE_NOT_FOUND: &str = "FileNotFound";
    pub const PROBE_ERROR: &str = "ProbeError";
}

#[derive(serde::Serialize)]
pub struct VideoPrepareResult {
    pub url: String,
    pub mime_type: String,
    pub action: String,
    pub duration_secs: f64,
}

#[derive(Debug, PartialEq, Clone, Copy, serde::Serialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum VideoAction {
    Direct,
    Remux,
    Transcode,
}

pub struct VideoManager {
    pub ffmpeg_path: Mutex<Option<String>>,
    pub active_processes: Arc<Mutex<HashMap<String, Child>>>,
}

impl Default for VideoManager {
    fn default() -> Self {
        Self {
            ffmpeg_path: Mutex::new(None),
            active_processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl VideoManager {
    pub fn init(&self, app: &AppHandle) {
        let mut path_guard = self.ffmpeg_path.lock().unwrap();
        if path_guard.is_some() {
            return;
        }

        // 1. Env var
        if let Ok(p) = std::env::var("LAP_FFMPEG_PATH") {
            if Path::new(&p).exists() {
                *path_guard = Some(p);
                return;
            }
        }

        // 2. Resource dir
        if let Ok(resource_dir) = app.path().resource_dir() {
            let p = resource_dir.join("ffmpeg");
            let p_exe = if cfg!(target_os = "windows") { p.with_extension("exe") } else { p };
            if p_exe.exists() {
                *path_guard = Some(p_exe.to_string_lossy().to_string());
                return;
            }
        }

        // 3. System which
        if let Ok(output) = Command::new(if cfg!(target_os = "windows") { "where" } else { "which" })
            .arg("ffmpeg")
            .output()
        {
            if output.status.success() {
                let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !p.is_empty() {
                    *path_guard = Some(p);
                    return;
                }
            }
        }

        panic!("ffmpeg binary not found in ENV, Resources, or System PATH");
    }

    pub fn get_ffmpeg_path(&self) -> Result<String, String> {
        self.ffmpeg_path.lock().unwrap().clone().ok_or_else(|| "FFmpeg path not initialized".to_string())
    }
}

// --------------------------------------------------------------------------
//  FFmpeg Wrapper & Basic Info extraction
// --------------------------------------------------------------------------

fn ffmpeg_catch_panic<T>(
    file_path: &str,
    context: &str,
    op: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let path = file_path.to_string();
    match panic::catch_unwind(AssertUnwindSafe(op)) {
        Ok(inner) => inner,
        Err(_) => {
            eprintln!("Panic caught during {} for video file: {}", context, path);
            Err(format!("{}{}: {}", FFMPEG_PANIC_ERR_PREFIX, context, path))
        }
    }
}

fn is_ffmpeg_panic_err(e: &str) -> bool {
    e.starts_with(FFMPEG_PANIC_ERR_PREFIX)
}

fn get_stream_rotation(stream: &ffmpeg::format::stream::Stream) -> i32 {
    if let Some(rot) = stream.metadata().get("rotate").and_then(|v| v.parse::<i32>().ok()) {
        if rot != 0 { return rot; }
    }
    unsafe {
        let raw_stream = stream.as_ptr();
        if raw_stream.is_null() || (*raw_stream).codecpar.is_null() { return 0; }
        let codecpar = (*raw_stream).codecpar;
        let n = (*codecpar).nb_coded_side_data;
        let side_data_ptr = (*codecpar).coded_side_data;
        if side_data_ptr.is_null() || n <= 0 { return 0; }
        for i in 0..n as isize {
            let entry = &*side_data_ptr.offset(i);
            if entry.type_ == ffmpeg::ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX
                && entry.size >= 36 && !entry.data.is_null()
            {
                let angle = ffmpeg::ffi::av_display_rotation_get(entry.data as *const i32);
                let rounded = (-angle).round() as i32;
                return ((rounded % 360) + 360) % 360;
            }
        }
    }
    0
}

pub fn get_video_dimensions(file_path: &str) -> Result<(u32, u32), String> {
    match ffmpeg_catch_panic(file_path, "reading dimensions", || {
        ffmpeg_next::init().map_err(|e| format!("ffmpeg init error: {:?}", e))?;
        let ictx = ffmpeg_next::format::input(&file_path).map_err(|e| format!("Failed to open file: {:?}", e))?;
        let input = ictx.streams().best(ffmpeg_next::media::Type::Video).ok_or("No video stream found")?;
        let rotation = get_stream_rotation(&input);
        let context = ffmpeg_next::codec::context::Context::from_parameters(input.parameters())
            .map_err(|e| format!("Failed to get codec context: {}", e))?;
        let video = context.decoder().video().map_err(|e| format!("Failed to get video decoder: {}", e))?;
        let (w, h) = (video.width(), video.height());
        if rotation == 90 || rotation == 270 { Ok((h, w)) } else { Ok((w, h)) }
    }) {
        Ok(dims) => Ok(dims),
        Err(e) if is_ffmpeg_panic_err(&e) => {
            eprintln!("Degrading video dimensions to 0×0 after FFmpeg panic: {}", e);
            Ok((0, 0))
        }
        Err(e) => Err(e),
    }
}

pub fn get_video_duration(file_path: &str) -> Result<f64, String> {
    // 1. Try library first
    let lib_res = ffmpeg_catch_panic(file_path, "reading duration", || {
        ffmpeg_next::init().map_err(|e| format!("ffmpeg init error: {:?}", e))?;
        let ictx = ffmpeg_next::format::input(file_path).map_err(|e| format!("Failed to open file: {e}"))?;
        let duration = ictx.duration();
        if duration > 0 {
             Ok(duration as f64 / ffmpeg_next::ffi::AV_TIME_BASE as f64)
        } else {
             Err("Duration is 0".to_string())
        }
    });

    if let Ok(d) = lib_res { return Ok(d); }

    // 2. Fallback to CLI probe if possible
    // Search for ffmpeg in common paths if we don't have it passed in
    let ffmpeg_exe = std::env::var("LAP_FFMPEG_PATH").ok()
        .or_else(|| {
            if cfg!(target_os = "macos") && Path::new("/opt/homebrew/bin/ffmpeg").exists() {
                Some("/opt/homebrew/bin/ffmpeg".to_string())
            } else {
                None
            }
        });

    if let Some(exe) = ffmpeg_exe {
        let mut cmd = Command::new(exe);
        cmd.arg("-i").arg(file_path).arg("-hide_banner");
        if let Ok(output) = cmd.output() {
            let info = String::from_utf8_lossy(&output.stderr);
            if let Some(pos) = info.find("Duration: ") {
                let s = &info[pos + 10..];
                if let Some(comma) = s.find(',') {
                    let time_str = &s[..comma];
                    let parts: Vec<&str> = time_str.split(':').collect();
                    if parts.len() == 3 {
                        let hours: u64 = parts[0].trim().parse().unwrap_or(0);
                        let mins: u64 = parts[1].trim().parse().unwrap_or(0);
                        let secs: f64 = parts[2].trim().parse().unwrap_or(0.0);
                        return Ok(hours as f64 * 3600.0 + mins as f64 * 60.0 + secs);
                    }
                }
            }
        }
    }

    Ok(0.0)
}

pub fn get_video_thumbnail(file_path: &str, thumbnail_size: u32) -> Result<Option<Vec<u8>>, String> {
    match ffmpeg_catch_panic(file_path, "thumbnail extraction", || {
        get_video_thumbnail_internal(file_path, thumbnail_size, true)
    }) {
        Ok(v) => Ok(v),
        Err(e) if is_ffmpeg_panic_err(&e) => {
            eprintln!("Skipping video thumbnail after FFmpeg panic: {}", e);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn get_video_thumbnail_internal(file_path: &str, thumbnail_size: u32, seek: bool) -> Result<Option<Vec<u8>>, String> {
    ffmpeg::init().map_err(|e| format!("{e}"))?;
    let mut ictx = ffmpeg::format::input(file_path).map_err(|e| format!("{e}"))?;
    let input = ictx.streams().best(ffmpeg::media::Type::Video).ok_or("No video")?;
    let stream_index = input.index();
    let rotation = get_stream_rotation(&input);
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())
        .map_err(|e| e.to_string())?
        .decoder()
        .video()
        .map_err(|e| e.to_string())?;
    if seek && ictx.duration() > 0 {
        if let Err(_) = ictx.seek(ictx.duration() / 10, ..) {
            return get_video_thumbnail_internal(file_path, thumbnail_size, false);
        }
    }
    let mut packet_count = 0;
    for (stream, packet) in ictx.packets() {
        packet_count += 1;
        if packet_count > MAX_THUMB_DECODE_PACKETS { return Ok(None); }
        if stream.index() != stream_index { continue; }
        decoder.send_packet(&packet).map_err(|e| e.to_string())?;
        let mut frame = ffmpeg::util::frame::Video::empty();
        if decoder.receive_frame(&mut frame).is_ok() {
            let (w, h) = (frame.width(), frame.height());
            let mut rgb_frame = ffmpeg::util::frame::Video::empty();
            let mut scaler = ffmpeg::software::scaling::context::Context::get(
                decoder.format(), w, h, ffmpeg::format::Pixel::RGB24, w, h, ffmpeg::software::scaling::flag::Flags::BILINEAR)
                .map_err(|e| e.to_string())?;
            scaler.run(&frame, &mut rgb_frame).map_err(|e| e.to_string())?;
            let stride = rgb_frame.stride(0);
            let row_bytes = w as usize * 3;
            if stride < row_bytes { return Ok(None); }
            let data = rgb_frame.data(0);
            let mut buf = Vec::with_capacity((w * h * 3) as usize);
            for y in 0..h as usize {
                let start = y * stride;
                buf.extend_from_slice(&data[start..start + row_bytes]);
            }
            let rgb_image = RgbImage::from_raw(w, h, buf).ok_or("buf error")?;
            let thumbnail = DynamicImage::ImageRgb8(rgb_image).thumbnail(u32::MAX, thumbnail_size);
            let rotated = match rotation {
                90 => thumbnail.rotate90(), 180 => thumbnail.rotate180(), 270 => thumbnail.rotate270(),
                -90 => thumbnail.rotate270(), -180 => thumbnail.rotate180(), -270 => thumbnail.rotate90(), _ => thumbnail,
            };
            let mut buffer = Cursor::new(Vec::new());
            rotated.write_to(&mut buffer, ImageFormat::Jpeg).map_err(|e| e.to_string())?;
            return Ok(Some(buffer.into_inner()));
        }
    }
    Ok(None)
}

// --------------------------------------------------------------------------
//  Metadata
// --------------------------------------------------------------------------

#[derive(Default, Debug)]
pub struct VideoMetadata {
    pub e_make: Option<String>,
    pub e_model: Option<String>,
    pub e_date_time: Option<String>,
    pub e_software: Option<String>,
    pub gps_latitude: Option<f64>,
    pub gps_longitude: Option<f64>,
    pub gps_altitude: Option<f64>,
}

pub fn get_video_metadata(file_path: &str) -> Result<VideoMetadata, String> {
    match ffmpeg_catch_panic(file_path, "reading metadata", || {
        ffmpeg::init().map_err(|e| e.to_string())?;
        let ictx = ffmpeg::format::input(&file_path).map_err(|e| e.to_string())?;
        let mut meta = HashMap::new();
        for (k, v) in ictx.metadata().iter() { meta.insert(k.to_lowercase(), v.to_string()); }
        if let Some(stream) = ictx.streams().best(ffmpeg::media::Type::Video) {
            for (k, v) in stream.metadata().iter() { meta.insert(k.to_lowercase(), v.to_string()); }
        }
        let mut m = VideoMetadata::default();
        let first = |keys: &[&str]| {
            for k in keys { if let Some(v) = meta.get(*k) { return Some(v.clone()); } }
            None
        };
        m.e_make = first(&["make", "camera_make", "com.apple.quicktime.make", "com.android.capture.camera.make"]);
        m.e_model = first(&["model", "camera_model", "com.apple.quicktime.model", "com.android.capture.camera.model"]);
        m.e_software = first(&["software", "com.apple.quicktime.software", "com.android.capture.camera.software", "encoder"]);
        m.e_date_time = first(&["com.apple.quicktime.creationdate", "com.android.capture.framedate", "creation_time", "media_time", "date", "datetimeoriginal"]);
        if let Some(loc) = first(&["location", "location-eng", "com.apple.quicktime.location.iso6709"]) {
            parse_apple_iso6709(&loc, &mut m);
        }
        if let Some(lat) = meta.get("gps_latitude") { m.gps_latitude = lat.parse().ok(); }
        if let Some(lon) = meta.get("gps_longitude") { m.gps_longitude = lon.parse().ok(); }
        if let Some(alt) = meta.get("gps_altitude") { m.gps_altitude = alt.parse().ok(); }
        if let Some(lat) = meta.get("com.dji.gpslatitude") { m.gps_latitude = lat.parse().ok(); }
        if let Some(lon) = meta.get("com.dji.gpslongitude") { m.gps_longitude = lon.parse().ok(); }
        Ok(m)
    }) {
        Ok(m) => Ok(m),
        Err(e) if is_ffmpeg_panic_err(&e) => {
            eprintln!("Degrading video metadata after panic: {}", e); Ok(VideoMetadata::default())
        }
        Err(e) => Err(e),
    }
}

fn parse_apple_iso6709(raw: &str, m: &mut VideoMetadata) {
    let s = raw.trim().trim_end_matches('/');
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut first = true;
    for ch in s.chars() {
        if (ch == '+' || ch == '-') && !first { parts.push(current.clone()); current.clear(); }
        current.push(ch); first = false;
    }
    if !current.is_empty() { parts.push(current); }
    if !parts.is_empty() { m.gps_latitude = parts[0].parse().ok(); }
    if parts.len() >= 2 { m.gps_longitude = parts[1].parse().ok(); }
    if parts.len() >= 3 { m.gps_altitude = parts[2].parse().ok(); }
}

// --------------------------------------------------------------------------
//  Video Preparation & Caching
// --------------------------------------------------------------------------

fn infer_direct_mime(path: &str) -> &'static str {
    if path.to_lowercase().ends_with(".webm") { "video/webm" } else { "video/mp4" }
}

fn is_cache_valid(p: &Path) -> bool {
    if !p.exists() { return false; }
    if let Ok(m) = p.metadata() {
        if m.len() == 0 { return false; }
    } else { return false; }
    // Optional: probe check (expensive, maybe skip if performance is key)
    match ffmpeg::format::input(&p) {
        Ok(ictx) => ictx.streams().best(ffmpeg::media::Type::Video).is_some(),
        Err(_) => false
    }
}

fn get_cache_filename(action: VideoAction, file_path: &str, ext: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    CACHE_VERSION.hash(&mut hasher);
    action.hash(&mut hasher);
    file_path.hash(&mut hasher);
    if let Ok(meta) = std::fs::metadata(file_path) {
        meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH).hash(&mut hasher);
        meta.len().hash(&mut hasher);
    }
    format!("{:x}.{}", hasher.finish(), ext)
}

fn touch_file(path: &Path) {
    let _ = std::fs::OpenOptions::new().write(true).open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()));
}

fn auto_cleanup_video_cache(cache_dir: &Path) {
    let max_size = 10 * 1024 * 1024 * 1024; // 10 GB
    let Ok(entries) = std::fs::read_dir(cache_dir) else { return; };
    let mut files = Vec::new();
    let mut total_size = 0;
    for entry in entries.flatten() {
        if let Ok(m) = entry.metadata() {
            if m.is_file() {
                total_size += m.len();
                files.push((entry.path(), m.len(), m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)));
            }
        }
    }
    if total_size <= max_size { return; }
    files.sort_by_key(|f| f.2); // Oldest first
    for (p, size, _) in files {
        if total_size <= 7 * 1024 * 1024 * 1024 { break; } // Keep 7GB
        if std::fs::remove_file(p).is_ok() { total_size -= size; }
    }
}

#[tauri::command]
pub async fn prepare_video(
    app: AppHandle,
    state: tauri::State<'_, VideoManager>,
    file_path: String,
    player_id: String,
    force: Option<String>,
) -> Result<VideoPrepareResult, String> {
    state.init(&app);
    let ffmpeg_path = state.get_ffmpeg_path()?;
    let path = Path::new(&file_path);
    if !path.exists() { return Err(error::FILE_NOT_FOUND.to_string()); }

    let action = match force.as_deref() {
        Some("remux") => VideoAction::Remux,
        Some("transcode") => VideoAction::Transcode,
        _ => VideoAction::Direct,
    };

    let duration = get_video_duration(&file_path).map_err(|_| error::PROBE_ERROR.to_string())?;
    println!("[Video] Detected duration: {}s", duration);
    println!("[Video] Initial Action: {:?}, Force: {:?}", action, force);

    // 1. Action isolation and duration check
    // Block any processing (Remux/Transcode) for videos > 10 mins to prevent UI hanging
    if action != VideoAction::Direct && duration > 600.0 {
        return Err(error::DURATION_EXCEEDED.to_string());
    }

    if action == VideoAction::Direct {
        println!("[Video] Using Direct path");
        return Ok(VideoPrepareResult {
            url: file_path.clone(),
            mime_type: infer_direct_mime(&file_path).to_string(),
            action: "direct".to_string(),
            duration_secs: duration,
        });
    }

    // 2. FFmpeg paths (Remux or Transcode)
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("video_cache");
    if !cache_dir.exists() { std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?; }

    let format_ext = if action == VideoAction::Remux {
        let is_vp9 = (|| -> Result<bool, String> {
            let mut probe = Command::new(&ffmpeg_path);
            probe.arg("-i").arg(&file_path).arg("-hide_banner");
            let output = probe.output().map_err(|e| e.to_string())?;
            let info = String::from_utf8_lossy(&output.stderr);
            Ok(info.contains("Video: vp9"))
        })().map_err(|_| error::PROBE_ERROR.to_string())?;
        if is_vp9 { "webm" } else { "mp4" }
    } else {
        "mp4"
    };

    let cache_file = cache_dir.join(get_cache_filename(action, &file_path, format_ext));
    if is_cache_valid(&cache_file) {
        touch_file(&cache_file);
        println!("[Video] Cache HIT: {:?}", cache_file);
        return Ok(VideoPrepareResult {
            url: cache_file.to_string_lossy().to_string(),
            mime_type: if format_ext == "webm" { "video/webm" } else { "video/mp4" }.to_string(),
            action: format!("{:?}", action).to_lowercase(),
            duration_secs: duration,
        });
    }
    println!("[Video] Cache MISS: {:?}", cache_file);

    // FFmpeg execution
    {
        let mut procs = state.active_processes.lock().unwrap();
        if let Some(mut child) = procs.remove(&player_id) { let _ = child.kill(); let _ = child.wait(); }
    }

    let part_file = cache_file.with_extension(format!("{}.part", format_ext));
    let mut cmd = Command::new(&ffmpeg_path);

    if action == VideoAction::Remux {
        cmd.arg("-i").arg(&file_path);
        cmd.arg("-c:v").arg("copy");
        if format_ext == "webm" {
            cmd.arg("-c:a").arg("libopus");
        } else {
            cmd.arg("-c:a").arg("aac").arg("-b:a").arg("192k");
        }
    } else {
        // Transcode
        cmd.arg("-fflags").arg("+genpts+igndts");
        cmd.arg("-max_interleave_delta").arg("0");
        cmd.arg("-i").arg(&file_path);
        cmd.arg("-map_metadata").arg("-1"); // Strip potentially corrupt legacy metadata
        cmd.arg("-c:v").arg("libx264").arg("-preset").arg("superfast").arg("-crf").arg("23")
           .arg("-profile:v").arg("baseline").arg("-level:v").arg("3.0")
           .arg("-vf").arg("scale=trunc(iw/2)*2:trunc(ih/2)*2")
           .arg("-pix_fmt").arg("yuv420p")
           .arg("-tag:v").arg("avc1")
           .arg("-c:a").arg("aac").arg("-b:a").arg("128k").arg("-ar").arg("44100");
    }

    cmd.arg("-map").arg("0:v:0").arg("-map").arg("0:a:0?");
    cmd.arg("-movflags").arg("faststart").arg("-f").arg(format_ext).arg("-y").arg(&part_file);

    println!("[Video] Running FFmpeg: {:?}", cmd);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let child = cmd.spawn().map_err(|e| format!("Spawn failed: {e}"))?;
    {
        let mut procs = state.active_processes.lock().unwrap();
        procs.insert(player_id.clone(), child);
    }

    // Wait loop
    let success = loop {
        {
            let mut procs = state.active_processes.lock().unwrap();
            if let Some(child) = procs.get_mut(&player_id) {
                if let Ok(Some(status)) = child.try_wait() {
                    procs.remove(&player_id);
                    break status.success();
                }
            } else {
                let _ = std::fs::remove_file(&part_file);
                return Err("Cancelled".to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    if success {
        // sync_all
        let f = std::fs::File::open(&part_file).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&part_file, &cache_file).map_err(|e| e.to_string())?;
        auto_cleanup_video_cache(&cache_dir);
        Ok(VideoPrepareResult {
            url: cache_file.to_string_lossy().to_string(),
            mime_type: if format_ext == "webm" { "video/webm" } else { "video/mp4" }.to_string(),
            action: format!("{:?}", action).to_lowercase(),
            duration_secs: duration,
        })
    } else {
        let _ = std::fs::remove_file(&part_file);
        Err(error::FFMPEG_FAILED.to_string())
    }
}

#[tauri::command]
pub async fn cancel_video_prepare(state: tauri::State<'_, VideoManager>, player_id: String) -> Result<(), String> {
    let mut procs = state.active_processes.lock().unwrap();
    if let Some(mut child) = procs.remove(&player_id) {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(())
}

#[tauri::command]
pub async fn clear_video_cache(app: AppHandle) -> Result<u64, String> {
    let mut freed: u64 = 0;
    if let Ok(cache_dir) = app.path().app_cache_dir().map(|d| d.join("video_cache")) {
        if cache_dir.exists() {
            for entry in std::fs::read_dir(&cache_dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if let Ok(meta) = entry.metadata() {
                    freed = freed.saturating_add(meta.len());
                }
            }
            std::fs::remove_dir_all(&cache_dir).map_err(|e| e.to_string())?;
        }
    }
    Ok(freed)
}
