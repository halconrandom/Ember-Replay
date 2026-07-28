mod config;
mod obs_ffi;
mod preview;

use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// Wrapper para poder guardar punteros crudos de libobs en un `static`.
/// Seguro en la practica: libobs es thread-safe para estas operaciones y
/// nosotros solo los tocamos desde comandos de Tauri (nunca en paralelo
/// real sobre el mismo puntero).
struct RawPtr<T>(*mut T);
unsafe impl<T> Send for RawPtr<T> {}
unsafe impl<T> Sync for RawPtr<T> {}

struct CaptureState {
  scene: RawPtr<obs_ffi::obs_scene_t>,
  capture_source: RawPtr<obs_ffi::obs_source_t>,
  /// Scene item de la fuente de captura principal. Guardado directo para no
  /// tener que buscarlo por nombre (los nombres se auto-renombran si se
  /// duplican y la busqueda empezaba a devolver el item equivocado).
  capture_item: RawPtr<obs_ffi::obs_sceneitem_t>,
  /// Una fuente de audio por cada entrada en config.audio_sources, en el
  /// mismo orden -- ocupan los canales de salida 1..=N.
  audio_sources: Vec<RawPtr<obs_ffi::obs_source_t>>,
  /// Un volmeter por fuente de audio (mismo orden/indice) -- alimenta
  /// AUDIO_LEVELS para los medidores en vivo del mixer.
  audio_volmeters: Vec<RawPtr<obs_ffi::obs_volmeter_t>>,
  /// Overlays (imagen/texto) encima de la captura, mismo orden/indice que
  /// config.overlays.
  overlays: Vec<OverlayItem>,
}

struct OverlayItem {
  source: RawPtr<obs_ffi::obs_source_t>,
  item: RawPtr<obs_ffi::obs_sceneitem_t>,
  /// Filtro "color_filter" (Color Correction) agregado a la fuente para
  /// controlar opacidad, igual que en OBS Studio.
  opacity_filter: RawPtr<obs_ffi::obs_source_t>,
}

struct OutputState {
  output: RawPtr<obs_ffi::obs_output_t>,
  video_encoder: RawPtr<obs_ffi::obs_encoder_t>,
  audio_encoder: RawPtr<obs_ffi::obs_encoder_t>,
  max_time_sec: i64,
}

static CAPTURE_STATE: Mutex<Option<CaptureState>> = Mutex::new(None);
static OUTPUT_STATE: Mutex<Option<OutputState>> = Mutex::new(None);
/// Solo `Some` mientras el preview esta activo (toggable a proposito -- ver
/// preview.rs). None = no hay ningun `obs_display` vivo, cero costo extra.
static PREVIEW_STATE: Mutex<Option<preview::PreviewState>> = Mutex::new(None);
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn emit_event<S: serde::Serialize + Clone>(event: &str, payload: S) {
  if let Some(app) = APP_HANDLE.get() {
    let _ = app.emit(event, payload);
  }
}
static CONFIG: Mutex<Option<config::EmberioConfig>> = Mutex::new(None);
static CONFIG_SAVE_TX: OnceLock<std::sync::mpsc::Sender<config::EmberioConfig>> = OnceLock::new();
/// Nivel de audio en vivo (0.0..=1.0) por indice de fuente, actualizado por
/// los callbacks de volmeter y emitido periodicamente al frontend.
static AUDIO_LEVELS: Mutex<Vec<f32>> = Mutex::new(Vec::new());
static OBS_INIT_LOCK: Mutex<()> = Mutex::new(());
/// (resolucion, fps) que ya estan aplicados en libobs. obs_reset_video /
/// obs_reset_audio son un teardown pesado de todo el pipeline -- solo hay
/// que correrlos cuando la config realmente cambio, no en cada arranque
/// del replay buffer.
static LAST_APPLIED_VIDEO: Mutex<Option<(String, i64)>> = Mutex::new(None);


const DEFAULT_HOTKEY_VK_F9: i32 = 0x78; // VK_F9
const DEFAULT_TOGGLE_HOTKEY_VK_F10: i32 = 0x79; // VK_F10

/// Codificadores de video candidatos para grabar, en orden de preferencia
/// cuando la config esta en "auto": hardware por vendor primero, x264 por
/// software al final como red de seguridad universal.
const VIDEO_ENCODER_CANDIDATES: &[(&str, &str)] = &[
  ("obs_nvenc_h264_tex", "NVIDIA NVENC"),
  ("h264_texture_amf", "AMD AMF"),
  ("obs_qsv11_v2", "Intel QuickSync"),
  ("obs_x264", "Software (x264)"),
];

#[derive(Clone, Copy)]
struct HotkeyBinding {
  vk_code: i32,
  ctrl: bool,
  shift: bool,
  alt: bool,
}

/// Slot 0 = hotkey de guardar clip (id 1), slot 1 = hotkey de encendido/
/// apagado (id 2).
static HOTKEYS: Mutex<[Option<HotkeyBinding>; 2]> = Mutex::new([None, None]);

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12; // Alt

unsafe fn key_is_down(vk: i32) -> bool {
  (GetAsyncKeyState(vk) as u16 & 0x8000) != 0
}

unsafe fn combo_is_down(b: &HotkeyBinding) -> bool {
  key_is_down(b.vk_code)
    && key_is_down(VK_CONTROL) == b.ctrl
    && key_is_down(VK_SHIFT) == b.shift
    && key_is_down(VK_MENU) == b.alt
}

/// Hotkeys globales al estilo libobs: en vez de RegisterHotKey (que depende
/// de que la cola de mensajes de una ventana reciba WM_HOTKEY -- muchos
/// juegos en pantalla completa exclusiva, o que capturan input crudo via
/// DirectInput/RawInput, simplemente nunca lo entregan), sondeamos el
/// estado fisico del teclado a nivel de OS con GetAsyncKeyState cada 30ms.
/// Es la misma tecnica que usa libobs (obs-hotkey.c: obs_hotkey_thread +
/// GetAsyncKeyState en obs-windows.c) y funciona sin importar que ventana
/// tenga el foco -- incluida una ventana exclusiva de juego.
fn init_native_hotkey_thread() {
  std::thread::spawn(|| {
    let mut was_down = [false; 2];
    loop {
      std::thread::sleep(std::time::Duration::from_millis(30));
      let bindings = { *HOTKEYS.lock().unwrap() };
      for (i, binding) in bindings.iter().enumerate() {
        let down = match binding {
          Some(b) => unsafe { combo_is_down(b) },
          None => false,
        };
        // Disparo solo en el flanco de bajada (recien presionado), igual
        // que MOD_NOREPEAT antes -- si no, dispararia en cada tick de 30ms
        // mientras se mantenga apretada la tecla.
        if down && !was_down[i] {
          if i == 0 {
            tauri::async_runtime::spawn(save_clip_and_notify());
          } else {
            tauri::async_runtime::spawn(toggle_recording_and_notify());
          }
        }
        was_down[i] = down;
      }
    }
  });
}

fn register_native_hotkey(id: i32, vk_code: i32, ctrl: bool, shift: bool, alt: bool) {
  let idx = (id - 1) as usize;
  if idx >= 2 {
    return;
  }
  HOTKEYS.lock().unwrap()[idx] = Some(HotkeyBinding { vk_code, ctrl, shift, alt });
}

#[link(name = "user32")]
extern "system" {
  fn GetSystemMetrics(n_index: i32) -> i32;
}
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

#[link(name = "winmm")]
extern "system" {
  pub fn PlaySoundW(
    psz_sound: *const u16,
    hmod: HWND,
    fdw_sound: u32,
  ) -> i32;
}

fn play_notification_sound() {
  let base = app_dir();
  let paths_to_try = vec![
    base.join("notify.wav"),
    base.join("../../../notify.wav"),
  ];

  for path in paths_to_try {
    if path.exists() {
      let path_wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
      unsafe {
        PlaySoundW(path_wide.as_ptr(), std::ptr::null_mut(), 0x00020003); // SND_FILENAME | SND_ASYNC | SND_NODEFAULT
      }
      break;
    }
  }
}

/// Convierte un buffer UTF-16 fijo de la API de Windows (terminado en NUL)
/// a String.
fn utf16_trimmed(buf: &[u16]) -> String {
  let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
  String::from_utf16_lossy(&buf[..len])
}

/// Mapea el device path DXGI que guarda OBS como monitor_id (ej.
/// "\\?\DISPLAY#GSM5CDE#5&...#{...}") al nombre GDI del adaptador
/// ("\\.\DISPLAY1"), que es el mismo nombre que reporta
/// tauri::Monitor::name(). Sin este mapeo nunca hay match y el indicador
/// terminaba en el monitor equivocado.
fn monitor_gdi_name_for_device_path(device_path: &str) -> Option<String> {
  use windows_sys::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
  const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;
  unsafe {
    for i in 0..16u32 {
      let mut adapter: DISPLAY_DEVICEW = std::mem::zeroed();
      adapter.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
      if EnumDisplayDevicesW(std::ptr::null(), i, &mut adapter, 0) == 0 {
        break;
      }
      let mut mon: DISPLAY_DEVICEW = std::mem::zeroed();
      mon.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
      if EnumDisplayDevicesW(adapter.DeviceName.as_ptr(), 0, &mut mon, EDD_GET_DEVICE_INTERFACE_NAME) == 0 {
        continue;
      }
      if utf16_trimmed(&mon.DeviceID).eq_ignore_ascii_case(device_path) {
        return Some(utf16_trimmed(&adapter.DeviceName));
      }
    }
  }
  None
}

fn get_target_monitor(app: &AppHandle) -> Option<tauri::Monitor> {
  let source_type = with_config(|c| c.video_source_type.clone());
  let source_id = with_config(|c| c.video_source_id.clone());

  let mut target_monitor = None;

  if source_type == "screen" {
    if let Some(id) = source_id {
      if let Ok(monitors) = app.available_monitors() {
        if let Some(gdi_name) = monitor_gdi_name_for_device_path(&id) {
          target_monitor = monitors.iter().find(|m| m.name() == Some(&gdi_name)).cloned();
        }
        if target_monitor.is_none() {
          if let Ok(idx) = id.parse::<usize>() {
            target_monitor = monitors.get(idx).cloned();
          } else {
            target_monitor = monitors.into_iter().find(|m| m.name() == Some(&id));
          }
        }
      }
    }
  }

  if target_monitor.is_none() {
    let main_win = app.get_webview_window("main");
    target_monitor = main_win
      .as_ref()
      .and_then(|w| w.current_monitor().ok().flatten());
  }

  if target_monitor.is_none() {
    target_monitor = app.primary_monitor().ok().flatten();
  }

  target_monitor
}

fn calculate_corner_position(
  monitor: &tauri::Monitor,
  width: f64,
  height: f64,
  offset_x: f64,
  offset_y: f64,
) -> (tauri::PhysicalPosition<i32>, tauri::LogicalPosition<f64>) {
  let pos = monitor.position();
  let size = monitor.size();
  let scale = monitor.scale_factor();

  let width_phys = (width * scale).round() as i32;
  let height_phys = (height * scale).round() as i32;
  let offset_x_phys = (offset_x * scale).round() as i32;
  let offset_y_phys = (offset_y * scale).round() as i32;

  let target_x_phys = pos.x + (size.width as i32) - width_phys - offset_x_phys;
  let target_y_phys = pos.y + (size.height as i32) - height_phys - offset_y_phys;

  let phys_pos = tauri::PhysicalPosition::new(target_x_phys, target_y_phys);
  let logical_pos = tauri::LogicalPosition::new(
    (target_x_phys as f64) / scale,
    (target_y_phys as f64) / scale,
  );

  (phys_pos, logical_pos)
}

