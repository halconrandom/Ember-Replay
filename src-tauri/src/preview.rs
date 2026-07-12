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
static SELECTED_OVERLAY: Mutex<Option<usize>> = Mutex::new(None);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResizeCorner {
  TopLeft,
  TopRight,
  BottomLeft,
  BottomRight,
}

pub enum DragAction {
  None,
  Pan,
  MoveOverlay(usize),
  ResizeOverlay {
    index: usize,
    corner: ResizeCorner,
    aspect_ratio: f32,
    initial_x: f32,
    initial_y: f32,
    initial_scale_x: f32,
    initial_scale_y: f32,
    initial_w: f32,
    initial_h: f32,
    click_canvas_x: f32,
    click_canvas_y: f32,
  }
}

static DRAG_ACTION: Mutex<DragAction> = Mutex::new(DragAction::None);

/// Resolucion base actual del canvas de OBS.
static CANVAS_SIZE: Mutex<(u32, u32)> = Mutex::new((1920, 1080));

/// Rectangulo (x, y, w, h en coordenadas de canvas) del overlay seleccionado,
/// precalculado desde el thread de UI. El callback de render SOLO lee este
/// valor -- nunca debe tocar CAPTURE_STATE/CONFIG desde el thread grafico de
/// libobs, porque el thread de UI toma esos locks mientras llama funciones de
/// libobs que a su vez esperan al thread grafico (deadlock AB-BA: la app se
/// congelaba y la ventana quedaba en blanco).
static SELECTION_RECT: Mutex<Option<(f32, f32, f32, f32)>> = Mutex::new(None);

/// Recalcula SELECTION_RECT desde el thread de UI. Llamar cada vez que cambia
/// la seleccion o el transform del overlay seleccionado.
pub fn refresh_selection_rect() {
  let selected = *SELECTED_OVERLAY.lock().unwrap();
  let rect = selected.and_then(|index| unsafe { crate::get_overlay_rect(index) });
  *SELECTION_RECT.lock().unwrap() = rect;
}

/// Limpia seleccion y drag (ej. cuando se elimina/reordena un overlay y los
/// indices dejan de valer).
pub fn clear_selection() {
  *SELECTED_OVERLAY.lock().unwrap() = None;
  *DRAG_ACTION.lock().unwrap() = DragAction::None;
  *SELECTION_RECT.lock().unwrap() = None;
}

/// Rectangulo de vista (left, top, ancho, alto) en coordenadas de canvas
/// (origen arriba-izquierda, igual que los scene items de OBS), aplicando
/// zoom y paneo. Usado tanto por el render como por el mapeo del mouse para
/// que siempre coincidan.
fn view_rect(base_w: u32, base_h: u32) -> (f32, f32, f32, f32) {
  let (zoom, pan_x, pan_y) = {
    let params = PREVIEW_PARAMS.lock().unwrap();
    (params.zoom, params.pan_x, params.pan_y)
  };
  let vw = base_w as f32 / zoom;
  let vh = base_h as f32 / zoom;
  let left = (base_w as f32 - vw) / 2.0 - pan_x;
  let top = (base_h as f32 - vh) / 2.0 - pan_y;
  (left, top, vw, vh)
}

pub fn client_to_canvas(click_x: i32, click_y: i32, cx: i32, cy: i32) -> Option<(f32, f32)> {
  let (base_w, base_h) = *CANVAS_SIZE.lock().unwrap();
  if base_w == 0 || base_h == 0 || cx == 0 || cy == 0 {
    return None;
  }

  let scale = (cx as f32 / base_w as f32).min(cy as f32 / base_h as f32);
  let scaled_w = (base_w as f32 * scale).round() as i32;
  let scaled_h = (base_h as f32 * scale).round() as i32;
  let off_x = (cx - scaled_w) / 2;
  let off_y = (cy - scaled_h) / 2;

  let px = click_x - off_x;
  let py = click_y - off_y;

  if px >= 0 && px < scaled_w && py >= 0 && py < scaled_h {
    let (view_left, view_top, view_w, view_h) = view_rect(base_w, base_h);
    let frac_x = px as f32 / scaled_w as f32;
    let frac_y = py as f32 / scaled_h as f32;
    Some((view_left + frac_x * view_w, view_top + frac_y * view_h))
  } else {
    None
  }
}

