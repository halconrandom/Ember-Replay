//! Preview en vivo de la escena: crea una ventana hija nativa (Win32) sobre
//! el area que el frontend reserva para el preview, y le pide a libobs que
//! dibuje ahi via `obs_display` (swapchain D3D11 propio, aparte del canvas
//! de grabacion). Toggable a proposito -- crear/destruir el `obs_display`
//! es lo que evita el costo de renderizar el preview cuando no hace falta.

use crate::obs_ffi;
use std::ffi::c_void;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
  CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
  WNDCLASSW, WS_POPUP, WS_VISIBLE, CS_DBLCLKS,
};

/// Parámetros de vista y cuadrícula del preview.
pub struct PreviewParams {
  pub zoom: f32,
  pub pan_x: f32,
  pub pan_y: f32,
  pub grid: String,
}

pub static PREVIEW_PARAMS: Mutex<PreviewParams> = Mutex::new(PreviewParams {
  zoom: 1.0,
  pan_x: 0.0,
  pan_y: 0.0,
  grid: String::new(),
});

static LAST_MOUSE: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Resolucion base actual del canvas de OBS.
static CANVAS_SIZE: Mutex<(u32, u32)> = Mutex::new((1920, 1080));

pub fn set_canvas_size(w: u32, h: u32) {
  *CANVAS_SIZE.lock().unwrap() = (w, h);
}

pub struct PreviewState {
  hwnd: HWND,
  display: *mut obs_ffi::obs_display_t,
}
unsafe impl Send for PreviewState {}
unsafe impl Sync for PreviewState {}

fn emit_preview_params_change() {
  let params = PREVIEW_PARAMS.lock().unwrap();
  crate::emit_event("preview-params-changed", (
    params.zoom,
    params.pan_x,
    params.pan_y,
    params.grid.clone()
  ));
}

pub fn set_grid(grid: String) {
  let mut params = PREVIEW_PARAMS.lock().unwrap();
  params.grid = grid;
  drop(params);
  emit_preview_params_change();
}