fn create_native_overlay_window(
  app: &AppHandle,
  window_id: &str,
  route: &str,
  width: f64,
  height: f64,
  offset_x: f64,
  offset_y: f64,
) {
  let target_monitor = get_target_monitor(app);

  let (phys_pos, logical_pos) = match target_monitor {
    Some(ref m) => calculate_corner_position(m, width, height, offset_x, offset_y),
    None => {
      let (screen_w, screen_h) = primary_monitor_size();
      let tx = (screen_w as f64) - width - offset_x;
      let ty = (screen_h as f64) - height - offset_y;
      (
        tauri::PhysicalPosition::new(tx as i32, ty as i32),
        tauri::LogicalPosition::new(tx, ty),
      )
    }
  };

  let builder = tauri::WebviewWindowBuilder::new(
    app,
    window_id,
    tauri::WebviewUrl::App(route.into())
  )
  .title("Ember Overlay")
  .inner_size(width, height)
  // Ver show_status_notification: sin min_inner_size Windows clampea al
  // minimo de tracking (~136px) y la ventana desborda el borde del monitor.
  .min_inner_size(16.0, 16.0)
  .position(logical_pos.x, logical_pos.y)
  .resizable(false)
  .decorations(false)
  .transparent(true)
  .always_on_top(true)
  .skip_taskbar(true)
  .shadow(false);

  if let Ok(win) = builder.build() {
    let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize::new(width, height)));
    let _ = win.set_position(tauri::Position::Physical(phys_pos));
    unsafe {
      if let Ok(hwnd) = win.hwnd() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
          GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, WS_EX_TRANSPARENT, WS_EX_LAYERED
        };
        let raw_hwnd = hwnd.0 as windows_sys::Win32::Foundation::HWND;
        let ex_style = GetWindowLongW(raw_hwnd, GWL_EXSTYLE);
        let new_style = (ex_style as u32 | WS_EX_TRANSPARENT | WS_EX_LAYERED) as i32;
        SetWindowLongW(raw_hwnd, GWL_EXSTYLE, new_style);
      }
    }
  }
}

fn show_toast_notification(app: &AppHandle, clip_time: i64) {
  play_notification_sound();

  let route = format!("toast?time={clip_time}");
  if let Some(win) = app.get_webview_window("clip_toast") {
    let _ = app.emit("show-toast", clip_time);
    if let Some(m) = get_target_monitor(app) {
      let (phys_pos, _) = calculate_corner_position(&m, 280.0, 70.0, 20.0, 60.0);
      let _ = win.set_position(tauri::Position::Physical(phys_pos));
    }
    return;
  }

  create_native_overlay_window(app, "clip_toast", &route, 280.0, 70.0, 20.0, 60.0);
}

fn show_status_notification(app: &AppHandle, active: bool) {
  let route = format!("status_toast?active={active}");

  let target_monitor = get_target_monitor(app);
  let (phys_pos, logical_pos) = match target_monitor {
    Some(ref m) => calculate_corner_position(m, 36.0, 36.0, 15.0, 55.0),
    None => {
      let (screen_w, screen_h) = primary_monitor_size();
      let tx = (screen_w as f64) - 36.0 - 15.0;
      let ty = (screen_h as f64) - 36.0 - 55.0;
      (
        tauri::PhysicalPosition::new(tx as i32, ty as i32),
        tauri::LogicalPosition::new(tx, ty),
      )
    }
  };

  if let Some(win) = app.get_webview_window("status_toast") {
    let _ = app.emit("show-status", active);
    // Fallback directo por si el evento no llega (webview suspendido o
    // permisos de eventos): setea el color inyectando JS.
    let _ = win.eval(&format!("window.__setStatus && window.__setStatus({active})"));
    let _ = win.set_position(tauri::Position::Physical(phys_pos));
    let _ = win.show();
    log::info!(
      "status_toast: active={active}, monitor={:?}, pos=({}, {})",
      target_monitor.as_ref().and_then(|m| m.name().cloned()),
      phys_pos.x,
      phys_pos.y
    );
    return;
  }

  let builder = tauri::WebviewWindowBuilder::new(
    app,
    "status_toast",
    tauri::WebviewUrl::App(route.into())
  )
  .title("Ember Status")
  .inner_size(36.0, 36.0)
  // Sin esto Windows clampea la ventana al minimo de tracking del sistema
  // (~136px de ancho): el calculo de esquina asume 36px y la ventana
  // desbordaba el borde del monitor, cayendo en la pantalla de al lado.
  .min_inner_size(16.0, 16.0)
  .position(logical_pos.x, logical_pos.y)
  .resizable(false)
  .decorations(false)
  .transparent(true)
  .always_on_top(true)
  .skip_taskbar(true)
  .shadow(false);

  match builder.build() {
    Ok(win) => {
      let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize::new(36.0, 36.0)));
      let _ = win.set_position(tauri::Position::Physical(phys_pos));
      unsafe {
        if let Ok(hwnd) = win.hwnd() {
          use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, WS_EX_TRANSPARENT, WS_EX_LAYERED
          };
          let raw_hwnd = hwnd.0 as windows_sys::Win32::Foundation::HWND;
          let ex_style = GetWindowLongW(raw_hwnd, GWL_EXSTYLE);
          let new_style = (ex_style as u32 | WS_EX_TRANSPARENT | WS_EX_LAYERED) as i32;
          SetWindowLongW(raw_hwnd, GWL_EXSTYLE, new_style);
        }
      }
    }
    Err(e) => {
      log::error!("No se pudo crear la ventana status_toast: {e}");
    }
  }
}

/// Resolucion del monitor principal, usada como canvas base de libobs.
/// No depende de que los modulos ya esten cargados (a diferencia de
/// list_monitors), asi que la podemos usar antes de obs_load_all_modules.
fn primary_monitor_size() -> (u32, u32) {
  unsafe {
    let w = GetSystemMetrics(SM_CXSCREEN);
    let h = GetSystemMetrics(SM_CYSCREEN);
    if w > 0 && h > 0 {
      (w as u32, h as u32)
    } else {
      (1920, 1080)
    }
  }
}

fn with_config<R>(f: impl FnOnce(&mut config::EmberioConfig) -> R) -> R {
  let mut guard = CONFIG.lock().unwrap();
  if guard.is_none() {
    *guard = Some(config::EmberioConfig::default());
  }
  f(guard.as_mut().unwrap())
}

fn persist_config() {
  if let Some(tx) = CONFIG_SAVE_TX.get() {
    let cfg = CONFIG.lock().unwrap().clone().unwrap_or_default();
    let _ = tx.send(cfg);
  }
}

fn app_dir() -> PathBuf {
  std::env::current_exe()
    .expect("no se pudo resolver el ejecutable actual")
    .parent()
    .expect("el ejecutable no tiene directorio padre")
    .to_path_buf()
}

// ---------------------------------------------------------------------------
// Helpers de proc_handler / calldata
// ---------------------------------------------------------------------------

/// Llama a un proc sin argumentos (ej. "save") sobre el proc_handler de un
/// output. `calldata_t` es un struct simple (stack/size/capacity/fixed) --
/// lo zero-inicializamos a mano en vez de usar `calldata_init` porque esa
/// es una funcion `static inline` de header y bindgen no genera simbolo
/// linkeable para esas.
unsafe fn call_proc_no_args(output: *mut obs_ffi::obs_output_t, name: &str) -> bool {
  let ph = obs_ffi::obs_output_get_proc_handler(output);
  if ph.is_null() {
    return false;
  }
  let mut cd: obs_ffi::calldata_t = std::mem::zeroed();
  let name_c = CString::new(name).unwrap();
  let ok = obs_ffi::proc_handler_call(ph, name_c.as_ptr(), &mut cd as *mut _);
  if !cd.stack.is_null() {
    obs_ffi::bfree(cd.stack as *mut _);
  }
  ok
}

/// Idem pero devuelve el string de salida (ej. "path" de "get_last_replay").
/// Devuelve None si el proc no puso nada (replay buffer todavia muxeando el
/// clip a disco).
unsafe fn call_proc_get_string(output: *mut obs_ffi::obs_output_t, name: &str, out_field: &str) -> Option<String> {
  let ph = obs_ffi::obs_output_get_proc_handler(output);
  if ph.is_null() {
    return None;
  }
  let mut cd: obs_ffi::calldata_t = std::mem::zeroed();
  let name_c = CString::new(name).unwrap();
  obs_ffi::proc_handler_call(ph, name_c.as_ptr(), &mut cd as *mut _);

  let field_c = CString::new(out_field).unwrap();
  let mut str_ptr: *const std::os::raw::c_char = std::ptr::null();
  let has_value = obs_ffi::calldata_get_string(&cd as *const _, field_c.as_ptr(), &mut str_ptr as *mut _);

  let result = if has_value && !str_ptr.is_null() {
    let s = CStr::from_ptr(str_ptr).to_string_lossy().into_owned();
    if s.is_empty() { None } else { Some(s) }
  } else {
    None
  };

  if !cd.stack.is_null() {
    obs_ffi::bfree(cd.stack as *mut _);
  }
  result
}

// ---------------------------------------------------------------------------
// Introspeccion de propiedades de fuentes (para poblar los dropdowns de
// monitor/dispositivo de audio -- el mismo mecanismo que usa la UI real de
// OBS: cada fuente expone una lista con las opciones disponibles en ese
// momento via su callback get_properties, sin necesidad de una instancia).
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct PropertyOption {
  name: String,
  value: String,
}

unsafe fn list_property_options(source_id: &str, property_key: &str) -> Vec<PropertyOption> {
  let id_c = CString::new(source_id).unwrap();
  let settings = obs_ffi::obs_data_create();
  let source = obs_ffi::obs_source_create_private(id_c.as_ptr(), std::ptr::null(), settings);
  obs_ffi::obs_data_release(settings);
  if source.is_null() {
    return vec![];
  }

  let props = obs_ffi::obs_source_properties(source);
  if props.is_null() {
    obs_ffi::obs_source_release(source);
    return vec![];
  }

  let key_c = CString::new(property_key).unwrap();
  let prop = obs_ffi::obs_properties_get(props, key_c.as_ptr());

  let mut result = Vec::new();
  if !prop.is_null() {
    let count = obs_ffi::obs_property_list_item_count(prop);
    for i in 0..count {
      let name_ptr = obs_ffi::obs_property_list_item_name(prop, i);
      let value_ptr = obs_ffi::obs_property_list_item_string(prop, i);
      let name = if name_ptr.is_null() {
        String::new()
      } else {
        CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
      };
      let value = if value_ptr.is_null() {
        String::new()
      } else {
        CStr::from_ptr(value_ptr).to_string_lossy().into_owned()
      };
      result.push(PropertyOption { name, value });
    }
  }

  obs_ffi::obs_properties_destroy(props);
  obs_ffi::obs_source_release(source);
  result
}

// ---------------------------------------------------------------------------
// Inicializacion (idempotente)
// ---------------------------------------------------------------------------

