mod obs_ffi;

use std::ffi::{CStr, CString};
use std::path::PathBuf;

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
    .invoke_handler(tauri::generate_handler![get_obs_version, obs_init])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
