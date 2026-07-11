mod config;
mod obs_ffi;
mod preview;

use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager};

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
static SAVE_HOTKEY_ID: Mutex<Option<obs_ffi::obs_hotkey_id>> = Mutex::new(None);
static CONFIG: Mutex<Option<config::EmberioConfig>> = Mutex::new(None);
/// Nivel de audio en vivo (0.0..=1.0) por indice de fuente, actualizado por
/// los callbacks de volmeter y emitido periodicamente al frontend.
static AUDIO_LEVELS: Mutex<Vec<f32>> = Mutex::new(Vec::new());
static OBS_INIT_LOCK: Mutex<()> = Mutex::new(());


const DEFAULT_HOTKEY_VK_F9: i32 = 0x78; // VK_F9

#[link(name = "user32")]
extern "system" {
  fn GetSystemMetrics(n_index: i32) -> i32;
}
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

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
  if let Some(app) = APP_HANDLE.get() {
    let cfg = CONFIG.lock().unwrap().clone().unwrap_or_default();
    config::save(app, &cfg);
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
  let props = obs_ffi::obs_get_source_properties(id_c.as_ptr());
  if props.is_null() {
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

  Ok(())
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
  // find_libobs_data_file). Fijamos el CWD al directorio del .exe para que
  // esa cuenta de "subir 2 niveles" de siempre en el mismo lugar
  // (build.rs copia data/ ahi: target/<profile>/../../data).
  std::env::set_current_dir(&base).map_err(|e| format!("no se pudo fijar el directorio de trabajo: {e}"))?;

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

  let data_root = base.join("../../data").to_string_lossy().replace('\\', "/");
  let bin_path = CString::new(base.join("obs-plugins/64bit").to_string_lossy().replace('\\', "/")).unwrap();
  let data_path = CString::new(format!("{data_root}/obs-plugins/%module%")).unwrap();
  obs_ffi::obs_add_module_path(bin_path.as_ptr(), data_path.as_ptr());
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
      if source_plugin == "game_capture" {
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
  obs_ffi::obs_scene_add(scene, capture_source);

  // 4. Agregar fuentes de audio (wasapi desktop y microfonos)
  let mut audio_sources = Vec::new();
  let mut audio_volmeters = Vec::new();
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

  *CAPTURE_STATE.lock().unwrap() = Some(CaptureState {
    scene: RawPtr(scene),
    capture_source: RawPtr(capture_source),
    audio_sources: audio_sources.into_iter().map(RawPtr).collect(),
    audio_volmeters: audio_volmeters.into_iter().map(RawPtr).collect(),
    overlays,
  });

  Ok(())
}

/// Crea la fuente (imagen o texto) + el item de escena para un overlay, y le
/// aplica su transform/visibilidad. Se usa tanto al recrear overlays
/// persistidos como al agregar uno nuevo en caliente.
unsafe fn create_overlay_item(scene: *mut obs_ffi::obs_scene_t, cfg: &config::OverlayConfig) -> Result<OverlayItem, String> {
  let (source_id, settings_key) = match cfg.kind {
    config::OverlayKind::Image => ("image_source", "file"),
    config::OverlayKind::Text => ("text_ft2_source", "text"),
  };
  let id_c = CString::new(source_id).unwrap();
  let name_c = CString::new(format!("Overlay ({source_id})")).unwrap();
  let settings = single_string_settings(settings_key, &Some(cfg.content.clone()));
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
  obs_ffi::obs_sceneitem_set_scale(item, &vec2_of(cfg.scale, cfg.scale) as *const _);
  obs_ffi::obs_sceneitem_set_visible(item, cfg.visible);
  obs_ffi::obs_sceneitem_set_locked(item, cfg.locked);

  Ok(OverlayItem { source: RawPtr(source), item: RawPtr(item) })
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

  let venc_id = CString::new("obs_nvenc_h264_tex").unwrap();
  let venc_name = CString::new("Ember Video Encoder").unwrap();
  let video_encoder =
    obs_ffi::obs_video_encoder_create(venc_id.as_ptr(), venc_name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
  if video_encoder.is_null() {
    return Err("obs_video_encoder_create('obs_nvenc_h264_tex') devolvio null (¿GPU sin NVENC?)".into());
  }
  obs_ffi::obs_encoder_set_video(video_encoder, obs_ffi::obs_get_video());

  let aenc_id = CString::new("ffmpeg_aac").unwrap();
  let aenc_name = CString::new("Ember Audio Encoder").unwrap();
  let audio_encoder =
    obs_ffi::obs_audio_encoder_create(aenc_id.as_ptr(), aenc_name.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut());
  if audio_encoder.is_null() {
    obs_ffi::obs_encoder_release(video_encoder);
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
    obs_ffi::obs_encoder_release(video_encoder);
    obs_ffi::obs_encoder_release(audio_encoder);
    return Err("obs_output_create('replay_buffer') devolvio null".into());
  }

  obs_ffi::obs_output_set_video_encoder(output, video_encoder);
  obs_ffi::obs_output_set_audio_encoder(output, audio_encoder, 0);

  let started = obs_ffi::obs_output_start(output);
  if !started {
    let err_ptr = obs_ffi::obs_output_get_last_error(output);
    let err = if err_ptr.is_null() {
      "razon desconocida".to_string()
    } else {
      CStr::from_ptr(err_ptr).to_string_lossy().into_owned()
    };
    obs_ffi::obs_encoder_release(video_encoder);
    obs_ffi::obs_encoder_release(audio_encoder);
    obs_ffi::obs_output_release(output);
    return Err(format!("obs_output_start fallo: {err}"));
  }

  *OUTPUT_STATE.lock().unwrap() = Some(OutputState {
    output: RawPtr(output),
    video_encoder: RawPtr(video_encoder),
    audio_encoder: RawPtr(audio_encoder),
    max_time_sec: clip_seconds,
  });

  Ok(clips_dir)
}

// ---------------------------------------------------------------------------
// Hotkey de "guardar clip" (global, vale aunque Emberio no tenga foco --
// libobs corre un thread propio que pollea el estado de teclado del sistema)
// ---------------------------------------------------------------------------

extern "C" fn save_hotkey_callback(
  _data: *mut c_void,
  _id: obs_ffi::obs_hotkey_id,
  _hotkey: *mut obs_ffi::obs_hotkey_t,
  pressed: bool,
) {
  if !pressed {
    return;
  }
  tauri::async_runtime::spawn(save_clip_and_notify());
}

unsafe fn ensure_save_hotkey_registered() -> obs_ffi::obs_hotkey_id {
  if let Some(id) = *SAVE_HOTKEY_ID.lock().unwrap() {
    return id;
  }

  let name = CString::new("emberio.save_clip").unwrap();
  let desc = CString::new("Ember: guardar clip").unwrap();
  let id = obs_ffi::obs_hotkey_register_frontend(
    name.as_ptr(),
    desc.as_ptr(),
    Some(save_hotkey_callback),
    std::ptr::null_mut(),
  );

  let saved = with_config(|c| c.hotkey);
  let (vk_code, modifiers) = match saved {
    Some(h) => {
      let mut m: u32 = 0;
      if h.ctrl {
        m |= obs_ffi::obs_interaction_flags_INTERACT_CONTROL_KEY as u32;
      }
      if h.shift {
        m |= obs_ffi::obs_interaction_flags_INTERACT_SHIFT_KEY as u32;
      }
      if h.alt {
        m |= obs_ffi::obs_interaction_flags_INTERACT_ALT_KEY as u32;
      }
      (h.vk_code, m)
    }
    None => (DEFAULT_HOTKEY_VK_F9, 0),
  };

  let mut combo = obs_ffi::obs_key_combination {
    modifiers,
    key: obs_ffi::obs_key_from_virtual_key(vk_code),
  };
  obs_ffi::obs_hotkey_load_bindings(id, &mut combo as *mut _, 1);

  *SAVE_HOTKEY_ID.lock().unwrap() = Some(id);
  id
}

/// Guarda el clip y avisa al frontend via evento (asi el boton y el hotkey
/// global comparten el mismo camino y la UI se entera de los dos).
async fn save_clip_and_notify() {
  let result = save_clip_internal().await;
  if let Some(app) = APP_HANDLE.get() {
    match result {
      Ok(path) => {
        let _ = app.emit("clip-saved", path);
      }
      Err(err) => {
        let _ = app.emit("clip-save-error", err);
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
#[tauri::command]
fn start_recording(clip_seconds: i64) -> Result<String, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    // Solo reaplicamos video/FPS/resolucion si el subsistema de video de OBS no esta activo.
    // obs_reset_video falla/crashea si hay grabacion o PREVIEW en curso.
    if !obs_ffi::obs_video_active() {
      apply_video_settings()?;
    }
    ensure_capture_started()?;
    let clips_dir = ensure_output_started(clip_seconds)?;
    ensure_save_hotkey_registered();

    with_config(|c| c.clip_seconds = clip_seconds);
    persist_config();

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
  let preview_active = PREVIEW_STATE.lock().unwrap().is_some();
  let recording_active = OUTPUT_STATE.lock().unwrap().is_some();
  if !preview_active && !recording_active {
    if let Some(state) = CAPTURE_STATE.lock().unwrap().take() {
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
        obs_ffi::obs_source_release(overlay.source.0);
      }
      obs_ffi::obs_scene_release(state.scene.0);
    }
    AUDIO_LEVELS.lock().unwrap().clear();
  }
}

#[tauri::command]
fn stop_recording() -> Result<String, String> {
  unsafe {
    if let Some(state) = OUTPUT_STATE.lock().unwrap().take() {
      obs_ffi::obs_output_stop(state.output.0);
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
  unsafe {
    if !obs_ffi::obs_initialized() {
      return Err("Llama a start_recording primero (libobs no esta inicializado)".into());
    }
    let id = ensure_save_hotkey_registered();

    let mut modifiers: u32 = 0;
    if ctrl {
      modifiers |= obs_ffi::obs_interaction_flags_INTERACT_CONTROL_KEY as u32;
    }
    if shift {
      modifiers |= obs_ffi::obs_interaction_flags_INTERACT_SHIFT_KEY as u32;
    }
    if alt {
      modifiers |= obs_ffi::obs_interaction_flags_INTERACT_ALT_KEY as u32;
    }

    let key = obs_ffi::obs_key_from_virtual_key(vk_code);
    let mut combo = obs_ffi::obs_key_combination { modifiers, key };
    obs_ffi::obs_hotkey_load_bindings(id, &mut combo as *mut _, 1);

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

/// Lista de pantallas disponibles (mismo mecanismo que usa la ventana de
/// configuracion de OBS: EnumDisplayMonitors corre en vivo al pedir las
/// propiedades de "monitor_capture").
#[tauri::command]
fn list_monitors() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("monitor_capture", "monitor_id"))
  }
}

#[tauri::command]
fn list_audio_output_devices() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("wasapi_output_capture", "device_id"))
  }
}

#[tauri::command]
fn list_audio_input_devices() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("wasapi_input_capture", "device_id"))
  }
}