/// Aplica (o reaplica) la config de video actual. Se puede llamar de nuevo
/// despues de stop_recording (ahi obs_video_active() vuelve a false) para
/// que un cambio de resolucion/FPS tome efecto en la proxima grabacion, sin
/// tener que reiniciar libobs entero.
unsafe fn apply_video_settings() -> Result<(), String> {
  let (resolution, fps) = with_config(|c| (c.resolution.clone(), c.fps));
  let (base_w, base_h) = primary_monitor_size();

  let (out_w, out_h) = if resolution == "720p" && base_h > 720 {
    let scale = 720.0 / base_h as f64;
    (((base_w as f64 * scale) as u32) & !1, 720u32)
  } else {
    (base_w, base_h)
  };

  let scale_type = if (out_w, out_h) == (base_w, base_h) {
    obs_ffi::obs_scale_type_OBS_SCALE_DISABLE
  } else {
    obs_ffi::obs_scale_type_OBS_SCALE_BICUBIC
  };

  let graphics_module = CString::new("libobs-d3d11").unwrap();
  let mut ovi = obs_ffi::obs_video_info {
    graphics_module: graphics_module.as_ptr(),
    fps_num: fps as u32,
    fps_den: 1,
    base_width: base_w,
    base_height: base_h,
    output_width: out_w,
    output_height: out_h,
    output_format: obs_ffi::video_format_VIDEO_FORMAT_NV12,
    adapter: 0,
    gpu_conversion: true,
    colorspace: obs_ffi::video_colorspace_VIDEO_CS_DEFAULT,
    range: obs_ffi::video_range_type_VIDEO_RANGE_DEFAULT,
    scale_type,
  };
  let video_result = obs_ffi::obs_reset_video(&mut ovi as *mut _);
  if video_result != obs_ffi::OBS_VIDEO_SUCCESS as i32 {
    return Err(format!("obs_reset_video fallo, codigo {video_result}"));
  }
  preview::set_canvas_size(base_w, base_h);

  let oai = obs_ffi::obs_audio_info {
    samples_per_sec: 48000,
    speakers: obs_ffi::speaker_layout_SPEAKERS_STEREO,
  };
  if !obs_ffi::obs_reset_audio(&oai as *const _) {
    return Err("obs_reset_audio fallo".into());
  }

  *LAST_APPLIED_VIDEO.lock().unwrap() = Some((resolution, fps));

  Ok(())
}

/// Reaplica resolucion/FPS solo si cambiaron desde la ultima aplicacion y el
/// video no esta activo. Antes se llamaba a apply_video_settings() en cada
/// arranque del replay buffer aunque nada hubiera cambiado; ese
/// obs_reset_video/obs_reset_audio gratuito despues de un stop+start era la
/// ventana del crash STATUS_ACCESS_VIOLATION al re-crear los encoders.
unsafe fn reapply_video_settings_if_changed() -> Result<(), String> {
  let current = with_config(|c| (c.resolution.clone(), c.fps));
  if LAST_APPLIED_VIDEO.lock().unwrap().as_ref() == Some(&current) {
    return Ok(());
  }
  if obs_ffi::obs_video_active() {
    // No se puede resetear con video activo; tomara efecto en el proximo
    // arranque en frio.
    return Ok(());
  }
  apply_video_settings()
}

/// Arranca libobs y carga los plugins. Corre una sola vez por proceso (a
/// diferencia de apply_video_settings, que se puede repetir).
unsafe fn ensure_obs_platform_initialized() -> Result<(), String> {
  let _guard = OBS_INIT_LOCK.lock().unwrap();
  if obs_ffi::obs_initialized() {
    return Ok(());
  }

  let base = app_dir();

  // libobs busca sus archivos core (shaders/effects) con una ruta
  // hardcodeada "../../data/libobs/" relativa al CWD del proceso, no al
  // ejecutable (ver vendor/obs-studio/libobs/obs-windows.c,
  // find_libobs_data_file) -- no hay API publica para overridear esto.
  // "data/" en si vive JUNTO al .exe (build.rs la copia ahi, y es tambien
  // donde el instalador NSIS deja los recursos empaquetados: en Windows
  // `resource_dir()` de Tauri == el directorio del ejecutable). Para que
  // la cuenta de "subir 2 niveles" de libobs siga dando con "data/" sin
  // tener que parchear su C, fijamos el CWD en un subdirectorio ficticio
  // 2 niveles por debajo de `base` (creado on-demand, nunca usado para
  // nada mas) en vez de `base` directamente.
  let cwd_anchor = base.join(".obs-cwd").join("anchor");
  std::fs::create_dir_all(&cwd_anchor)
    .map_err(|e| format!("no se pudo crear el directorio de trabajo para libobs: {e}"))?;
  std::env::set_current_dir(&cwd_anchor).map_err(|e| format!("no se pudo fijar el directorio de trabajo: {e}"))?;

  let locale = CString::new("en-US").unwrap();
  let started = obs_ffi::obs_startup(locale.as_ptr(), std::ptr::null(), std::ptr::null_mut());
  if !started {
    return Err("obs_startup fallo".into());
  }

  // Importante: reset_video/reset_audio tienen que correr ANTES de cargar
  // los modulos. win-capture decide en su obs_module_load() si registra la
  // fuente de monitor moderna (DXGI, "monitor_id" string) o la vieja (GDI,
  // "monitor" int) mirando si ya hay un dispositivo D3D11 activo
  // (gs_get_device_type()) -- si todavia no hay contexto de graficos, cae
  // siempre al modo legacy GDI aunque la GPU soporte DXGI perfectamente.
  apply_video_settings()?;

  // No hace falta un obs_add_module_path() manual: obs_startup ya llamo a
  // add_default_module_paths() (obs.c), que registra "../../obs-plugins/64bit"
  // y "../../data/obs-plugins/%module%" relativos al CWD -- con el
  // cwd_anchor de arriba eso resuelve exactamente a base/obs-plugins/64bit y
  // base/data/obs-plugins/%module%, que es donde build.rs ya los copio.
  // Registrar el mismo path a mano duplicaba la carga de cada plugin
  // ("obs_register_source: ... already exists! Duplicate library?").
  obs_ffi::obs_load_all_modules();
  obs_ffi::obs_post_load_modules();

  Ok(())
}

/// `vec2` es un union (con un miembro `x`/`y` anonimo y un alias `ptr:
/// [f32;2]`) del lado de C -- construimos siempre via el miembro `ptr` para
/// no depender del nombre exacto que bindgen le da al campo anonimo.
unsafe fn vec2_of(x: f32, y: f32) -> obs_ffi::vec2 {
  let mut v: obs_ffi::vec2 = std::mem::zeroed();
  v.__bindgen_anon_1.ptr = [x, y];
  v
}

/// Crea un `obs_data_t*` con un solo campo string (o null si `value` es None,
/// para dejar que la fuente use su propio default).
unsafe fn single_string_settings(key: &str, value: &Option<String>) -> *mut obs_ffi::obs_data_t {
  match value {
    Some(v) => {
      let settings = obs_ffi::obs_data_create();
      let key_c = CString::new(key).unwrap();
      let val_c = CString::new(v.as_str()).unwrap();
      obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
      settings
    }
    None => std::ptr::null_mut(),
  }
}

/// Maximo de fuentes de audio simultaneas (canales de salida 1..=5; el canal
/// 0 es el video). Alcanza y sobra para escenarios tipo Voicemeeter con
/// varios buses virtuales.
const MAX_AUDIO_SOURCES: usize = 5;

/// Convierte el dB que reporta el volmeter de OBS a un 0.0..=1.0 para la UI
/// (rango estandar de VU meter: -60dB = silencio, 0dB = full scale).
extern "C" fn volmeter_callback(
  param: *mut c_void,
  _magnitude: *const f32,
  peak: *const f32,
  _input_peak: *const f32,
) {
  let index = param as usize;
  if peak.is_null() {
    return;
  }
  let db = unsafe { *peak };
  let normalized = if db.is_finite() { ((db + 60.0) / 60.0).clamp(0.0, 1.0) } else { 0.0 };
  if let Ok(mut levels) = AUDIO_LEVELS.lock() {
    if index < levels.len() {
      levels[index] = normalized;
    }
  }
}

unsafe fn ensure_capture_started() -> Result<(), String> {
  if CAPTURE_STATE.lock().unwrap().is_some() {
    return Ok(());
  }

  let (source_type, mut source_id_cfg) = with_config(|c| (c.video_source_type.clone(), c.video_source_id.clone()));
  let audio_sources_cfg = with_config(|c| c.audio_sources.clone());

  let scene_name = CString::new("Ember Scene").unwrap();
  let scene = obs_ffi::obs_scene_create(scene_name.as_ptr());
  if scene.is_null() {
    return Err("obs_scene_create devolvio null".into());
  }
  let scene_source = obs_ffi::obs_scene_get_source(scene);

  let source_plugin = match source_type.as_str() {
    "window" => "window_capture",
    "game" => "game_capture",
    _ => "monitor_capture",
  };

  if source_id_cfg.is_none() {
    let options = list_property_options(source_plugin, if source_plugin == "monitor_capture" { "monitor_id" } else { "window" });
    if let Some(first) = options.first() {
      source_id_cfg = Some(first.value.clone());
      with_config(|c| c.video_source_id = Some(first.value.clone()));
      persist_config();
    }
  }

  let plugin_c = CString::new(source_plugin).unwrap();
  let name_c = CString::new("Captura Principal").unwrap();
  let settings = obs_ffi::obs_data_create();
  if let Some(ref val) = source_id_cfg {
    let val_c = CString::new(val.as_str()).unwrap();
    if source_plugin == "monitor_capture" {
      let key_c = CString::new("monitor_id").unwrap();
      obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
    } else {
      let key_c = CString::new("window").unwrap();
      obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
      if source_plugin == "window_capture" {
        let method_key = CString::new("method").unwrap();
        obs_ffi::obs_data_set_int(settings, method_key.as_ptr(), 2);
      } else if source_plugin == "game_capture" {
        let mode_key = CString::new("capture_mode").unwrap();
        let mode_val = CString::new("window").unwrap();
        obs_ffi::obs_data_set_string(settings, mode_key.as_ptr(), mode_val.as_ptr());
      }
    }
  }

  let capture_source = obs_ffi::obs_source_create(plugin_c.as_ptr(), name_c.as_ptr(), settings, std::ptr::null_mut());
  obs_ffi::obs_data_release(settings);

  if capture_source.is_null() {
    obs_ffi::obs_scene_release(scene);
    return Err(format!("obs_source_create('{source_plugin}') devolvio null"));
  }
  let capture_item = obs_ffi::obs_scene_add(scene, capture_source);

  // 4. Agregar fuentes de audio (wasapi desktop y microfonos)
  let mut audio_sources = Vec::new();
  let mut audio_volmeters: Vec<*mut obs_ffi::obs_volmeter_t> = Vec::new();
  for (idx, entry) in audio_sources_cfg.iter().enumerate() {
    let source_id = match entry.kind {
      config::AudioSourceKind::Output => "wasapi_output_capture",
      config::AudioSourceKind::Input => "wasapi_input_capture",
    };
    let source_id_c = CString::new(source_id).unwrap();
    let source_name_c = CString::new(format!("Audio Source {idx}")).unwrap();
    let settings = single_string_settings("device_id", &Some(entry.device_id.clone()));
    let source = obs_ffi::obs_source_create(
      source_id_c.as_ptr(),
      source_name_c.as_ptr(),
      settings,
      std::ptr::null_mut(),
    );
    if !settings.is_null() {
      obs_ffi::obs_data_release(settings);
    }
    if source.is_null() {
      for v in &audio_volmeters {
        if !v.is_null() {
          obs_ffi::obs_volmeter_detach_source(*v);
          obs_ffi::obs_volmeter_destroy(*v);
        }
      }
      for s in &audio_sources {
        obs_ffi::obs_source_release(*s);
      }
      obs_ffi::obs_source_release(capture_source);
      obs_ffi::obs_scene_release(scene);
      return Err(format!("obs_source_create('{source_id}') devolvio null para '{}'", entry.label));
    }
    audio_sources.push(source);

    // Configurar volumen inicial
    obs_ffi::obs_source_set_volume(source, entry.volume);
    obs_ffi::obs_source_set_muted(source, entry.muted);

    // Asignar al slot de salida del mixer OBS (1 para desktop, 2 para mic, etc.)
    let track_num = (idx + 1) as u32;
    if (track_num as usize) <= MAX_AUDIO_SOURCES {
      obs_ffi::obs_set_output_source(track_num, source);
    }

    // Crear medidor de nivel (volmeter)
    let volmeter = obs_ffi::obs_volmeter_create(obs_ffi::obs_fader_type_OBS_FADER_LOG);
    if !volmeter.is_null() {
      if obs_ffi::obs_volmeter_attach_source(volmeter, source) {
        obs_ffi::obs_volmeter_add_callback(volmeter, Some(volmeter_callback), idx as *mut std::ffi::c_void);
        audio_volmeters.push(volmeter);
      } else {
        obs_ffi::obs_volmeter_destroy(volmeter);
        audio_volmeters.push(std::ptr::null_mut());
      }
    } else {
      audio_volmeters.push(std::ptr::null_mut());
    }
  }

  *AUDIO_LEVELS.lock().unwrap() = vec![0.0; audio_sources.len()];

  // 6. Configurar salida de video del preview al output global de libobs
  obs_ffi::obs_set_output_source(0, scene_source);

  // 7. Cargar overlays persistentes en config
  let overlays_cfg = with_config(|c| c.overlays.clone());
  let mut overlays = Vec::new();
  for cfg in overlays_cfg {
    match create_overlay_item(scene, &cfg) {
      Ok(item) => overlays.push(item),
      Err(err) => {
        log::error!("No se pudo cargar overlay persistido: {}", err);
      }
    }
  }

  let mut capture_state = CaptureState {
    scene: RawPtr(scene),
    capture_source: RawPtr(capture_source),
    capture_item: RawPtr(capture_item),
    audio_sources: audio_sources.into_iter().map(RawPtr).collect(),
    audio_volmeters: audio_volmeters.into_iter().map(RawPtr).collect(),
    overlays,
  };
  sync_scene_z_order(&mut capture_state);

  *CAPTURE_STATE.lock().unwrap() = Some(capture_state);

  Ok(())
}

