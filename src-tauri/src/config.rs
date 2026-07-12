use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct HotkeyConfig {
  pub vk_code: i32,
  pub ctrl: bool,
  pub shift: bool,
  pub alt: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioSourceKind {
  /// wasapi_output_capture -- audio de salida de un dispositivo (desktop,
  /// o un output virtual de Voicemeeter, etc).
  Output,
  /// wasapi_input_capture -- entrada (microfono, o un input virtual).
  Input,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AudioSourceConfig {
  pub kind: AudioSourceKind,
  pub device_id: String,
  pub label: String,
  /// Multiplicador lineal de volumen (1.0 = 100%, 0.0 = silencio total,
  /// hasta 2.0 = +100% de boost). Independiente de `muted`, igual que en
  /// el mixer de OBS.
  #[serde(default = "default_volume")]
  pub volume: f32,
  #[serde(default)]
  pub muted: bool,
}

fn default_volume() -> f32 {
  1.0
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
  /// image_source -- overlay estatico (logo, marco, etc). `content` = ruta
  /// de archivo.
  Image,
  /// text_ft2_source -- overlay de texto simple. `content` = el texto.
  Text,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct OverlayConfig {
  pub kind: OverlayKind,
  pub content: String,
  #[serde(default = "default_overlay_pos")]
  pub x: f32,
  #[serde(default = "default_overlay_pos")]
  pub y: f32,
  #[serde(default = "default_overlay_scale")]
  pub scale: f32,
  #[serde(default = "default_overlay_scale")]
  pub scale_x: f32,
  #[serde(default = "default_overlay_scale")]
  pub scale_y: f32,
  #[serde(default = "default_overlay_visible")]
  pub visible: bool,
  #[serde(default = "default_overlay_locked")]
  pub locked: bool,
}

impl OverlayConfig {
  pub fn get_scale_x(&self) -> f32 {
    if self.scale_x == 1.0 && self.scale != 1.0 { self.scale } else { self.scale_x }
  }
  pub fn get_scale_y(&self) -> f32 {
    if self.scale_y == 1.0 && self.scale != 1.0 { self.scale } else { self.scale_y }
  }
}

fn default_overlay_locked() -> bool {
  false
}

fn default_overlay_pos() -> f32 {
  100.0
}

fn default_overlay_scale() -> f32 {
  1.0
}

fn default_overlay_visible() -> bool {
  true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct EmberioConfig {
  #[serde(default = "default_clip_seconds")]
  pub clip_seconds: i64,
  pub clips_dir: Option<String>,
  pub hotkey: Option<HotkeyConfig>,
  #[serde(default = "default_video_source_type")]
  pub video_source_type: String,
  pub video_source_id: Option<String>,
  pub monitor_id: Option<String>,
  #[serde(default = "default_audio_sources")]
  pub audio_sources: Vec<AudioSourceConfig>,
  #[serde(default = "default_theme")]
  pub theme: String,
  /// "original" (misma resolucion que el monitor principal) o "720p".
  #[serde(default = "default_resolution")]
  pub resolution: String,
  #[serde(default = "default_fps")]
  pub fps: i64,
  #[serde(default)]
  pub overlays: Vec<OverlayConfig>,
}

fn default_clip_seconds() -> i64 {
  60
}

fn default_theme() -> String {
  "dark".to_string()
}

fn default_video_source_type() -> String {
  "screen".to_string()
}

fn default_resolution() -> String {
  "original".to_string()
}

fn default_fps() -> i64 {
  60
}

fn default_audio_sources() -> Vec<AudioSourceConfig> {
  vec![AudioSourceConfig {
    kind: AudioSourceKind::Output,
    device_id: "default".to_string(),
    label: "Audio de escritorio".to_string(),
    volume: default_volume(),
    muted: false,
  }]
}

impl Default for EmberioConfig {
  fn default() -> Self {
    Self {
      clip_seconds: default_clip_seconds(),
      clips_dir: None,
      hotkey: None,
      video_source_type: default_video_source_type(),
      video_source_id: None,
      monitor_id: None,
      audio_sources: default_audio_sources(),
      theme: default_theme(),
      resolution: default_resolution(),
      fps: default_fps(),
      overlays: Vec::new(),
    }
  }
}

impl Default for HotkeyConfig {
  fn default() -> Self {
    Self { vk_code: 0, ctrl: false, shift: false, alt: false }
  }
}

fn config_path(app: &AppHandle) -> PathBuf {
  let dir = app.path().app_config_dir().expect("no se pudo resolver el directorio de config de la app");
  std::fs::create_dir_all(&dir).ok();
  dir.join("config.json")
}

pub fn load(app: &AppHandle) -> EmberioConfig {
  let path = config_path(app);
  std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(app: &AppHandle, config: &EmberioConfig) {
  let path = config_path(app);
  if let Ok(json) = serde_json::to_string_pretty(config) {
    let _ = std::fs::write(path, json);
  }
}