/// Elegir que pantalla capturar.
#[tauri::command]
fn set_monitor(id: String) -> Result<String, String> {
  set_video_source("screen".to_string(), id)?;
  Ok("Pantalla de captura configurada.".into())
}

#[tauri::command]
fn set_video_source(source_type: String, source_id: String) -> Result<(), String> {
  with_config(|c| {
    c.video_source_type = source_type.clone();
    c.video_source_id = Some(source_id.clone());
  });
  persist_config();

  unsafe {
    let mut guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
      let scene = state.scene.0;
      
      // Eliminar la fuente vieja
      let old_source = state.capture_source.0;
      let name_c = obs_ffi::obs_source_get_name(old_source);
      let old_item = obs_ffi::obs_scene_find_source_recursive(scene, name_c);
      if !old_item.is_null() {
        obs_ffi::obs_sceneitem_remove(old_item);
      }

      // Crear nueva fuente
      let source_plugin = match source_type.as_str() {
        "window" => "window_capture",
        "game" => "game_capture",
        _ => "monitor_capture",
      };

      let plugin_c = CString::new(source_plugin).unwrap();
      let name_c = CString::new("Captura Principal").unwrap();
      let settings = obs_ffi::obs_data_create();
      let val_c = CString::new(source_id.as_str()).unwrap();

      if source_plugin == "monitor_capture" {
        let key_c = CString::new("monitor_id").unwrap();
        obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
      } else {
        let key_c = CString::new("window").unwrap();
        obs_ffi::obs_data_set_string(settings, key_c.as_ptr(), val_c.as_ptr());
        if source_plugin == "game_capture" {
          let mode_key = CString::new("capture_mode").unwrap();
          let mode_val = CString::new("window").unwrap();
          obs_ffi::obs_data_set_string(settings, mode_key.as_ptr(), mode_val.as_ptr());
        }
      }

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

      // Ubicar en el fondo (índice 0)
      obs_ffi::obs_sceneitem_set_order_position(new_item, 0);

      // Liberar fuente antigua y guardar la nueva
      obs_ffi::obs_source_release(old_source);
      state.capture_source = RawPtr(new_source);
    }
  }
  Ok(())
}