/// Crea la fuente (imagen o texto) + el item de escena para un overlay, y le
/// aplica su transform/visibilidad. Se usa tanto al recrear overlays
/// persistidos como al agregar uno nuevo en caliente.
unsafe fn create_overlay_item(scene: *mut obs_ffi::obs_scene_t, cfg: &config::OverlayConfig) -> Result<OverlayItem, String> {
  let (source_id, settings_key) = match cfg.kind {
    config::OverlayKind::Image => ("image_source", "file"),
    config::OverlayKind::Text => ("text_ft2_source", "text"),
    config::OverlayKind::Browser => ("browser_source", "url"),
  };
  let id_c = CString::new(source_id).unwrap();
  let name_c = CString::new(format!("Overlay ({source_id})")).unwrap();
  
  let settings = if source_id == "browser_source" {
    let s = obs_ffi::obs_data_create();
    let url_key = CString::new("url").unwrap();
    let url_val = CString::new(cfg.content.as_str()).unwrap();
    obs_ffi::obs_data_set_string(s, url_key.as_ptr(), url_val.as_ptr());
    
    let w_key = CString::new("width").unwrap();
    let h_key = CString::new("height").unwrap();
    obs_ffi::obs_data_set_int(s, w_key.as_ptr(), 800);
    obs_ffi::obs_data_set_int(s, h_key.as_ptr(), 600);
    s
  } else {
    single_string_settings(settings_key, &Some(cfg.content.clone()))
  };

  let source = obs_ffi::obs_source_create(id_c.as_ptr(), name_c.as_ptr(), settings, std::ptr::null_mut());
  if !settings.is_null() {
    obs_ffi::obs_data_release(settings);
  }
  if source.is_null() {
    return Err(format!("obs_source_create('{source_id}') devolvio null"));
  }

  let item = obs_ffi::obs_scene_add(scene, source);
  if item.is_null() {
    obs_ffi::obs_source_release(source);
    return Err("obs_scene_add devolvio null para el overlay".into());
  }

  obs_ffi::obs_sceneitem_set_pos(item, &vec2_of(cfg.x, cfg.y) as *const _);
  obs_ffi::obs_sceneitem_set_scale(item, &vec2_of(cfg.get_scale_x(), cfg.get_scale_y()) as *const _);
  obs_ffi::obs_sceneitem_set_visible(item, cfg.visible);
  obs_ffi::obs_sceneitem_set_locked(item, cfg.locked);

  // Filtro de opacidad (Color Correction, como OBS Studio). Privado: no
  // aparece en listados globales de fuentes.
  //
  // OJO: hay que pedir "color_filter_v2" explicitamente. El id pelado
  // "color_filter" resuelve a la v1, que lee "opacity" como ENTERO 0-100 --
  // nuestro 1.0 (double) se convertia en 1% de opacidad y los overlays
  // quedaban practicamente invisibles. La v2 usa double 0.0..=1.0.
  let filter_id = CString::new("color_filter_v2").unwrap();
  let filter_name = CString::new("Ember Opacidad").unwrap();
  let filter_settings = obs_ffi::obs_data_create();
  let opacity_key = CString::new("opacity").unwrap();
  obs_ffi::obs_data_set_double(filter_settings, opacity_key.as_ptr(), cfg.opacity.clamp(0.0, 1.0) as f64);
  let opacity_filter =
    obs_ffi::obs_source_create_private(filter_id.as_ptr(), filter_name.as_ptr(), filter_settings);
  obs_ffi::obs_data_release(filter_settings);
  if !opacity_filter.is_null() {
    obs_ffi::obs_source_filter_add(source, opacity_filter);
  }

  Ok(OverlayItem { source: RawPtr(source), item: RawPtr(item), opacity_filter: RawPtr(opacity_filter) })
}

unsafe fn sync_scene_z_order(state: &mut CaptureState) {
  let mut items_raw: Vec<*mut obs_ffi::obs_sceneitem_t> = Vec::new();

  // La captura principal siempre al fondo (Z-index 0).
  if !state.capture_item.0.is_null() {
    items_raw.push(state.capture_item.0);
  }

  // Append overlays in reverse order of the list (index 0 is front-most, index len-1 is back-most)
  for overlay in state.overlays.iter().rev() {
    items_raw.push(overlay.item.0);
  }

  obs_ffi::obs_scene_reorder_items(state.scene.0, items_raw.as_ptr() as *const _, items_raw.len());
}

// Disciplina de locks para todo lo que llama el wnd_proc del preview (thread
// de UI): copiar los punteros/datos necesarios BAJO el lock, soltarlo, y
// recien despues llamar a libobs. Nunca mantener CAPTURE_STATE/CONFIG tomados
// mientras se llama a una funcion de libobs.

pub(crate) unsafe fn try_select_overlay(canvas_x: f32, canvas_y: f32) -> Option<usize> {
  // Snapshot de (source_ptr, x, y, scale_x, scale_y, seleccionable) por overlay.
  let snapshot: Vec<(*mut obs_ffi::obs_source_t, f32, f32, f32, f32, bool)> = {
    let guard = CAPTURE_STATE.lock().unwrap();
    let state = guard.as_ref()?;
    let config_guard = CONFIG.lock().unwrap();
    let config = config_guard.as_ref()?;
    state
      .overlays
      .iter()
      .enumerate()
      .map(|(i, overlay)| {
        let cfg = config.overlays.get(i);
        let selectable = cfg.map(|c| !c.locked && c.visible).unwrap_or(false);
        let (x, y, sx, sy) = cfg
          .map(|c| (c.x, c.y, c.get_scale_x(), c.get_scale_y()))
          .unwrap_or((0.0, 0.0, 1.0, 1.0));
        (overlay.source.0, x, y, sx, sy, selectable)
      })
      .collect()
  };

  for (index, (source, x, y, sx, sy, selectable)) in snapshot.into_iter().enumerate() {
    if !selectable {
      continue;
    }
    let w = obs_ffi::obs_source_get_width(source) as f32;
    let h = obs_ffi::obs_source_get_height(source) as f32;

    if canvas_x >= x && canvas_x <= x + w * sx && canvas_y >= y && canvas_y <= y + h * sy {
      return Some(index);
    }
  }

  None
}

pub(crate) unsafe fn drag_selected_overlay(index: usize, dx: f32, dy: f32) {
  let update = {
    let guard = CAPTURE_STATE.lock().unwrap();
    let mut config_guard = CONFIG.lock().unwrap();
    match (guard.as_ref(), config_guard.as_mut()) {
      (Some(state), Some(config)) => {
        match (state.overlays.get(index), config.overlays.get_mut(index)) {
          (Some(overlay), Some(cfg)) => {
            cfg.x += dx;
            cfg.y += dy;
            Some((overlay.item.0, cfg.x, cfg.y))
          }
          _ => None,
        }
      }
      _ => None,
    }
  };
  if let Some((item, x, y)) = update {
    obs_ffi::obs_sceneitem_set_pos(item, &vec2_of(x, y) as *const _);
    preview::refresh_selection_rect();
  }
}

pub(crate) fn finish_drag_overlay() {
  persist_config();
  preview::refresh_selection_rect();
  if let Some(app) = APP_HANDLE.get() {
    let list = overlay_info_list();
    let _ = app.emit("overlays-updated", list);
  }
}

pub(crate) unsafe fn get_overlay_source_dims_and_scale(index: usize) -> Option<(f32, f32, f32, f32)> {
  let (source, sx, sy) = {
    let guard = CAPTURE_STATE.lock().unwrap();
    let state = guard.as_ref()?;
    let config_guard = CONFIG.lock().unwrap();
    let config = config_guard.as_ref()?;
    let overlay = state.overlays.get(index)?;
    let cfg = config.overlays.get(index)?;
    (overlay.source.0, cfg.get_scale_x(), cfg.get_scale_y())
  };

  let w = obs_ffi::obs_source_get_width(source) as f32;
  let h = obs_ffi::obs_source_get_height(source) as f32;

  Some((w, h, sx, sy))
}

pub(crate) unsafe fn get_overlay_rect(index: usize) -> Option<(f32, f32, f32, f32)> {
  let (source, x, y, sx, sy) = {
    let guard = CAPTURE_STATE.lock().unwrap();
    let state = guard.as_ref()?;
    let config_guard = CONFIG.lock().unwrap();
    let config = config_guard.as_ref()?;
    let overlay = state.overlays.get(index)?;
    let cfg = config.overlays.get(index)?;
    (overlay.source.0, cfg.x, cfg.y, cfg.get_scale_x(), cfg.get_scale_y())
  };

  let w = obs_ffi::obs_source_get_width(source) as f32;
  let h = obs_ffi::obs_source_get_height(source) as f32;

  Some((x, y, w * sx, h * sy))
}

