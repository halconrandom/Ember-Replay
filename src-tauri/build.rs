use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    tauri_build::build();
    setup_libobs();
}

/// Emberio: genera los bindings de Rust hacia libobs (vendoreado en
/// ../../Emberio, ver _notes/*.md ahi) y copia los binarios de runtime
/// (obs.dll, plugins, data) junto al ejecutable para poder correr/testear
/// sin instalar nada mas.
fn setup_libobs() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // OJO: evitar .canonicalize() aca -- en Windows devuelve rutas con
    // prefijo "\\?\" (verbatim) que rompen la resolucion de includes
    // relativos de clang (ej. "util/c99defs.h" dejaba de encontrarse).
    let emberio_root = manifest_dir.join("../vendor/obs-studio");
    assert!(
        emberio_root.join("libobs/obs.h").exists(),
        "No se encontro vendor/obs-studio (submodule). Corre: git submodule update --init"
    );

    let libobs_dir = emberio_root.join("libobs");
    let build_config_dir = emberio_root.join("build_x64/config");
    let build_libobs_dir = emberio_root.join("build_x64/libobs");
    let lib_dir = emberio_root.join("build_x64/libobs/RelWithDebInfo");
    let rundir = emberio_root.join("build_x64/rundir/RelWithDebInfo");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=obs");

    let bindings = bindgen::Builder::default()
        // obs.h ya incluye obs-audio-controls.h internamente (linea ~85),
        // asi que obs_volmeter_* ya viene incluido sin agregar nada mas.
        .header(libobs_dir.join("obs.h").to_string_lossy())
        .clang_arg(format!("-I{}", libobs_dir.display()))
        .clang_arg(format!("-I{}", build_config_dir.display()))
        .clang_arg(format!("-I{}", build_libobs_dir.display()))
        .clang_arg(format!("-I{}", emberio_root.display()))
        .allowlist_function("obs_.*")
        .allowlist_type("obs_.*")
        .allowlist_var("OBS_.*")
        .allowlist_function("calldata_.*")
        .allowlist_type("calldata_.*")
        .allowlist_function("signal_handler_.*")
        .allowlist_function("proc_handler_.*")
        .allowlist_function("bfree")
        .allowlist_type("obs_interaction_flags")
        // Preview: obs_display dibuja via la API de graficos de libobs
        // (gs_*), que no tiene el prefijo "obs_".
        .allowlist_function("gs_.*")
        .allowlist_type("gs_.*")
        .allowlist_type("vec2")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("No se pudieron generar los bindings de libobs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("obs_bindings.rs"))
        .expect("No se pudo escribir obs_bindings.rs");

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out -> subimos 3 niveles
    // para llegar a target/<profile>, donde queda el .exe final.
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("No se pudo resolver target dir desde OUT_DIR")
        .to_path_buf();

    copy_files_flat(&rundir.join("bin/64bit"), &target_dir);
    copy_dir_recursive(&rundir.join("obs-plugins/64bit"), &target_dir.join("obs-plugins/64bit"));

    // Copiar el graphics-hook64.dll necesario para capturar juegos (game_capture)
    let hook_src = emberio_root.join("build_x64/plugins/win-capture/graphics-hook/RelWithDebInfo/graphics-hook64.dll");
    if hook_src.exists() {
        let hook_dst_exe = target_dir.join("graphics-hook64.dll");
        let hook_dst_plugin = target_dir.join("obs-plugins/64bit/graphics-hook64.dll");
        fs::copy(&hook_src, &hook_dst_exe).ok();
        fs::copy(&hook_src, &hook_dst_plugin).ok();
    }

    // OJO: "data/" NO va junto al .exe. libobs busca sus archivos core
    // (shaders/effects) con una ruta hardcodeada "../../data/libobs/"
    // relativa al CWD (no al ejecutable, ver libobs/obs-windows.c
    // find_libobs_data_file -- no hay API publica para overridear esto).
    // Como en lib.rs fijamos el CWD al directorio del .exe (target/<profile>)
    // antes de llamar obs_startup, "data/" tiene que vivir 2 niveles arriba
    // de ahi (junto a Cargo.toml) para que "../../data/libobs/" resuelva bien.
    let src_tauri_dir = target_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("no se pudo resolver src-tauri/ desde target dir");
    copy_dir_recursive(&rundir.join("data"), &src_tauri_dir.join("data"));

    // obs.dll (y los encoders) linkean en tiempo de carga contra el runtime
    // de obs-deps (ffmpeg, x264, curl, jansson, zlib...). Ese "bin" no se
    // copia solo al rundir de OBS, asi que lo copiamos nosotros.
    if let Some(deps_bin) = find_obs_deps_bin(&emberio_root.join(".deps")) {
        copy_files_flat(&deps_bin, &target_dir);
    }
}

fn find_obs_deps_bin(deps_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(deps_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy();
        if path.is_dir() && name.starts_with("obs-deps-") && name.ends_with("-x64") {
            let bin = path.join("bin");
            if bin.is_dir() {
                return Some(bin);
            }
        }
    }
    None
}

fn copy_files_flat(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).ok();
    let Ok(entries) = fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = fs::copy(&path, dst.join(path.file_name().unwrap()));
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).ok();
    let Ok(entries) = fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir_recursive(&path, &target);
        } else {
            let _ = fs::copy(&path, &target);
        }
    }
}
