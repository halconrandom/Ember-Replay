mod obs_ffi;

use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

/// Wrapper para poder guardar punteros crudos de libobs en un `static`.
/// Seguro en la practica: libobs es thread-safe para estas operaciones y
/// nosotros solo los tocamos desde comandos de Tauri (nunca en paralelo
/// real sobre el mismo puntero).
struct RawPtr<T>(*mut T);
unsafe impl<T> Send for RawPtr<T> {}
unsafe impl<T> Sync for RawPtr<T> {}

struct CaptureState {
  scene: RawPtr<obs_ffi::obs_scene_t>,
  monitor_source: RawPtr<obs_ffi::obs_source_t>,
  audio_source: RawPtr<obs_ffi::obs_source_t>,
}

struct OutputState {
  output: RawPtr<obs_ffi::obs_output_t>,
  video_encoder: RawPtr<obs_ffi::obs_encoder_t>,
  audio_encoder: RawPtr<obs_ffi::obs_encoder_t>,
  max_time_sec: i64,
}

static CAPTURE_STATE: Mutex<Option<CaptureState>> = Mutex::new(None);
static OUTPUT_STATE: Mutex<Option<OutputState>> = Mutex::new(None);
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
static SAVE_HOTKEY_ID: Mutex<Option<obs_ffi::obs_hotkey_id>> = Mutex::new(None);

const DEFAULT_HOTKEY_VK_F9: i32 = 0x78; // VK_F9

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
// Inicializacion (idempotente)
// ---------------------------------------------------------------------------

unsafe fn ensure_obs_initialized() -> Result<(), String> {
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

  let data_root = base.join("../../data").to_string_lossy().replace('\\', "/");
  let bin_path = CString::new(base.join("obs-plugins/64bit").to_string_lossy().replace('\\', "/")).unwrap();
  let data_path = CString::new(format!("{data_root}/obs-plugins/%module%")).unwrap();
  obs_ffi::obs_add_module_path(bin_path.as_ptr(), data_path.as_ptr());
  obs_ffi::obs_load_all_modules();
  obs_ffi::obs_post_load_modules();

  let graphics_module = CString::new("libobs-d3d11").unwrap();
  let mut ovi = obs_ffi::obs_video_info {
    graphics_module: graphics_module.as_ptr(),
    fps_num: 30,
    fps_den: 1,
    base_width: 1920,
    base_height: 1080,
    output_width: 1920,
    output_height: 1080,
    output_format: obs_ffi::video_format_VIDEO_FORMAT_NV12,
    adapter: 0,
    gpu_conversion: true,
    colorspace: obs_ffi::video_colorspace_VIDEO_CS_DEFAULT,
    range: obs_ffi::video_range_type_VIDEO_RANGE_DEFAULT,
    scale_type: obs_ffi::obs_scale_type_OBS_SCALE_DISABLE,
  };
  let video_result = obs_ffi::obs_reset_video(&mut ovi as *mut _);
  if video_result != obs_ffi::OBS_VIDEO_SUCCESS as i32 {
    return Err(format!("obs_reset_video fallo, codigo {video_result}"));
  }

  let oai = obs_ffi::obs_audio_info {
    samples_per_sec: 48000,
    speakers: obs_ffi::speaker_layout_SPEAKERS_STEREO,
  };
  if !obs_ffi::obs_reset_audio(&oai as *const _) {
    return Err("obs_reset_audio fallo".into());
  }

  Ok(())
}