pub(crate) unsafe fn resize_selected_overlay(index: usize, x: f32, y: f32, scale_x: f32, scale_y: f32) {
  let item = {
    let guard = CAPTURE_STATE.lock().unwrap();
    let mut config_guard = CONFIG.lock().unwrap();
    match (guard.as_ref(), config_guard.as_mut()) {
      (Some(state), Some(config)) => {
        match (state.overlays.get(index), config.overlays.get_mut(index)) {
          (Some(overlay), Some(cfg)) => {
            cfg.x = x;
            cfg.y = y;
            cfg.scale_x = scale_x;
            cfg.scale_y = scale_y;
            cfg.scale = scale_x; // sync legacy scale
            Some(overlay.item.0)
          }
          _ => None,
        }
      }
      _ => None,
    }
  };
  if let Some(item) = item {
    obs_ffi::obs_sceneitem_set_pos(item, &vec2_of(x, y) as *const _);
    obs_ffi::obs_sceneitem_set_scale(item, &vec2_of(scale_x, scale_y) as *const _);
    preview::refresh_selection_rect();
  }
}

unsafe fn ensure_output_started(clip_seconds: i64) -> Result<PathBuf, String> {
  {
    let guard = OUTPUT_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      let _ = state.max_time_sec; // ya armado; ignoramos clip_seconds nuevo (hay que stop_recording primero)
      let clips_dir = with_config(|c| c.clips_dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dir().join("clips"));
      return Ok(clips_dir);
    }
  }

  let aenc_id = CString::new("ffmpeg_aac").unwrap();
  let aenc_name = CString::new("Ember Audio Encoder").unwrap();
  let audio_encoder =
    obs_ffi::obs_audio_encoder_create(aenc_id.as_ptr(), aenc_name.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut());
  if audio_encoder.is_null() {
    return Err("obs_audio_encoder_create('ffmpeg_aac') devolvio null".into());
  }
  obs_ffi::obs_encoder_set_audio(audio_encoder, obs_ffi::obs_get_audio());

  let clips_dir = with_config(|c| c.clips_dir.clone())
    .map(PathBuf::from)
    .unwrap_or_else(|| app_dir().join("clips"));
  std::fs::create_dir_all(&clips_dir).map_err(|e| format!("no se pudo crear la carpeta de clips: {e}"))?;

  let settings = obs_ffi::obs_data_create();
  let dir_c = CString::new(clips_dir.to_string_lossy().replace('\\', "/")).unwrap();
  let dir_key = CString::new("directory").unwrap();
  obs_ffi::obs_data_set_string(settings, dir_key.as_ptr(), dir_c.as_ptr());
  let fmt_key = CString::new("format").unwrap();
  let fmt_val = CString::new("ember-clip-%CCYY-%MM-%DD-%hh-%mm-%ss").unwrap();
  obs_ffi::obs_data_set_string(settings, fmt_key.as_ptr(), fmt_val.as_ptr());
  let ext_key = CString::new("extension").unwrap();
  let ext_val = CString::new("mp4").unwrap();
  obs_ffi::obs_data_set_string(settings, ext_key.as_ptr(), ext_val.as_ptr());
  let max_time_key = CString::new("max_time_sec").unwrap();
  obs_ffi::obs_data_set_int(settings, max_time_key.as_ptr(), clip_seconds);
  let max_size_key = CString::new("max_size_mb").unwrap();
  obs_ffi::obs_data_set_int(settings, max_size_key.as_ptr(), 1000);

  let output_id = CString::new("replay_buffer").unwrap();
  let output_name = CString::new("Ember Replay Buffer").unwrap();
  let output = obs_ffi::obs_output_create(output_id.as_ptr(), output_name.as_ptr(), settings, std::ptr::null_mut());
  obs_ffi::obs_data_release(settings);
  if output.is_null() {
    obs_ffi::obs_encoder_release(audio_encoder);
    return Err("obs_output_create('replay_buffer') devolvio null".into());
  }
  obs_ffi::obs_output_set_audio_encoder(output, audio_encoder, 0);

  // Codificador de video: por defecto ("auto") probamos hardware por vendor
  // en orden de preferencia y caemos a x264 por software si ninguno
  // funciona. No basta con que obs_video_encoder_create() no devuelva null
  // -- en encoders "_tex" (NVENC/AMF/QSV) la sesion real de hardware recien
  // se intenta crear adentro de obs_output_start(), y si falla ahi libobs
  // muchas veces no deja un mensaje en last_error (queda "razon
  // desconocida"). Por eso el unico chequeo confiable es intentar arrancar
  // el output con cada candidato.
  //
  // Si el usuario eligio un encoder puntual en configuraciones, respetamos
  // esa eleccion (sin fallback automatico a otro): es una eleccion explicita
  // para forzar rendimiento o depurar, y silenciarla con un fallback la
  // volveria inutil para ese proposito.
  let selection = with_config(|c| c.video_encoder.clone());
  let forced = VIDEO_ENCODER_CANDIDATES.iter().find(|entry| entry.0 == selection);
  let candidates: Vec<(&str, &str)> = match forced {
    Some(entry) => vec![*entry],
    None => VIDEO_ENCODER_CANDIDATES.to_vec(), // "auto" o valor desconocido/viejo -> probamos todos
  };

  let registered = registered_encoder_ids();
  let mut video_encoder: *mut obs_ffi::obs_encoder_t = std::ptr::null_mut();
  let mut chosen_label = "";
  let mut attempt_errors: Vec<String> = Vec::new();

  for (id, label) in candidates {
    if !registered.iter().any(|r| r == id) {
      continue; // este build de libobs no cargo el plugin de este encoder, ni lo intentamos
    }

    let venc_id = CString::new(id).unwrap();
    let venc_name = CString::new("Ember Video Encoder").unwrap();
    let candidate =
      obs_ffi::obs_video_encoder_create(venc_id.as_ptr(), venc_name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
    if candidate.is_null() {
      attempt_errors.push(format!("{label} ({id}): obs_video_encoder_create devolvio null"));
      continue;
    }
    obs_ffi::obs_encoder_set_video(candidate, obs_ffi::obs_get_video());
    obs_ffi::obs_output_set_video_encoder(output, candidate);

    if obs_ffi::obs_output_start(output) {
      video_encoder = candidate;
      chosen_label = label;
      break;
    }

    let err_ptr = obs_ffi::obs_output_get_last_error(output);
    let err = if err_ptr.is_null() {
      "razon desconocida (probablemente el hardware no soporta este codificador)".to_string()
    } else {
      CStr::from_ptr(err_ptr).to_string_lossy().into_owned()
    };
    attempt_errors.push(format!("{label} ({id}): {err}"));
    obs_ffi::obs_encoder_release(candidate);
  }

  if video_encoder.is_null() {
    obs_ffi::obs_encoder_release(audio_encoder);
    obs_ffi::obs_output_release(output);
    return Err(format!(
      "No se pudo iniciar la grabacion con ningun codificador de video disponible:\n{}",
      attempt_errors.join("\n")
    ));
  }

  log::info!("Grabacion iniciada usando codificador de video: {chosen_label}");

  *OUTPUT_STATE.lock().unwrap() = Some(OutputState {
    output: RawPtr(output),
    video_encoder: RawPtr(video_encoder),
    audio_encoder: RawPtr(audio_encoder),
    max_time_sec: clip_seconds,
  });

  Ok(clips_dir)
}

/// Lista los IDs de encoder que libobs registro efectivamente en este build
/// (obs_load_all_modules ya corrio). Sirve para no intentar crear un
/// encoder de un vendor cuyo plugin ni siquiera cargo (ej. AMF en una PC
/// sin GPU AMD), evitando ruido y objetos "dummy" que fallan mas adelante.
unsafe fn registered_encoder_ids() -> Vec<String> {
  let mut ids = Vec::new();
  let mut idx: usize = 0;
  loop {
    let mut id_ptr: *const std::os::raw::c_char = std::ptr::null();
    if !obs_ffi::obs_enum_encoder_types(idx, &mut id_ptr as *mut _) {
      break;
    }
    if !id_ptr.is_null() {
      ids.push(CStr::from_ptr(id_ptr).to_string_lossy().into_owned());
    }
    idx += 1;
  }
  ids
}



async fn save_clip_and_notify() {
  let result = save_clip_internal().await;
  if let Some(app) = APP_HANDLE.get() {
    match result {
      Ok(path) => {
        let _ = app.emit("clip-saved", path);
        let clip_seconds = with_config(|c| c.clip_seconds);
        show_toast_notification(app, clip_seconds);
      }
      Err(err) => {
        let _ = app.emit("clip-save-error", err);
      }
    }
  }
}

async fn toggle_recording_and_notify() {
  let is_recording = { OUTPUT_STATE.lock().unwrap().is_some() };
  if is_recording {
    let _ = stop_recording_impl();
    if let Some(app) = APP_HANDLE.get() {
      let _ = app.emit("recording-status-changed", false);
      show_status_notification(app, false);
    }
  } else {
    let clip_seconds = with_config(|c| c.clip_seconds);
    unsafe {
      if let Err(e) = ensure_obs_platform_initialized() {
        log::error!("Error al inicializar libobs en toggle: {}", e);
        return;
      }
      if let Err(e) = reapply_video_settings_if_changed() {
        log::error!("Error al reaplicar video settings en toggle: {}", e);
      }
      if let Err(e) = ensure_capture_started() {
        log::error!("Error al iniciar captura en toggle: {}", e);
        return;
      }
      match ensure_output_started(clip_seconds) {
        Ok(_) => {
          if let Some(app) = APP_HANDLE.get() {
            let _ = app.emit("recording-status-changed", true);
            show_status_notification(app, true);
          }
        }
        Err(e) => {
          log::error!("Error al iniciar output en toggle: {}", e);
        }
      }
    }
  }
}

async fn save_clip_internal() -> Result<String, String> {
  let output_ptr: RawPtr<obs_ffi::obs_output_t> = {
    let guard = OUTPUT_STATE.lock().unwrap();
    match &*guard {
      Some(state) => RawPtr(state.output.0),
      None => return Err("Todavia no estas grabando".into()),
    }
  };

  unsafe {
    if !call_proc_no_args(output_ptr.0, "save") {
      return Err("No se pudo llamar al proc 'save'".into());
    }
  }

  for _ in 0..50 {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let path = unsafe { call_proc_get_string(output_ptr.0, "get_last_replay", "path") };
    if let Some(path) = path {
      return Ok(path);
    }
  }

  Err("Timeout esperando que se guarde el clip (5s)".into())
}

// ---------------------------------------------------------------------------
// Comandos expuestos al frontend
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_obs_version() -> String {
  unsafe {
    let ptr = obs_ffi::obs_get_version_string();
    if ptr.is_null() {
      return "obs_get_version_string() devolvio null".into();
    }
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
  }
}

