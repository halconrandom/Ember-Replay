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

#[derive(Serialize, Deserialize, Clone)]
pub struct EmberioConfig {
  #[serde(default = "default_clip_seconds")]
  pub clip_seconds: i64,
  pub clips_dir: Option<String>,
  pub hotkey: Option<HotkeyConfig>,
}

fn default_clip_seconds() -> i64 {
  60
}

impl Default for EmberioConfig {
  fn default() -> Self {
    Self { clip_seconds: default_clip_seconds(), clips_dir: None, hotkey: None }
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
  std::fs::read_to_string(&path)
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_else(|| EmberioConfig { clip_seconds: default_clip_seconds(), clips_dir: None, hotkey: None })
}

pub fn save(app: &AppHandle, config: &EmberioConfig) {
  let path = config_path(app);
  if let Ok(json) = serde_json::to_string_pretty(config) {
    let _ = std::fs::write(path, json);
  }
}