#[tauri::command]
fn list_windows() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("window_capture", "window"))
  }
}

#[tauri::command]
fn list_games() -> Result<Vec<PropertyOption>, String> {
  unsafe {
    ensure_obs_platform_initialized()?;
    Ok(list_property_options("game_capture", "window"))
  }
}

#[tauri::command]
fn set_overlay_locked(index: usize, locked: bool) -> Result<Vec<OverlayInfo>, String> {
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
fn reorder_overlay(index: usize, up: bool) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    let mut guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
      let len = state.overlays.len();
      let target_index = if up {
        if index + 1 < len { index + 1 } else { return Err("Ya está al frente".into()); }
      } else {
        if index > 0 { index - 1 } else { return Err("Ya está al fondo".into()); }
      };
      
      state.overlays.swap(index, target_index);
      
      let items_raw: Vec<*mut obs_ffi::obs_sceneitem_t> = state.overlays.iter().map(|item| item.item.0).collect();
      obs_ffi::obs_scene_reorder_items(state.scene.0, items_raw.as_ptr() as *const _, items_raw.len());
    }
  }

  with_config(|c| {
    let len = c.overlays.len();
    let target_index = if up {
      if index + 1 < len { index + 1 } else { index }
    } else {
      if index > 0 { index - 1 } else { index }
    };
    c.overlays.swap(index, target_index);
  });
  persist_config();

  Ok(overlay_info_list())
}