/// "Quiero grabar": inicializa libobs si hace falta, arma la captura
/// (monitor + audio) si hace falta, arranca el replay buffer con la
/// duracion pedida, y asegura que el hotkey de guardado este activo.
// Async a proposito: la primera vez hace trabajo pesado (obs_startup, carga
// de modulos, creacion de encoders). Como comando sync corria en el thread
// principal y congelaba la UI varios segundos.
#[tauri::command]
async fn start_recording(clip_seconds: i64) -> Result<String, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    // Solo reaplicamos video/FPS/resolucion si cambiaron y el subsistema de
    // video de OBS no esta activo. obs_reset_video falla/crashea si hay
    // grabacion en curso, y resetear sin necesidad abre ventanas de crash.
    reapply_video_settings_if_changed()?;
    ensure_capture_started()?;
    let clips_dir = ensure_output_started(clip_seconds)?;

    with_config(|c| c.clip_seconds = clip_seconds);
    persist_config();

    if let Some(app) = APP_HANDLE.get() {
      show_status_notification(app, true);
    }

    Ok(format!(
      "Grabando (buffer de {clip_seconds}s). Los clips se guardan en: {}",
      clips_dir.to_string_lossy()
    ))
  }
}

/// Corta la grabacion y libera todo lo creado (output, encoders, fuentes,
/// escena). libobs en si (obs_startup) queda inicializado para poder volver
/// a arrancar rapido con start_recording.
unsafe fn cleanup_capture_if_idle() {
  // Dejamos la captura activa en segundo plano para que el mezclador de audio
  // y los atajos globales nativos sigan funcionando siempre de manera continua.
}

/// Libera todos los recursos de libobs de un CaptureState ya sacado del
/// static (el lock NO debe estar tomado al llamar esto).
unsafe fn release_capture_state(state: CaptureState) {
  obs_ffi::obs_set_output_source(0, std::ptr::null_mut());
  for i in 1..=MAX_AUDIO_SOURCES {
    obs_ffi::obs_set_output_source(i as u32, std::ptr::null_mut());
  }
  for volmeter in state.audio_volmeters {
    if !volmeter.0.is_null() {
      // obs_volmeter_destroy libera tambien los callbacks registrados,
      // no hace falta remove_callback aparte.
      obs_ffi::obs_volmeter_detach_source(volmeter.0);
      obs_ffi::obs_volmeter_destroy(volmeter.0);
    }
  }
  obs_ffi::obs_source_release(state.capture_source.0);
  for source in state.audio_sources {
    obs_ffi::obs_source_release(source.0);
  }
  for overlay in state.overlays {
    if !overlay.opacity_filter.0.is_null() {
      obs_ffi::obs_source_release(overlay.opacity_filter.0);
    }
    obs_ffi::obs_source_release(overlay.source.0);
  }
  obs_ffi::obs_scene_release(state.scene.0);
}

#[tauri::command]
async fn stop_recording() -> Result<String, String> {
  let res = stop_recording_impl();
  if res.is_ok() {
    if let Some(app) = APP_HANDLE.get() {
      let _ = app.emit("recording-status-changed", false);
      show_status_notification(app, false);
    }
  }
  res
}

fn stop_recording_impl() -> Result<String, String> {
  unsafe {
    // take-then-drop: no sostener OUTPUT_STATE durante las llamadas a libobs.
    let state_opt = { OUTPUT_STATE.lock().unwrap().take() };
    if let Some(state) = state_opt {
      obs_ffi::obs_output_stop(state.output.0);

      // obs_output_stop es asincrono en libobs -- esperar a que el output
      // realmente se detenga antes de liberar recursos para evitar
      // use-after-free / crashes.
      for _ in 0..100 {
        if !obs_ffi::obs_output_active(state.output.0) {
          break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
      }

      obs_ffi::obs_output_release(state.output.0);
      obs_ffi::obs_encoder_release(state.video_encoder.0);
      obs_ffi::obs_encoder_release(state.audio_encoder.0);
    }
    cleanup_capture_if_idle();
  }
  Ok("Grabacion detenida y recursos liberados".into())
}

/// Dispara el guardado manualmente (equivalente a apretar el hotkey). El
/// resultado llega por evento ("clip-saved" / "clip-save-error"), no por el
/// valor de retorno, para compartir el mismo camino que el hotkey global.
#[tauri::command]
fn save_clip_now() {
  tauri::async_runtime::spawn(save_clip_and_notify());
}

/// Cambia que tecla dispara "guardar clip". `vk_code` es un codigo de
/// virtual-key de Windows (el frontend lo saca de `KeyboardEvent.keyCode`,
/// que en Windows coincide con los VK_* para teclas comunes).
#[tauri::command]
fn set_save_hotkey(vk_code: i32, ctrl: bool, shift: bool, alt: bool) -> Result<String, String> {
  register_native_hotkey(1, vk_code, ctrl, shift, alt);

  with_config(|c| c.hotkey = Some(config::HotkeyConfig { vk_code, ctrl, shift, alt }));
  persist_config();

  let combo_desc = format!(
    "{}{}{}vk={vk_code}",
    if ctrl { "Ctrl+" } else { "" },
    if shift { "Shift+" } else { "" },
    if alt { "Alt+" } else { "" },
  );

  Ok(format!("Hotkey de guardado ahora es: {combo_desc}"))
}

#[tauri::command]
fn set_toggle_hotkey(vk_code: i32, ctrl: bool, shift: bool, alt: bool) -> Result<String, String> {
  register_native_hotkey(2, vk_code, ctrl, shift, alt);

  with_config(|c| c.hotkey_toggle = Some(config::HotkeyConfig { vk_code, ctrl, shift, alt }));
  persist_config();

  let combo_desc = format!(
    "{}{}{}vk={vk_code}",
    if ctrl { "Ctrl+" } else { "" },
    if shift { "Shift+" } else { "" },
    if alt { "Alt+" } else { "" },
  );

  Ok(format!("Hotkey de encendido/apagado ahora es: {combo_desc}"))
}

/// Cambia donde se guardan los clips. Si ya estas grabando, el output ya
/// armado sigue usando el directorio viejo -- hay que stop_recording +
/// start_recording de nuevo para que tome el cambio.
#[tauri::command]
fn set_clips_dir(dir: String) -> Result<String, String> {
  let path = PathBuf::from(&dir);
  std::fs::create_dir_all(&path).map_err(|e| format!("no se pudo usar esa carpeta: {e}"))?;
  with_config(|c| c.clips_dir = Some(dir.clone()));
  persist_config();
  Ok(format!("Los clips nuevos se van a guardar en: {dir}"))
}

#[tauri::command]
fn get_config() -> config::EmberioConfig {
  with_config(|c| c.clone())
}

/// Codificadores de video utilizables para grabar en esta PC: "auto" mas
/// los que libobs realmente registro (ver VIDEO_ENCODER_CANDIDATES). Que un
/// encoder este registrado no garantiza al 100% que el hardware funcione
/// (ver ensure_output_started), pero descarta de entrada los vendors que ni
/// siquiera cargaron su plugin.
#[tauri::command]
async fn list_video_encoders() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    let registered = registered_encoder_ids();
    let mut options = vec![PropertyOption { name: "Automatico (recomendado)".to_string(), value: "auto".to_string() }];
    for (id, label) in VIDEO_ENCODER_CANDIDATES {
      if registered.iter().any(|r| r == id) {
        options.push(PropertyOption { name: label.to_string(), value: id.to_string() });
      }
    }
    Ok(options)
  }
}

/// Lista de pantallas disponibles (mismo mecanismo que usa la ventana de
/// configuracion de OBS: EnumDisplayMonitors corre en vivo al pedir las
/// propiedades de "monitor_capture").
#[tauri::command]
async fn list_monitors() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("monitor_capture", "monitor_id"))
  }
}

#[tauri::command]
async fn list_audio_output_devices() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("wasapi_output_capture", "device_id"))
  }
}

#[tauri::command]
async fn list_audio_input_devices() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("wasapi_input_capture", "device_id"))
  }
}

/// Elegir que pantalla capturar.
#[tauri::command]
async fn set_monitor(id: String) -> Result<String, String> {
  set_video_source("screen".to_string(), id).await?;
  Ok("Pantalla de captura configurada.".into())
}

/// Arma el obs_data de settings para una fuente de captura de video segun el
/// plugin y el id elegido (monitor o ventana).
unsafe fn build_capture_settings(source_plugin: &str, source_id: &str) -> *mut obs_ffi::obs_data_t {
  let settings = obs_ffi::obs_data_create();
  let val_c = CString::new(source_id).unwrap();
  if source_plugin == "monitor_capture" {
    let key_c = CString::new("monitor_id").unwrap();
    obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
  } else {
    let key_c = CString::new("window").unwrap();
    obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
    if source_plugin == "window_capture" {
      let method_key = CString::new("method").unwrap();
      obs_ffi::obs_data_set_int(settings, method_key.as_ptr(), 2);
    } else if source_plugin == "game_capture" {
      let mode_key = CString::new("capture_mode").unwrap();
      let mode_val = CString::new("window").unwrap();
      obs_ffi::obs_data_set_string(settings, mode_key.as_ptr(), mode_val.as_ptr());
    }
  }
  settings
}

#[tauri::command]
async fn set_video_source(source_type: String, source_id: String) -> Result<(), String> {
  with_config(|c| {
    c.video_source_type = source_type.clone();
    c.video_source_id = Some(source_id.clone());
  });
  persist_config();

  let source_plugin = match source_type.as_str() {
    "window" => "window_capture",
    "game" => "game_capture",
    _ => "monitor_capture",
  };

  unsafe {
    // Snapshot de punteros: NO mantener el lock mientras llamamos a libobs.
    let (scene, current_source) = {
      let guard = CAPTURE_STATE.lock().unwrap();
      match &*guard {
        Some(state) => (state.scene.0, state.capture_source.0),
        // Sin captura activa: la config ya quedo guardada y se usa cuando
        // arranque (ensure_capture_started).
        None => return Ok(()),
      }
    };

    let current_id = CStr::from_ptr(obs_ffi::obs_source_get_id(current_source)).to_string_lossy().into_owned();

    if current_id == source_plugin {
      // Mismo tipo de fuente (ej. cambiar de una pantalla a otra, o de una
      // ventana a otra): actualizar en caliente, igual que el dialogo de
      // propiedades de OBS. Nada se destruye, no hay ventana de crash.
      let settings = build_capture_settings(source_plugin, &source_id);
      obs_ffi::obs_source_update(current_source, settings);
      obs_ffi::obs_data_release(settings);
      return Ok(());
    }

    // Tipo distinto (pantalla <-> ventana <-> juego): crear la fuente nueva
    // primero, agregarla a la escena, y recien despues sacar y soltar la
    // vieja -- todo sin mantener CAPTURE_STATE tomado durante llamadas obs.
    let plugin_c = CString::new(source_plugin).unwrap();
    let name_c = CString::new(format!("Captura Principal ({source_plugin})")).unwrap();
    let settings = build_capture_settings(source_plugin, &source_id);
    let new_source = obs_ffi::obs_source_create(plugin_c.as_ptr(), name_c.as_ptr(), settings, std::ptr::null_mut());
    obs_ffi::obs_data_release(settings);

    if new_source.is_null() {
      return Err("Error al crear la nueva fuente de captura".into());
    }

    let new_item = obs_ffi::obs_scene_add(scene, new_source);
    if new_item.is_null() {
      obs_ffi::obs_source_release(new_source);
      return Err("Error al agregar la nueva fuente a la escena".into());
    }

    // Publicar los punteros nuevos y quedarnos con los viejos para soltarlos.
    let old = {
      let mut guard = CAPTURE_STATE.lock().unwrap();
      match guard.as_mut() {
        Some(state) => {
          let old = (state.capture_item.0, state.capture_source.0);
          state.capture_source = RawPtr(new_source);
          state.capture_item = RawPtr(new_item);
          sync_scene_z_order(state);
          Some(old)
        }
        None => None,
      }
    };

    if let Some((old_item, old_source)) = old {
      if !old_item.is_null() {
        obs_ffi::obs_sceneitem_remove(old_item);
      }
      obs_ffi::obs_source_release(old_source);
    } else {
      // La captura se desarmo en el medio; deshacer lo nuestro.
      obs_ffi::obs_sceneitem_remove(new_item);
      obs_ffi::obs_source_release(new_source);
    }
  }
  Ok(())
}