/// `screen_to_canvas` = pixeles de canvas por pixel de pantalla (inversa de
/// letterbox_scale * zoom): asi el area de agarre de la esquina mide siempre
/// ~14px reales en pantalla, sin importar zoom ni tamano del preview.
fn check_corner_click(canvas_x: f32, canvas_y: f32, x: f32, y: f32, w: f32, h: f32, screen_to_canvas: f32) -> Option<ResizeCorner> {
  let threshold = 14.0 * screen_to_canvas;
  let threshold_sq = threshold * threshold;

  let dist_sq = |cx: f32, cy: f32| {
    let dx = canvas_x - cx;
    let dy = canvas_y - cy;
    dx * dx + dy * dy
  };

  if dist_sq(x, y) < threshold_sq {
    return Some(ResizeCorner::TopLeft);
  }
  if dist_sq(x + w, y) < threshold_sq {
    return Some(ResizeCorner::TopRight);
  }
  if dist_sq(x, y + h) < threshold_sq {
    return Some(ResizeCorner::BottomLeft);
  }
  if dist_sq(x + w, y + h) < threshold_sq {
    return Some(ResizeCorner::BottomRight);
  }

  None
}

pub fn set_canvas_size(w: u32, h: u32) {
  *CANVAS_SIZE.lock().unwrap() = (w, h);
}