#[tauri::command]
fn set_audio_source_label(index: usize, label: String) -> Result<Vec<config::AudioSourceConfig>, String> {
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
fn add_audio_source(kind: String, device_id: String, label: String) -> Result<Vec<config::AudioSourceConfig>, String> {
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
fn remove_audio_source(index: usize) -> Result<Vec<config::AudioSourceConfig>, String> {
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
fn set_audio_source_volume(index: usize, volume: f32) -> Result<Vec<config::AudioSourceConfig>, String> {
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
fn set_audio_source_muted(index: usize, muted: bool) -> Result<Vec<config::AudioSourceConfig>, String> {
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
  visible: bool,
  locked: bool,
}

fn overlay_kind_str(kind: config::OverlayKind) -> &'static str {
  match kind {
    config::OverlayKind::Image => "image",
    config::OverlayKind::Text => "text",
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
        visible: o.visible,
        locked: o.locked,
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
  CAPTURE_STATE.lock().unwrap().as_mut().unwrap().overlays.push(item);
  with_config(|c| c.overlays.push(cfg));
  persist_config();
  Ok(overlay_info_list())
}

/// Agrega un overlay de imagen (logo, marco, etc). `path` es una ruta de
/// archivo absoluta (el frontend la saca del dialogo nativo de archivos).
#[tauri::command]
fn add_image_overlay(path: String) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    add_overlay(config::OverlayConfig {
      kind: config::OverlayKind::Image,
      content: path,
      x: 100.0,
      y: 100.0,
      scale: 1.0,
      visible: true,
      locked: false,
    })
  }
}

/// Agrega un overlay de texto simple (fuente/color por defecto de libobs).
#[tauri::command]
fn add_text_overlay(text: String) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    add_overlay(config::OverlayConfig {
      kind: config::OverlayKind::Text,
      content: text,
      x: 100.0,
      y: 100.0,
      scale: 1.0,
      visible: true,
      locked: false,
    })
  }
}