unsafe fn ensure_capture_started() -> Result<(), String> {
  if CAPTURE_STATE.lock().unwrap().is_some() {
    return Ok(());
  }

  let scene_name = CString::new("Emberio Scene").unwrap();
  let scene = obs_ffi::obs_scene_create(scene_name.as_ptr());
  if scene.is_null() {
    return Err("obs_scene_create devolvio null".into());
  }
  let scene_source = obs_ffi::obs_scene_get_source(scene);

  let monitor_id = CString::new("monitor_capture").unwrap();
  let monitor_name = CString::new("Monitor").unwrap();
  let monitor_source =
    obs_ffi::obs_source_create(monitor_id.as_ptr(), monitor_name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
  if monitor_source.is_null() {
    obs_ffi::obs_scene_release(scene);
    return Err("obs_source_create('monitor_capture') devolvio null".into());
  }
  obs_ffi::obs_scene_add(scene, monitor_source);

  let audio_id = CString::new("wasapi_output_capture").unwrap();
  let audio_name = CString::new("Audio de escritorio").unwrap();
  let audio_source =
    obs_ffi::obs_source_create(audio_id.as_ptr(), audio_name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
  if audio_source.is_null() {
    obs_ffi::obs_source_release(monitor_source);
    obs_ffi::obs_scene_release(scene);
    return Err("obs_source_create('wasapi_output_capture') devolvio null".into());
  }

  obs_ffi::obs_set_output_source(0, scene_source);
  obs_ffi::obs_set_output_source(1, audio_source);

  *CAPTURE_STATE.lock().unwrap() = Some(CaptureState {
    scene: RawPtr(scene),
    monitor_source: RawPtr(monitor_source),
    audio_source: RawPtr(audio_source),
  });

  Ok(())
}

unsafe fn ensure_output_started(clip_seconds: i64) -> Result<PathBuf, String> {
  {
    let guard = OUTPUT_STATE.lock().unwrap();
    if let Some(state) = &*guard {
      let _ = state.max_time_sec; // ya armado; ignoramos clip_seconds nuevo (hay que stop_recording primero)
      return Ok(app_dir().join("clips"));
    }
  }

  let venc_id = CString::new("obs_nvenc_h264_tex").unwrap();
  let venc_name = CString::new("Emberio Video Encoder").unwrap();
  let video_encoder =
    obs_ffi::obs_video_encoder_create(venc_id.as_ptr(), venc_name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
  if video_encoder.is_null() {
    return Err("obs_video_encoder_create('obs_nvenc_h264_tex') devolvio null (¿GPU sin NVENC?)".into());
  }
  obs_ffi::obs_encoder_set_video(video_encoder, obs_ffi::obs_get_video());

  let aenc_id = CString::new("ffmpeg_aac").unwrap();
  let aenc_name = CString::new("Emberio Audio Encoder").unwrap();
  let audio_encoder =
    obs_ffi::obs_audio_encoder_create(aenc_id.as_ptr(), aenc_name.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut());
  if audio_encoder.is_null() {
    obs_ffi::obs_encoder_release(video_encoder);
    return Err("obs_audio_encoder_create('ffmpeg_aac') devolvio null".into());
  }
  obs_ffi::obs_encoder_set_audio(audio_encoder, obs_ffi::obs_get_audio());

  let clips_dir = app_dir().join("clips");
  std::fs::create_dir_all(&clips_dir).map_err(|e| format!("no se pudo crear la carpeta de clips: {e}"))?;

  let settings = obs_ffi::obs_data_create();
  let dir_c = CString::new(clips_dir.to_string_lossy().replace('\\', "/")).unwrap();
  let dir_key = CString::new("directory").unwrap();
  obs_ffi::obs_data_set_string(settings, dir_key.as_ptr(), dir_c.as_ptr());
  let fmt_key = CString::new("format").unwrap();
  let fmt_val = CString::new("emberio-clip-%CCYY-%MM-%DD-%hh-%mm-%ss").unwrap();
  obs_ffi::obs_data_set_string(settings, fmt_key.as_ptr(), fmt_val.as_ptr());
  let ext_key = CString::new("extension").unwrap();
  let ext_val = CString::new("mp4").unwrap();
  obs_ffi::obs_data_set_string(settings, ext_key.as_ptr(), ext_val.as_ptr());
  let max_time_key = CString::new("max_time_sec").unwrap();
  obs_ffi::obs_data_set_int(settings, max_time_key.as_ptr(), clip_seconds);
  let max_size_key = CString::new("max_size_mb").unwrap();
  obs_ffi::obs_data_set_int(settings, max_size_key.as_ptr(), 1000);

  let output_id = CString::new("replay_buffer").unwrap();
  let output_name = CString::new("Emberio Replay Buffer").unwrap();
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
  let desc = CString::new("Emberio: guardar clip").unwrap();
  let id = obs_ffi::obs_hotkey_register_frontend(
    name.as_ptr(),
    desc.as_ptr(),
    Some(save_hotkey_callback),
    std::ptr::null_mut(),
  );

  let mut combo = obs_ffi::obs_key_combination {
    modifiers: 0,
    key: obs_ffi::obs_key_from_virtual_key(DEFAULT_HOTKEY_VK_F9),
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
    ensure_obs_initialized()?;
    ensure_capture_started()?;
    let clips_dir = ensure_output_started(clip_seconds)?;
    ensure_save_hotkey_registered();

    Ok(format!(
      "Grabando (buffer de {clip_seconds}s). Los clips se guardan en: {}",
      clips_dir.to_string_lossy()
    ))
  }
}

/// Corta la grabacion y libera todo lo creado (output, encoders, fuentes,
/// escena). libobs en si (obs_startup) queda inicializado para poder volver
/// a arrancar rapido con start_recording.
#[tauri::command]
fn stop_recording() -> Result<String, String> {
  unsafe {
    if let Some(state) = OUTPUT_STATE.lock().unwrap().take() {
      obs_ffi::obs_output_stop(state.output.0);
      obs_ffi::obs_encoder_release(state.video_encoder.0);
      obs_ffi::obs_encoder_release(state.audio_encoder.0);
    }
    if let Some(state) = CAPTURE_STATE.lock().unwrap().take() {
      obs_ffi::obs_set_output_source(0, std::ptr::null_mut());
      obs_ffi::obs_set_output_source(1, std::ptr::null_mut());
      obs_ffi::obs_source_release(state.monitor_source.0);
      obs_ffi::obs_source_release(state.audio_source.0);
      obs_ffi::obs_scene_release(state.scene.0);
    }
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

    let combo_desc = format!(
      "{}{}{}vk={vk_code}",
      if ctrl { "Ctrl+" } else { "" },
      if shift { "Shift+" } else { "" },
      if alt { "Alt+" } else { "" },
    );

    Ok(format!("Hotkey de guardado ahora es: {combo_desc}"))
  }
}

fn cleanup_before_exit() {
  let _ = stop_recording();
  unsafe {
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
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      let _ = APP_HANDLE.set(app.handle().clone());

      let show_item = MenuItem::with_id(app, "show", "Mostrar Emberio", true, None::<&str>)?;
      let quit_item = MenuItem::with_id(app, "quit", "Salir (corta la grabacion)", true, None::<&str>)?;
      let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

      TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Emberio")
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
      set_save_hotkey
    ])
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|_app_handle, event| match event {
      tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => cleanup_before_exit(),
      _ => {}
    });
}