#[tauri::command]
async fn list_windows() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("window_capture", "window"))
  }
}

#[tauri::command]
async fn list_games() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("game_capture", "window"))
  }
}

#[tauri::command]
async fn set_overlay_locked(index: usize, locked: bool) -> Result<Vec<OverlayInfo>, String> {
  {
    let guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      if let Some(item) = state.overlays.get(index) {
        unsafe { obs_ffi::obs_sceneitem_set_locked(item.item.0, locked) };
      }
    }
  }
  with_config(|c| {
    if let Some(o) = c.overlays.get_mut(index) {
      o.locked = locked;
    }
  });
  persist_config();
  Ok(overlay_info_list())
}

#[tauri::command]
async fn reorder_overlay(index: usize, up: bool) -> Result<Vec<OverlayInfo>, String> {
  // Cambian los indices: la seleccion vieja del preview deja de valer.
  preview::clear_selection();
  unsafe {
    let mut guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
      let len = state.overlays.len();
      let target_index = if up {
        if index > 0 { index - 1 } else { return Err("Ya está al frente".into()); }
      } else {
        if index + 1 < len { index + 1 } else { return Err("Ya está al fondo".into()); }
      };
      
      state.overlays.swap(index, target_index);
      sync_scene_z_order(state);
    }
  }

  with_config(|c| {
    let len = c.overlays.len();
    let target_index = if up {
      if index > 0 { index - 1 } else { index }
    } else {
      if index + 1 < len { index + 1 } else { index }
    };
    c.overlays.swap(index, target_index);
  });
  persist_config();

  Ok(overlay_info_list())
}

#[tauri::command]
async fn set_audio_source_label(index: usize, label: String) -> Result<Vec<config::AudioSourceConfig>, String> {
  let list = with_config(|c| {
    if let Some(entry) = c.audio_sources.get_mut(index) {
      entry.label = label;
    }
    c.audio_sources.clone()
  });
  persist_config();
  Ok(list)
}

/// Agrega una fuente de audio a la lista (hasta MAX_AUDIO_SOURCES). Pensado
/// para setups tipo Voicemeeter con varios buses virtuales que se quieren
/// capturar todos a la vez, cada uno en su propio canal.
#[tauri::command]
async fn add_audio_source(kind: String, device_id: String, label: String) -> Result<Vec<config::AudioSourceConfig>, String> {
  let kind = match kind.as_str() {
    "output" => config::AudioSourceKind::Output,
    "input" => config::AudioSourceKind::Input,
    other => return Err(format!("kind invalido: {other} (usa 'output' o 'input')")),
  };

  let sources = with_config(|c| {
    if c.audio_sources.len() >= MAX_AUDIO_SOURCES {
      return None;
    }
    c.audio_sources.push(config::AudioSourceConfig { kind, device_id, label, volume: 1.0, muted: false });
    Some(c.audio_sources.clone())
  });

  match sources {
    Some(list) => {
      persist_config();
      Ok(list)
    }
    None => Err(format!("Ya hay el maximo de {MAX_AUDIO_SOURCES} fuentes de audio")),
  }
}

#[tauri::command]
async fn remove_audio_source(index: usize) -> Result<Vec<config::AudioSourceConfig>, String> {
  let list = with_config(|c| {
    if index < c.audio_sources.len() {
      c.audio_sources.remove(index);
    }
    c.audio_sources.clone()
  });
  persist_config();
  Ok(list)
}

/// Ajusta volumen/mute de una fuente de audio ya configurada. Si la
/// grabacion esta activa, tambien lo aplica en vivo sobre la fuente real
/// (sin necesidad de reiniciar), igual que el mixer de OBS.
#[tauri::command]
async fn set_audio_source_volume(index: usize, volume: f32) -> Result<Vec<config::AudioSourceConfig>, String> {
  let list = with_config(|c| {
    if let Some(entry) = c.audio_sources.get_mut(index) {
      entry.volume = volume;
    }
    c.audio_sources.clone()
  });
  persist_config();

  if let Some(state) = &*CAPTURE_STATE.lock().unwrap() {
    if let Some(source) = state.audio_sources.get(index) {
      unsafe { obs_ffi::obs_source_set_volume(source.0, volume) };
    }
  }

  Ok(list)
}

#[tauri::command]
async fn set_audio_source_muted(index: usize, muted: bool) -> Result<Vec<config::AudioSourceConfig>, String> {
  let list = with_config(|c| {
    if let Some(entry) = c.audio_sources.get_mut(index) {
      entry.muted = muted;
    }
    c.audio_sources.clone()
  });
  persist_config();

  if let Some(state) = &*CAPTURE_STATE.lock().unwrap() {
    if let Some(source) = state.audio_sources.get(index) {
      unsafe { obs_ffi::obs_source_set_muted(source.0, muted) };
    }
  }

  Ok(list)
}

// ---------------------------------------------------------------------------
// Fuentes de escena: overlays simples (imagen/texto) encima de la captura.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Clone)]
struct OverlayInfo {
  index: usize,
  kind: String,
  content: String,
  x: f32,
  y: f32,
  scale: f32,
  scale_x: f32,
  scale_y: f32,
  visible: bool,
  locked: bool,
  opacity: f32,
}

fn overlay_kind_str(kind: config::OverlayKind) -> &'static str {
  match kind {
    config::OverlayKind::Image => "image",
    config::OverlayKind::Text => "text",
    config::OverlayKind::Browser => "browser",
  }
}

fn overlay_info_list() -> Vec<OverlayInfo> {
  with_config(|c| {
    c.overlays
      .iter()
      .enumerate()
      .map(|(index, o)| OverlayInfo {
        index,
        kind: overlay_kind_str(o.kind).to_string(),
        content: o.content.clone(),
        x: o.x,
        y: o.y,
        scale: o.scale,
        scale_x: o.get_scale_x(),
        scale_y: o.get_scale_y(),
        visible: o.visible,
        locked: o.locked,
        opacity: o.opacity,
      })
      .collect()
  })
}

#[tauri::command]
fn list_overlays() -> Vec<OverlayInfo> {
  overlay_info_list()
}

unsafe fn add_overlay(cfg: config::OverlayConfig) -> Result<Vec<OverlayInfo>, String> {
  ensure_obs_platform_initialized()?;
  ensure_capture_started()?;

  let scene = match &*CAPTURE_STATE.lock().unwrap() {
    Some(state) => state.scene.0,
    None => return Err("La captura todavia no esta lista".into()),
  };
  let item = create_overlay_item(scene, &cfg)?;
  {
    let mut guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
      state.overlays.insert(0, item);
      sync_scene_z_order(state);
    }
  }
  with_config(|c| c.overlays.insert(0, cfg));
  persist_config();
  Ok(overlay_info_list())
}

/// Agrega un overlay de imagen (logo, marco, etc). `path` es una ruta de
/// archivo absoluta (el frontend la saca del dialogo nativo de archivos).
#[tauri::command]
async fn add_image_overlay(path: String) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    add_overlay(config::OverlayConfig {
      kind: config::OverlayKind::Image,
      content: path,
      x: 100.0,
      y: 100.0,
      scale: 1.0,
      scale_x: 1.0,
      scale_y: 1.0,
      visible: true,
      locked: false,
      opacity: 1.0,
    })
  }
}

/// Agrega un overlay de texto simple (fuente/color por defecto de libobs).
#[tauri::command]
async fn add_text_overlay(text: String) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    add_overlay(config::OverlayConfig {
      kind: config::OverlayKind::Text,
      content: text,
      x: 100.0,
      y: 100.0,
      scale: 1.0,
      scale_x: 1.0,
      scale_y: 1.0,
      visible: true,
      locked: false,
      opacity: 1.0,
    })
  }
}

/// Agrega un overlay de navegador (widget o pagina web). `url` es la direccion.
#[tauri::command]
async fn add_browser_overlay(url: String) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    add_overlay(config::OverlayConfig {
      kind: config::OverlayKind::Browser,
      content: url,
      x: 100.0,
      y: 100.0,
      scale: 1.0,
      scale_x: 1.0,
      scale_y: 1.0,
      visible: true,
      locked: false,
      opacity: 1.0,
    })
  }
}

#[tauri::command]
async fn remove_overlay(index: usize) -> Result<Vec<OverlayInfo>, String> {
  // Los indices de los demas overlays cambian: soltar la seleccion antes de
  // tocar nada para que el preview no dibuje/drag-ee un indice viejo.
  preview::clear_selection();
  unsafe {
    let removed = {
      let mut guard = CAPTURE_STATE.lock().unwrap();
      match guard.as_mut() {
        Some(state) if index < state.overlays.len() => {
          let item = state.overlays.remove(index);
          sync_scene_z_order(state);
          Some(item)
        }
        _ => None,
      }
    };
    if let Some(item) = removed {
      obs_ffi::obs_sceneitem_remove(item.item.0);
      if !item.opacity_filter.0.is_null() {
        obs_ffi::obs_source_release(item.opacity_filter.0);
      }
      obs_ffi::obs_source_release(item.source.0);
    }
  }
  with_config(|c| {
    if index < c.overlays.len() {
      c.overlays.remove(index);
    }
  });
  persist_config();
  Ok(overlay_info_list())
}

/// Opacidad de un overlay (0.0 = invisible, 1.0 = opaco), aplicada en vivo
/// via el filtro de correccion de color, igual que OBS.
#[tauri::command]
async fn set_overlay_opacity(index: usize, opacity: f32) -> Result<Vec<OverlayInfo>, String> {
  let opacity = opacity.clamp(0.0, 1.0);
  let filter = {
    let guard = CAPTURE_STATE.lock().unwrap();
    guard.as_ref().and_then(|state| state.overlays.get(index)).map(|o| o.opacity_filter.0)
  };
  if let Some(filter) = filter {
    if !filter.is_null() {
      unsafe {
        let settings = obs_ffi::obs_data_create();
        let key = CString::new("opacity").unwrap();
        obs_ffi::obs_data_set_double(settings, key.as_ptr(), opacity as f64);
        obs_ffi::obs_source_update(filter, settings);
        obs_ffi::obs_data_release(settings);
      }
    }
  }
  with_config(|c| {
    if let Some(o) = c.overlays.get_mut(index) {
      o.opacity = opacity;
    }
  });
  persist_config();
  Ok(overlay_info_list())
}