#[tauri::command]
fn remove_overlay(index: usize) -> Result<Vec<OverlayInfo>, String> {
  unsafe {
    let mut guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
      if index < state.overlays.len() {
        let item = state.overlays.remove(index);
        obs_ffi::obs_sceneitem_remove(item.item.0);
        obs_ffi::obs_source_release(item.source.0);
      }
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

#[tauri::command]
fn set_overlay_visible(index: usize, visible: bool) -> Result<Vec<OverlayInfo>, String> {
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
  Ok(overlay_info_list())
}

/// Reposiciona/escala un overlay ya creado (posicion en pixeles del canvas
/// base, escala uniforme 0.1..=5.0).
#[tauri::command]
fn set_overlay_transform(index: usize, x: f32, y: f32, scale: f32) -> Result<Vec<OverlayInfo>, String> {
  {
    let guard = CAPTURE_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      if let Some(item) = state.overlays.get(index) {
        unsafe {
          obs_ffi::obs_sceneitem_set_pos(item.item.0, &vec2_of(x, y) as *const _);
          obs_ffi::obs_sceneitem_set_scale(item.item.0, &vec2_of(scale, scale) as *const _);
        }
      }
    }
  }
  with_config(|c| {
    if let Some(o) = c.overlays.get_mut(index) {
      o.x = x;
      o.y = y;
      o.scale = scale;
    }
  });
  persist_config();
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
    } else if let Some(state) = PREVIEW_STATE.lock().unwrap().take() {
      preview::destroy(state);
      cleanup_capture_if_idle();
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
fn set_theme(theme: String) -> Result<(), String> {
  with_config(|c| c.theme = theme);
  persist_config();
  Ok(())
}

/// "original" o "720p". Si ya estas grabando no aplica hasta que hagas
/// stop_recording + start_recording (obs_reset_video no se puede llamar con
/// el video activo).
#[tauri::command]
fn set_resolution(resolution: String) -> Result<(), String> {
  with_config(|c| c.resolution = resolution);
  persist_config();
  Ok(())
}

#[tauri::command]
fn set_fps(fps: i64) -> Result<(), String> {
  with_config(|c| c.fps = fps);
  persist_config();
  Ok(())
}

fn cleanup_before_exit() {
  let _ = stop_recording();
  unsafe {
    if let Some(state) = PREVIEW_STATE.lock().unwrap().take() {
      preview::destroy(state);
    }
    if let Some(state) = CAPTURE_STATE.lock().unwrap().take() {
      obs_ffi::obs_set_output_source(0, std::ptr::null_mut());
      for i in 1..=MAX_AUDIO_SOURCES {
        obs_ffi::obs_set_output_source(i as u32, std::ptr::null_mut());
      }
      for volmeter in state.audio_volmeters {
        if !volmeter.0.is_null() {
          obs_ffi::obs_volmeter_detach_source(volmeter.0);
          obs_ffi::obs_volmeter_destroy(volmeter.0);
        }
      }
      obs_ffi::obs_source_release(state.capture_source.0);
      for source in state.audio_sources {
        obs_ffi::obs_source_release(source.0);
      }
      for overlay in state.overlays {
        obs_ffi::obs_source_release(overlay.source.0);
      }
      obs_ffi::obs_scene_release(state.scene.0);
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
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      let _ = APP_HANDLE.set(app.handle().clone());
      *CONFIG.lock().unwrap() = Some(config::load(app.handle()));

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
      }
    })
    .invoke_handler(tauri::generate_handler![
      get_obs_version,
      start_recording,
      stop_recording,
      save_clip_now,
      set_save_hotkey,
      set_clips_dir,
      get_config,
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
      remove_overlay,
      set_overlay_visible,
      set_overlay_transform,
      toggle_preview,
      update_preview_rect,
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