pub fn reset_zoom() {
  let mut params = PREVIEW_PARAMS.lock().unwrap();
  params.zoom = 1.0;
  params.pan_x = 0.0;
  params.pan_y = 0.0;
  drop(params);
  emit_preview_params_change();
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
  match msg {
    windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN |
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN => {
      let x = (lparam & 0xffff) as i16 as i32;
      let y = ((lparam >> 16) & 0xffff) as i16 as i32;
      *LAST_MOUSE.lock().unwrap() = Some((x, y));
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP |
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MBUTTONUP => {
      *LAST_MOUSE.lock().unwrap() = None;
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE => {
      let wparam_u = wparam as usize;
      let left_down = (wparam_u & 0x0001) != 0;
      let middle_down = (wparam_u & 0x0010) != 0;
      let is_button_down = left_down || middle_down;
      
      let x = (lparam & 0xffff) as i16 as i32;
      let y = ((lparam >> 16) & 0xffff) as i16 as i32;
      
      if is_button_down {
        let mut last_mouse = LAST_MOUSE.lock().unwrap();
        if let Some((last_x, last_y)) = *last_mouse {
          let dx = x - last_x;
          let dy = y - last_y;
          if dx != 0 || dy != 0 {
            let mut params = PREVIEW_PARAMS.lock().unwrap();
            let scale_factor = params.zoom;
            params.pan_x += (dx as f32) / scale_factor;
            params.pan_y -= (dy as f32) / scale_factor; // Eje Y invertido (OBS es bottom-up)
            drop(params);
            
            *last_mouse = Some((x, y));
            emit_preview_params_change();
          }
        } else {
          *last_mouse = Some((x, y));
        }
      } else {
        *LAST_MOUSE.lock().unwrap() = None;
      }
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL => {
      let delta = ((wparam >> 16) & 0xffff) as i16;
      let mut params = PREVIEW_PARAMS.lock().unwrap();
      let factor = if delta > 0 { 1.15f32 } else { 1.0f32 / 1.15f32 };
      params.zoom = (params.zoom * factor).clamp(0.1, 20.0);
      drop(params);
      emit_preview_params_change();
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK => {
      reset_zoom();
      0
    }
    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
  }
}

fn class_name_wide() -> Vec<u16> {
  "EmberPreviewClass\0".encode_utf16().collect()
}

fn register_class_once() {
  static INIT: std::sync::Once = std::sync::Once::new();
  INIT.call_once(|| unsafe {
    let class_name = class_name_wide();
    let wc = WNDCLASSW {
      style: CS_DBLCLKS,
      lpfnWndProc: Some(wnd_proc),
      cbClsExtra: 0,
      cbWndExtra: 0,
      hInstance: GetModuleHandleW(std::ptr::null()),
      hIcon: std::ptr::null_mut(),
      hCursor: std::ptr::null_mut(),
      hbrBackground: std::ptr::null_mut(),
      lpszMenuName: std::ptr::null(),
      lpszClassName: class_name.as_ptr(),
    };
    RegisterClassW(&wc);
  });
}

unsafe fn vec4_of(r: f32, g: f32, b: f32, a: f32) -> obs_ffi::vec4 {
  let mut v: obs_ffi::vec4 = std::mem::zeroed();
  v.__bindgen_anon_1.ptr = [r, g, b, a];
  v
}

unsafe fn draw_grid_lines(base_w: u32, base_h: u32, grid_type: &str) {
  if grid_type == "none" || grid_type.is_empty() {
    return;
  }
  let effect = obs_ffi::obs_get_base_effect(obs_ffi::obs_base_effect_OBS_EFFECT_SOLID);
  if effect.is_null() {
    return;
  }
  let technique = obs_ffi::gs_effect_get_technique(effect, b"Solid\0".as_ptr() as *const _);
  let color_param = obs_ffi::gs_effect_get_param_by_name(effect, b"color\0".as_ptr() as *const _);
  if technique.is_null() || color_param.is_null() {
    return;
  }

  // Color de las líneas: gris claro semi-transparente
  let grid_color = vec4_of(0.4, 0.4, 0.4, 0.5);
  obs_ffi::gs_effect_set_vec4(color_param, &grid_color as *const _);

  let passes = obs_ffi::gs_technique_begin(technique);
  for i in 0..passes {
    obs_ffi::gs_technique_begin_pass(technique, i);
    obs_ffi::gs_render_start(true);

    // 1. Contorno del Canvas
    obs_ffi::gs_vertex2f(0.0, 0.0);
    obs_ffi::gs_vertex2f(base_w as f32, 0.0);

    obs_ffi::gs_vertex2f(base_w as f32, 0.0);
    obs_ffi::gs_vertex2f(base_w as f32, base_h as f32);

    obs_ffi::gs_vertex2f(base_w as f32, base_h as f32);
    obs_ffi::gs_vertex2f(0.0, base_h as f32);

    obs_ffi::gs_vertex2f(0.0, base_h as f32);
    obs_ffi::gs_vertex2f(0.0, 0.0);

    // 2. Líneas divisorias internas
    let divisions = match grid_type {
      "thirds" => 3,
      "grid10" => 10,
      "grid20" => 20,
      _ => 0,
    };

    if divisions > 1 {
      for d in 1..divisions {
        let frac = d as f32 / divisions as f32;
        // Línea vertical
        let x = base_w as f32 * frac;
        obs_ffi::gs_vertex2f(x, 0.0);
        obs_ffi::gs_vertex2f(x, base_h as f32);

        // Línea horizontal
        let y = base_h as f32 * frac;
        obs_ffi::gs_vertex2f(0.0, y);
        obs_ffi::gs_vertex2f(base_w as f32, y);
      }
    }

    obs_ffi::gs_render_draw(obs_ffi::gs_draw_mode_GS_LINES);
    obs_ffi::gs_technique_end_pass(technique);
  }
  obs_ffi::gs_technique_end(technique);
}

/// Dibuja la escena actual (lo mismo que ve la grabacion) escalada y
/// centrada dentro del rectangulo real de la ventana de preview, aplicando zoom,
/// paneo y cuadrícula de ayuda.
extern "C" fn render_preview_callback(_param: *mut c_void, cx: u32, cy: u32) {
  unsafe {
    let (base_w, base_h) = *CANVAS_SIZE.lock().unwrap();
    if base_w == 0 || base_h == 0 || cx == 0 || cy == 0 {
      return;
    }

    let scale = (cx as f32 / base_w as f32).min(cy as f32 / base_h as f32);
    let scaled_w = ((base_w as f32 * scale).round() as i32).max(1);
    let scaled_h = ((base_h as f32 * scale).round() as i32).max(1);
    let off_x = (cx as i32 - scaled_w) / 2;
    let off_y = (cy as i32 - scaled_h) / 2;

    // Cargar parámetros de transformación y cuadrícula
    let (zoom, pan_x, pan_y, grid_type) = {
      let params = PREVIEW_PARAMS.lock().unwrap();
      (params.zoom, params.pan_x, params.pan_y, params.grid.clone())
    };

    let half_w = (base_w as f32 / 2.0) / zoom;
    let half_h = (base_h as f32 / 2.0) / zoom;
    let left = base_w as f32 / 2.0 - half_w - pan_x;
    let right = base_w as f32 / 2.0 + half_w - pan_x;
    let bottom = base_h as f32 / 2.0 - half_h - pan_y;
    let top = base_h as f32 / 2.0 + half_h - pan_y;

    obs_ffi::gs_viewport_push();
    obs_ffi::gs_projection_push();
    obs_ffi::gs_ortho(left, right, bottom, top, -100.0, 100.0);
    obs_ffi::gs_set_viewport(off_x, off_y, scaled_w, scaled_h);
    let scene_source_ptr = {
      let guard = crate::CAPTURE_STATE.lock().unwrap();
      if let Some(state) = &*guard {
        obs_ffi::obs_scene_get_source(state.scene.0)
      } else {
        std::ptr::null_mut()
      }
    };
    if !scene_source_ptr.is_null() {
      obs_ffi::obs_source_video_render(scene_source_ptr);
    }

    // Dibujar cuadrícula de guía
    draw_grid_lines(base_w, base_h, &grid_type);

    obs_ffi::gs_projection_pop();
    obs_ffi::gs_viewport_pop();
  }
}

/// Crea la ventana hija + el `obs_display` que dibuja ahi.
pub unsafe fn create(parent_hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> Result<PreviewState, String> {
  register_class_once();
  let class_name = class_name_wide();
  let w = w.max(1);
  let h = h.max(1);

  let mut pt = POINT { x, y };
  ClientToScreen(parent_hwnd, &mut pt);

  let hwnd = CreateWindowExW(
    0,
    class_name.as_ptr(),
    std::ptr::null(),
    WS_POPUP | WS_VISIBLE,
    pt.x,
    pt.y,
    w,
    h,
    parent_hwnd,
    std::ptr::null_mut(),
    GetModuleHandleW(std::ptr::null()),
    std::ptr::null(),
  );
  if hwnd.is_null() {
    return Err("CreateWindowExW devolvio null para la ventana de preview".into());
  }

  let mut init_data = obs_ffi::gs_init_data {
    window: obs_ffi::gs_window { hwnd: hwnd as *mut c_void },
    cx: w as u32,
    cy: h as u32,
    num_backbuffers: 2,
    format: obs_ffi::gs_color_format_GS_BGRA,
    zsformat: obs_ffi::gs_zstencil_format_GS_ZS_NONE,
    adapter: 0,
  };
  let display = obs_ffi::obs_display_create(&mut init_data as *const _, 0xFFFF0000);
  if display.is_null() {
    DestroyWindow(hwnd);
    return Err("obs_display_create devolvio null (¿ya se inicio libobs?)".into());
  }
  obs_ffi::obs_display_add_draw_callback(display, Some(render_preview_callback), std::ptr::null_mut());

  // Notificar estado de zoom inicial al crearse el display
  emit_preview_params_change();

  Ok(PreviewState { hwnd, display })
}

pub unsafe fn resize(state: &PreviewState, parent_hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
  let w = w.max(1);
  let h = h.max(1);
  let mut pt = POINT { x, y };
  ClientToScreen(parent_hwnd, &mut pt);
  SetWindowPos(state.hwnd, std::ptr::null_mut(), pt.x, pt.y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
  obs_ffi::obs_display_resize(state.display, w as u32, h as u32);
}

pub unsafe fn destroy(state: PreviewState) {
  obs_ffi::obs_display_destroy(state.display);
  DestroyWindow(state.hwnd);
}