pub fn canvas_size() -> (u32, u32) {
  *CANVAS_SIZE.lock().unwrap()
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

unsafe fn draw_selection_box(x: f32, y: f32, w: f32, h: f32, handle_size: f32) {
  let effect = obs_ffi::obs_get_base_effect(obs_ffi::obs_base_effect_OBS_EFFECT_SOLID);
  if effect.is_null() {
    return;
  }
  let technique = obs_ffi::gs_effect_get_technique(effect, b"Solid\0".as_ptr() as *const _);
  let color_param = obs_ffi::gs_effect_get_param_by_name(effect, b"color\0".as_ptr() as *const _);
  if technique.is_null() || color_param.is_null() {
    return;
  }

  // Border color: Vibrant Orange (0.88, 0.35, 0.06, 0.9)
  let border_color = vec4_of(0.88, 0.35, 0.06, 0.9);
  obs_ffi::gs_effect_set_vec4(color_param, &border_color as *const _);

  // Coordenadas de canvas top-left, igual que la proyeccion del render.
  let left = x;
  let right = x + w;
  let top = y;
  let bottom = y + h;

  let passes = obs_ffi::gs_technique_begin(technique);
  for i in 0..passes {
    obs_ffi::gs_technique_begin_pass(technique, i);
    obs_ffi::gs_render_start(true);

    // Draw selection border (rectangle)
    obs_ffi::gs_vertex2f(left, top);
    obs_ffi::gs_vertex2f(right, top);

    obs_ffi::gs_vertex2f(right, top);
    obs_ffi::gs_vertex2f(right, bottom);

    obs_ffi::gs_vertex2f(right, bottom);
    obs_ffi::gs_vertex2f(left, bottom);

    obs_ffi::gs_vertex2f(left, bottom);
    obs_ffi::gs_vertex2f(left, top);

    // Tamano del handle en pixeles de canvas (ya escalado para que se vea
    // constante en pantalla, lo pasa el caller).
    let hs = handle_size;

    // Draw 4 square handles at the corners using lines
    for cx in &[left, right] {
      for cy in &[bottom, top] {
        obs_ffi::gs_vertex2f(cx - hs, cy - hs);
        obs_ffi::gs_vertex2f(cx + hs, cy - hs);

        obs_ffi::gs_vertex2f(cx + hs, cy - hs);
        obs_ffi::gs_vertex2f(cx + hs, cy + hs);

        obs_ffi::gs_vertex2f(cx + hs, cy + hs);
        obs_ffi::gs_vertex2f(cx - hs, cy + hs);

        obs_ffi::gs_vertex2f(cx - hs, cy + hs);
        obs_ffi::gs_vertex2f(cx - hs, cy - hs);
      }
    }

    obs_ffi::gs_render_stop(obs_ffi::gs_draw_mode_GS_LINES);
    obs_ffi::gs_technique_end_pass(technique);
  }
  obs_ffi::gs_technique_end(technique);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
  match msg {
    windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN => {
      let x = (lparam & 0xffff) as i16 as i32;
      let y = ((lparam >> 16) & 0xffff) as i16 as i32;
      *LAST_MOUSE.lock().unwrap() = Some((x, y));

      let mut rect: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
      windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
      let cx = rect.right - rect.left;
      let cy = rect.bottom - rect.top;

      let zoom = PREVIEW_PARAMS.lock().unwrap().zoom;
      // Pixeles de canvas por pixel de pantalla (para el area de agarre).
      let (base_w, base_h) = *CANVAS_SIZE.lock().unwrap();
      let letterbox = if base_w > 0 && cx > 0 {
        (cx as f32 / base_w as f32).min(cy as f32 / base_h.max(1) as f32)
      } else {
        1.0
      };
      let screen_to_canvas = 1.0 / (letterbox * zoom).max(0.0001);

      if let Some((canvas_x, canvas_y)) = client_to_canvas(x, y, cx, cy) {
        // First check if they clicked a corner of the CURRENTLY selected overlay
        let current_selected = *SELECTED_OVERLAY.lock().unwrap();
        let mut drag_started = false;

        if let Some(index) = current_selected {
          if let Some((ox, oy, ow, oh)) = crate::get_overlay_rect(index) {
            if let Some(corner) = check_corner_click(canvas_x, canvas_y, ox, oy, ow, oh, screen_to_canvas) {
              let (w_orig, h_orig, sx, sy) = crate::get_overlay_source_dims_and_scale(index).unwrap_or((1.0, 1.0, 1.0, 1.0));
              *DRAG_ACTION.lock().unwrap() = DragAction::ResizeOverlay {
                index,
                corner,
                aspect_ratio: w_orig / h_orig,
                initial_x: ox,
                initial_y: oy,
                initial_scale_x: sx,
                initial_scale_y: sy,
                initial_w: ow,
                initial_h: oh,
                click_canvas_x: canvas_x,
                click_canvas_y: canvas_y,
              };
              drag_started = true;
            }
          }
        }
        
        if !drag_started {
          // Check if they clicked a new overlay
          if let Some(index) = crate::try_select_overlay(canvas_x, canvas_y) {
            *SELECTED_OVERLAY.lock().unwrap() = Some(index);
            *DRAG_ACTION.lock().unwrap() = DragAction::MoveOverlay(index);
          } else {
            *SELECTED_OVERLAY.lock().unwrap() = None;
            *DRAG_ACTION.lock().unwrap() = DragAction::Pan;
          }
        }
      } else {
        *SELECTED_OVERLAY.lock().unwrap() = None;
        *DRAG_ACTION.lock().unwrap() = DragAction::Pan;
      }
      refresh_selection_rect();
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN => {
      let x = (lparam & 0xffff) as i16 as i32;
      let y = ((lparam >> 16) & 0xffff) as i16 as i32;
      *LAST_MOUSE.lock().unwrap() = Some((x, y));
      *SELECTED_OVERLAY.lock().unwrap() = None;
      *DRAG_ACTION.lock().unwrap() = DragAction::Pan;
      refresh_selection_rect();
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP => {
      *LAST_MOUSE.lock().unwrap() = None;
      let mut drag_action = DRAG_ACTION.lock().unwrap();
      match *drag_action {
        DragAction::MoveOverlay(_) | DragAction::ResizeOverlay { .. } => {
          *drag_action = DragAction::None;
          crate::finish_drag_overlay();
        }
        _ => {
          *drag_action = DragAction::None;
        }
      }
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MBUTTONUP => {
      *LAST_MOUSE.lock().unwrap() = None;
      *DRAG_ACTION.lock().unwrap() = DragAction::None;
      0
    }
    windows_sys::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE => {
      let wparam_u = wparam as usize;
      let left_down = (wparam_u & 0x0001) != 0;
      let middle_down = (wparam_u & 0x0010) != 0;
      let shift_down = (wparam_u & 0x0004) != 0; // MK_SHIFT
      
      let x = (lparam & 0xffff) as i16 as i32;
      let y = ((lparam >> 16) & 0xffff) as i16 as i32;
      
      if left_down || middle_down {
        let mut last_mouse = LAST_MOUSE.lock().unwrap();
        if let Some((last_x, last_y)) = *last_mouse {
          let dx = x - last_x;
          let dy = y - last_y;
          if dx != 0 || dy != 0 {
            let drag_action = DRAG_ACTION.lock().unwrap();
            
            // If middle button is down, we always force Pan
            let current_action = if middle_down { DragAction::Pan } else {
              match *drag_action {
                DragAction::None => DragAction::Pan,
                ref other => match other {
                  DragAction::MoveOverlay(idx) => DragAction::MoveOverlay(*idx),
                  DragAction::ResizeOverlay {
                    index,
                    corner,
                    aspect_ratio,
                    initial_x,
                    initial_y,
                    initial_scale_x,
                    initial_scale_y,
                    initial_w,
                    initial_h,
                    click_canvas_x,
                    click_canvas_y,
                  } => DragAction::ResizeOverlay {
                    index: *index,
                    corner: *corner,
                    aspect_ratio: *aspect_ratio,
                    initial_x: *initial_x,
                    initial_y: *initial_y,
                    initial_scale_x: *initial_scale_x,
                    initial_scale_y: *initial_scale_y,
                    initial_w: *initial_w,
                    initial_h: *initial_h,
                    click_canvas_x: *click_canvas_x,
                    click_canvas_y: *click_canvas_y,
                  },
                  _ => DragAction::Pan,
                }
              }
            };

            match current_action {
              DragAction::MoveOverlay(index) => {
                let (base_w, base_h) = *CANVAS_SIZE.lock().unwrap();
                let mut rect: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect); }
                let cx = rect.right - rect.left;
                let cy = rect.bottom - rect.top;
                
                let scale = (cx as f32 / base_w as f32).min(cy as f32 / base_h as f32);
                let zoom = PREVIEW_PARAMS.lock().unwrap().zoom;
                let overall_scale = scale * zoom;
                if overall_scale > 0.0 {
                  let dx_canvas = dx as f32 / overall_scale;
                  let dy_canvas = dy as f32 / overall_scale;
                  crate::drag_selected_overlay(index, dx_canvas, dy_canvas);
                }
              }
              DragAction::ResizeOverlay {
                index,
                corner,
                aspect_ratio,
                initial_x,
                initial_y,
                initial_scale_x: _,
                initial_scale_y: _,
                initial_w,
                initial_h,
                click_canvas_x,
                click_canvas_y,
              } => {
                let mut rect: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect); }
                let cx = rect.right - rect.left;
                let cy = rect.bottom - rect.top;
                
                if let Some((canvas_x, canvas_y)) = client_to_canvas(x, y, cx, cy) {
                  let dx_canvas = canvas_x - click_canvas_x;
                  let dy_canvas = canvas_y - click_canvas_y;

                  let (w_orig, h_orig, _, _) = crate::get_overlay_source_dims_and_scale(index).unwrap_or((1.0, 1.0, 1.0, 1.0));

                  let mut new_x = initial_x;
                  let mut new_y = initial_y;
                  let mut new_w;
                  let mut new_h;

                  match corner {
                    ResizeCorner::BottomRight => {
                      new_w = initial_w + dx_canvas;
                      new_h = initial_h + dy_canvas;
                    }
                    ResizeCorner::TopRight => {
                      new_w = initial_w + dx_canvas;
                      new_h = initial_h - dy_canvas;
                      new_y = (initial_y + initial_h) - new_h;
                    }
                    ResizeCorner::BottomLeft => {
                      new_w = initial_w - dx_canvas;
                      new_h = initial_h + dy_canvas;
                      new_x = (initial_x + initial_w) - new_w;
                    }
                    ResizeCorner::TopLeft => {
                      new_w = initial_w - dx_canvas;
                      new_h = initial_h - dy_canvas;
                      new_x = (initial_x + initial_w) - new_w;
                      new_y = (initial_y + initial_h) - new_h;
                    }
                  }

                  new_w = new_w.max(10.0);
                  new_h = new_h.max(10.0);

                  // Default is proportional resize (Shift NOT down)
                  if !shift_down {
                    match corner {
                      ResizeCorner::BottomRight | ResizeCorner::BottomLeft => {
                        new_h = new_w / aspect_ratio;
                      }
                      ResizeCorner::TopRight | ResizeCorner::TopLeft => {
                        new_h = new_w / aspect_ratio;
                        new_y = (initial_y + initial_h) - new_h;
                      }
                    }
                  }

                  let scale_x = new_w / w_orig;
                  let scale_y = new_h / h_orig;

                  crate::resize_selected_overlay(index, new_x, new_y, scale_x, scale_y);
                }
              }
              _ => {
                let mut params = PREVIEW_PARAMS.lock().unwrap();
                let scale_factor = params.zoom;
                params.pan_x += (dx as f32) / scale_factor;
                params.pan_y += (dy as f32) / scale_factor;
                drop(params);
                emit_preview_params_change();
              }
            }
            *last_mouse = Some((x, y));
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

    obs_ffi::gs_render_stop(obs_ffi::gs_draw_mode_GS_LINES);
    obs_ffi::gs_technique_end_pass(technique);
  }
  obs_ffi::gs_technique_end(technique);
}

/// Dibuja la escena actual (lo mismo que ve la grabacion) escalada y
/// centrada dentro del rectangulo real de la ventana de preview, aplicando
/// zoom, paneo y cuadricula de ayuda.
///
/// IMPORTANTE: esto corre en el thread grafico de libobs. NO debe tomar
/// CAPTURE_STATE/CONFIG ni ningun lock que el thread de UI tome mientras
/// llama a libobs -- eso producia el deadlock que congelaba la app entera
/// (ventana en blanco, "la aplicacion no responde"). Por eso renderiza la
/// textura principal ya compuesta (obs_render_main_texture, cero locks
/// nuestros) y lee la seleccion de SELECTION_RECT, que es solo datos.
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

    let grid_type = PREVIEW_PARAMS.lock().unwrap().grid.clone();
    let (view_left, view_top, view_w, view_h) = view_rect(base_w, base_h);

    obs_ffi::gs_viewport_push();
    obs_ffi::gs_projection_push();
    obs_ffi::gs_ortho(view_left, view_left + view_w, view_top, view_top + view_h, -100.0, 100.0);
    obs_ffi::gs_set_viewport(off_x, off_y, scaled_w, scaled_h);

    // La escena esta puesta como output source (canal 0), asi que la textura
    // principal ya la tiene compuesta -- mismo mecanismo que el preview de
    // OBS Studio.
    obs_ffi::obs_render_main_texture();

    // Dibujar cuadricula de guia
    draw_grid_lines(base_w, base_h, &grid_type);

    // Caja de seleccion del overlay (precalculada por el thread de UI).
    // handle_size en px de canvas para que se vea ~5px constantes en pantalla.
    if let Some((ox, oy, ow, oh)) = *SELECTION_RECT.lock().unwrap() {
      let screen_to_canvas = (view_w / scaled_w.max(1) as f32).max(0.0001);
      draw_selection_box(ox, oy, ow, oh, 5.0 * screen_to_canvas);
    }

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
    format: obs_ffi::gs_color_format_GS_RGBA,
    zsformat: obs_ffi::gs_zstencil_format_GS_ZS_NONE,
    adapter: 0,
  };
  // Color de fondo del display: libobs lo interpreta como 0xAABBGGRR (RGBA
  // little-endian, R en el byte bajo). Para que se mezcle con el panel #121215:
  let display = obs_ffi::obs_display_create(&mut init_data as *const _, 0xFF151212);
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