#[tauri::command]
async fn set_overlay_visible(index: usize, visible: bool) -> Result<Vec<OverlayInfo>, String> {
  {
    let guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      if let Some(item) = state.overlays.get(index) {
        unsafe { obs_ffi::obs_sceneitem_set_visible(item.item.0, visible) };
      }
    }
  }
  with_config(|c| {
    if let Some(o) = c.overlays.get_mut(index) {
      o.visible = visible;
    }
  });
  persist_config();
  preview::refresh_selection_rect();
  Ok(overlay_info_list())
}

/// Reposiciona/escala un overlay ya creado (posicion en pixeles del canvas
/// base, escala uniforme 0.1..=5.0).
#[tauri::command]
async fn set_overlay_transform(index: usize, x: f32, y: f32, scale_x: f32, scale_y: f32) -> Result<Vec<OverlayInfo>, String> {
  {
    let guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      if let Some(item) = state.overlays.get(index) {
        unsafe {
          obs_ffi::obs_sceneitem_set_pos(item.item.0, &vec2_of(x, y) as *const _);
          obs_ffi::obs_sceneitem_set_scale(item.item.0, &vec2_of(scale_x, scale_y) as *const _);
        }
      }
    }
  }
  with_config(|c| {
    if let Some(o) = c.overlays.get_mut(index) {
      o.x = x;
      o.y = y;
      o.scale_x = scale_x;
      o.scale_y = scale_y;
      o.scale = scale_x; // sync legacy scale
    }
  });
  persist_config();
  preview::refresh_selection_rect();
  Ok(overlay_info_list())
}

// ---------------------------------------------------------------------------
// Preview en vivo (toggable -- ver preview.rs para el por que).
// ---------------------------------------------------------------------------

/// Prende o apaga el preview. Al prenderlo crea la ventana nativa + el
/// `obs_display` de libobs sobre el rectangulo indicado (coordenadas de
/// cliente de la ventana principal, en pixeles fisicos); al apagarlo los
/// destruye. Sin esto activo no hay ningun costo extra de renderizado.
#[tauri::command]
fn toggle_preview(enabled: bool, x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
  unsafe {
    if enabled {
      ensure_obs_platform_initialized()?;
      ensure_capture_started()?;
      if PREVIEW_STATE.lock().unwrap().is_some() {
        return Ok(());
      }
      let app = APP_HANDLE.get().ok_or("La app todavia no esta lista")?;
      let window = app.get_webview_window("main").ok_or("No se encontro la ventana principal")?;
      let hwnd = window.hwnd().map_err(|e| format!("no se pudo obtener el HWND de la ventana: {e}"))?;
      let state = preview::create(hwnd.0 as windows_sys::Win32::Foundation::HWND, x, y, width, height)?;
      *PREVIEW_STATE.lock().unwrap() = Some(state);
    } else {
      // OJO: sacar el estado en un bloque aparte -- en un `if let` el guard
      // temporal del Mutex vive durante TODO el cuerpo, y adentro
      // cleanup_capture_if_idle() vuelve a lockear PREVIEW_STATE
      // (self-deadlock: la app quedaba congelada al apagar el preview).
      let state_opt = { PREVIEW_STATE.lock().unwrap().take() };
      if let Some(state) = state_opt {
        preview::destroy(state);
        preview::clear_selection();
        cleanup_capture_if_idle();
      }
    }
  }
  Ok(())
}

/// Actualiza el rectangulo del preview (ej. al resizear la ventana). No hace
/// nada si el preview esta apagado.
#[tauri::command]
fn update_preview_rect(x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
  unsafe {
    let guard = PREVIEW_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      let app = APP_HANDLE.get().ok_or("La app todavia no esta lista")?;
      let window = app.get_webview_window("main").ok_or("No se encontro la ventana principal")?;
      let hwnd = window.hwnd().map_err(|e| format!("no se pudo obtener el HWND: {e}"))?;
      preview::resize(state, hwnd.0 as windows_sys::Win32::Foundation::HWND, x, y, width, height);
    }
  }
  Ok(())
}

/// Resolucion base del canvas de OBS -- el frontend la usa para darle al
/// rectangulo del preview exactamente la misma proporcion (sin franjas
/// negras ambiguas).
#[tauri::command]
fn get_canvas_size() -> (u32, u32) {
  preview::canvas_size()
}

#[tauri::command]
fn set_preview_grid(grid: String) -> Result<(), String> {
  preview::set_grid(grid);
  Ok(())
}

#[tauri::command]
fn reset_preview_zoom() -> Result<(), String> {
  preview::reset_zoom();
  Ok(())
}

#[tauri::command]
async fn set_theme(theme: String) -> Result<(), String> {
  with_config(|c| c.theme = theme);
  persist_config();
  Ok(())
}

/// "original" o "720p". Si ya estas grabando no aplica hasta que hagas
/// stop_recording + start_recording (obs_reset_video no se puede llamar con
/// el video activo).
#[tauri::command]
async fn set_resolution(resolution: String) -> Result<(), String> {
  with_config(|c| c.resolution = resolution);
  persist_config();
  Ok(())
}

/// "auto" o el id de un encoder puntual (ver VIDEO_ENCODER_CANDIDATES). Si
/// ya estas grabando no aplica hasta que hagas stop_recording +
/// start_recording, igual que resolucion/fps.
#[tauri::command]
async fn set_video_encoder(encoder: String) -> Result<(), String> {
  with_config(|c| c.video_encoder = encoder);
  persist_config();
  Ok(())
}

#[tauri::command]
async fn set_fps(fps: i64) -> Result<(), String> {
  with_config(|c| c.fps = fps);
  persist_config();
  Ok(())
}

fn cleanup_before_exit() {
  let _ = stop_recording_impl();
  unsafe {
    let preview_opt = { PREVIEW_STATE.lock().unwrap().take() };
    if let Some(state) = preview_opt {
      preview::destroy(state);
    }
    let state_opt = { CAPTURE_STATE.lock().unwrap().take() };
    if let Some(state) = state_opt {
      release_capture_state(state);
    }
    if obs_ffi::obs_initialized() {
      obs_ffi::obs_shutdown();
    }
  }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  use tauri::menu::{Menu, MenuItem};
  use tauri::tray::TrayIconBuilder;
  use tauri::{Manager, WindowEvent};

  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .setup(|app| {
      let (tx, rx) = std::sync::mpsc::channel::<config::EmberioConfig>();
      let _ = CONFIG_SAVE_TX.set(tx);
      let app_handle_clone = app.handle().clone();
      std::thread::spawn(move || {
        while let Ok(cfg) = rx.recv() {
          let mut latest_cfg = cfg;
          while let Ok(next_cfg) = rx.try_recv() {
            latest_cfg = next_cfg;
          }
          config::save(&app_handle_clone, &latest_cfg);
        }
      });

      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      let _ = APP_HANDLE.set(app.handle().clone());
      let latest_cfg = config::load(app.handle());
      *CONFIG.lock().unwrap() = Some(latest_cfg.clone());

      // Inicializar el hilo de hotkey global nativo
      init_native_hotkey_thread();

      // Cargar los hotkeys guardados o registrarlos por defecto
      let loaded_hotkey = latest_cfg.hotkey;
      if let Some(h) = loaded_hotkey {
        register_native_hotkey(1, h.vk_code, h.ctrl, h.shift, h.alt);
      } else {
        register_native_hotkey(1, DEFAULT_HOTKEY_VK_F9, false, false, false);
      }

      let loaded_toggle_hotkey = latest_cfg.hotkey_toggle;
      if let Some(h) = loaded_toggle_hotkey {
        register_native_hotkey(2, h.vk_code, h.ctrl, h.shift, h.alt);
      } else {
        register_native_hotkey(2, DEFAULT_TOGGLE_HOTKEY_VK_F10, false, false, false);
      }

      // Inicializar el indicador de estado permanente en la esquina inferior derecha (inicia en rojo)
      show_status_notification(app.handle(), false);

      // Inicializar libobs y arrancar la captura en segundo plano
      tauri::async_runtime::spawn(async {
        unsafe {
          if let Err(e) = ensure_obs_platform_initialized() {
            log::error!("Error al inicializar libobs en setup: {}", e);
          } else if let Err(e) = ensure_capture_started() {
            log::error!("Error al iniciar captura en setup: {}", e);
          }
        }
      });

      // Emisor periodico de niveles de audio para el mixer en vivo -- lee
      // AUDIO_LEVELS (que van llenando los callbacks de volmeter) y la
      // manda al frontend. Corre siempre; si no hay fuentes activas manda
      // un array vacio y no hace nada.
      tauri::async_runtime::spawn(async {
        loop {
          tokio::time::sleep(std::time::Duration::from_millis(100)).await;
          let levels = AUDIO_LEVELS.lock().unwrap().clone();
          if let Some(app) = APP_HANDLE.get() {
            let _ = app.emit("audio-levels", levels);
          }
        }
      });

      let show_item = MenuItem::with_id(app, "show", "Mostrar Ember", true, None::<&str>)?;
      let quit_item = MenuItem::with_id(app, "quit", "Salir (corta la grabacion)", true, None::<&str>)?;
      let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

      let icon = match app.default_window_icon() {
        Some(icon) => icon.clone(),
        None => {
          tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
            .unwrap_or_else(|_| tauri::image::Image::new(&[0, 0, 0, 0], 1, 1))
        }
      };

      TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Ember")
        .on_menu_event(|app, event| match event.id.as_ref() {
          "show" => {
            if let Some(window) = app.get_webview_window("main") {
              let _ = window.show();
              let _ = window.set_focus();
              unsafe {
                if let Some(state) = &*PREVIEW_STATE.lock().unwrap() {
                  preview::set_visible(state, true);
                }
              }
            }
          }
          "quit" => {
            app.exit(0);
          }
          _ => {}
        })
        .build(app)?;

      Ok(())
    })
    .on_window_event(|window, event| {
      // Cerrar la ventana (la X) la oculta en vez de matar el proceso --
      // asi la grabacion sigue en pie con solo el hotkey, sin necesitar la
      // ventana abierta. Salir de verdad es via el menu de la bandeja.
      if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
        // El preview es una ventana "owned" (WS_POPUP) aparte del webview --
        // ocultar la ventana principal no la oculta sola, asi que quedaba
        // pegada en pantalla. La escondemos a mano junto con la ventana.
        unsafe {
          if let Some(state) = &*PREVIEW_STATE.lock().unwrap() {
            preview::set_visible(state, false);
          }
        }
      }
    })
    .invoke_handler(tauri::generate_handler![
      get_obs_version,
      start_recording,
      stop_recording,
      save_clip_now,
      set_save_hotkey,
      set_toggle_hotkey,
      set_clips_dir,
      get_config,
      list_video_encoders,
      set_video_encoder,
      list_monitors,
      list_audio_output_devices,
      list_audio_input_devices,
      set_monitor,
      set_video_source,
      list_windows,
      list_games,
      set_overlay_locked,
      reorder_overlay,
      set_audio_source_label,
      add_audio_source,
      remove_audio_source,
      set_audio_source_volume,
      set_audio_source_muted,
      set_theme,
      set_resolution,
      set_fps,
      list_overlays,
      add_image_overlay,
      add_text_overlay,
      add_browser_overlay,
      remove_overlay,
      set_overlay_visible,
      set_overlay_transform,
      set_overlay_opacity,
      toggle_preview,
      update_preview_rect,
      get_canvas_size,
      set_preview_grid,
      reset_preview_zoom
    ])
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|_app_handle, event| match event {
      tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => cleanup_before_exit(),
      _ => {}
    });
}
