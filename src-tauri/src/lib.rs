mod obs_ffi;

use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Mutex;

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

static CAPTURE_STATE: Mutex<Option<CaptureState>> = Mutex::new(None);

struct OutputState {
  output: RawPtr<obs_ffi::obs_output_t>,
  video_encoder: RawPtr<obs_ffi::obs_encoder_t>,
  audio_encoder: RawPtr<obs_ffi::obs_encoder_t>,
}

static OUTPUT_STATE: Mutex<Option<OutputState>> = Mutex::new(None);

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

/// Idem pero devuelve el string de salida "path" (usado por
/// "get_last_replay"). Devuelve None si el proc no puso nada (replay
/// buffer todavia muxeando el clip a disco).
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

fn app_dir() -> PathBuf {
  std::env::current_exe()
    .expect("no se pudo resolver el ejecutable actual")
    .parent()
    .expect("el ejecutable no tiene directorio padre")
    .to_path_buf()
}

/// Paso 1: arrancar libobs, cargar los plugins (win-capture, win-wasapi,
/// obs-ffmpeg, obs-nvenc/obs-x264) y configurar video+audio base. Todavia no
/// crea escena ni arranca el replay buffer -- eso es el siguiente paso.
#[tauri::command]
fn obs_init() -> Result<String, String> {
  unsafe {
    if obs_ffi::obs_initialized() {
      return Ok("libobs ya estaba inicializado".into());
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

    let data_root = base
      .join("../../data")
      .to_string_lossy()
      .replace('\\', "/");
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
    let audio_ok = obs_ffi::obs_reset_audio(&oai as *const _);
    if !audio_ok {
      return Err("obs_reset_audio fallo".into());
    }

    Ok("libobs inicializado: modulos cargados, video 1920x1080@30 y audio 48kHz estereo OK".into())
  }
}

/// Paso 2: crear una escena con captura de monitor (duplicator/DXGI, id
/// "monitor_capture") + audio de escritorio (win-wasapi, id
/// "wasapi_output_capture"), y ponerlas activas (canal 0 = video principal,
/// canal 1 = audio global). Todavia no arranca el replay buffer.
#[tauri::command]
fn obs_start_capture() -> Result<String, String> {
  unsafe {
    if !obs_ffi::obs_initialized() {
      return Err("Llama a obs_init primero".into());
    }

    {
      let guard = CAPTURE_STATE.lock().unwrap();
      if guard.is_some() {
        return Ok("La captura ya estaba armada".into());
      }
    }

    let scene_name = CString::new("Emberio Scene").unwrap();
    let scene = obs_ffi::obs_scene_create(scene_name.as_ptr());
    if scene.is_null() {
      return Err("obs_scene_create devolvio null".into());
    }
    let scene_source = obs_ffi::obs_scene_get_source(scene);

    let monitor_id = CString::new("monitor_capture").unwrap();
    let monitor_name = CString::new("Monitor").unwrap();
    let monitor_source = obs_ffi::obs_source_create(
      monitor_id.as_ptr(),
      monitor_name.as_ptr(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    );
    if monitor_source.is_null() {
      obs_ffi::obs_scene_release(scene);
      return Err("obs_source_create('monitor_capture') devolvio null".into());
    }
    obs_ffi::obs_scene_add(scene, monitor_source);

    let audio_id = CString::new("wasapi_output_capture").unwrap();
    let audio_name = CString::new("Audio de escritorio").unwrap();
    let audio_source = obs_ffi::obs_source_create(
      audio_id.as_ptr(),
      audio_name.as_ptr(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    );
    if audio_source.is_null() {
      obs_ffi::obs_source_release(monitor_source);
      obs_ffi::obs_scene_release(scene);
      return Err("obs_source_create('wasapi_output_capture') devolvio null".into());
    }

    obs_ffi::obs_set_output_source(0, scene_source);
    obs_ffi::obs_set_output_source(1, audio_source);

    let monitor_active = obs_ffi::obs_source_active(monitor_source);
    let audio_active = obs_ffi::obs_source_active(audio_source);

    *CAPTURE_STATE.lock().unwrap() = Some(CaptureState {
      scene: RawPtr(scene),
      monitor_source: RawPtr(monitor_source),
      audio_source: RawPtr(audio_source),
    });

    Ok(format!(
      "Escena creada. monitor_capture activo={monitor_active}, wasapi_output_capture activo={audio_active}"
    ))
  }
}

/// Paso 3: crear encoders (NVENC h264 + AAC), armar el output "replay_buffer"
/// (buffer circular de los ultimos N segundos ya encodeados) y arrancarlo.
/// Requiere que obs_start_capture ya haya corrido (necesita la escena activa
/// en el canal 0 para tener algo que encodear).
#[tauri::command]
fn obs_setup_output() -> Result<String, String> {
  unsafe {
    if CAPTURE_STATE.lock().unwrap().is_none() {
      return Err("Llama a obs_start_capture primero".into());
    }
    if OUTPUT_STATE.lock().unwrap().is_some() {
      return Ok("El output ya estaba armado".into());
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
    let audio_encoder = obs_ffi::obs_audio_encoder_create(
      aenc_id.as_ptr(),
      aenc_name.as_ptr(),
      std::ptr::null_mut(),
      0,
      std::ptr::null_mut(),
    );
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
    obs_ffi::obs_data_set_int(settings, max_time_key.as_ptr(), 60);
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
      return Err(format!("obs_output_start fallo: {err}"));
    }

    *OUTPUT_STATE.lock().unwrap() = Some(OutputState {
      output: RawPtr(output),
      video_encoder: RawPtr(video_encoder),
      audio_encoder: RawPtr(audio_encoder),
    });

    Ok(format!(
      "Replay buffer arrancado. Guarda en: {}",
      clips_dir.to_string_lossy()
    ))
  }
}

/// Paso 4: guardar el clip (ultimos N segundos bufferizados). El guardado
/// real es asincrono (un thread interno remuxea a disco), asi que
/// disparamos "save" y esperamos a que "get_last_replay" devuelva una ruta.
#[tauri::command]
async fn obs_save_clip() -> Result<String, String> {
  // Envuelto en RawPtr (Send+Sync a mano) porque un *mut crudo no es Send,
  // y este valor queda "vivo" del otro lado de un .await.
  let output_ptr: RawPtr<obs_ffi::obs_output_t> = {
    let guard = OUTPUT_STATE.lock().unwrap();
    match &*guard {
      Some(state) => RawPtr(state.output.0),
      None => return Err("Llama a obs_setup_output primero".into()),
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .setup(|app| {
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
      get_obs_version,
      obs_init,
      obs_start_capture,
      obs_setup_output,
      obs_save_clip
    ])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
