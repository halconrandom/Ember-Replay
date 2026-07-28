use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    // Antes de tauri_build::build(): este valida que bundle.resources
    // (tauri.conf.json) exista en disco en CADA build, y setup_libobs() es
    // quien puebla runtime/ con esos archivos.
    setup_libobs();
    tauri_build::build();
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
    let hook_src = emberio_root.join("build_x64/plugins/win-capture/graphics-hook/RelWithDebInfo/graphics-hook64.dll");

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

    // Se copia dos veces con el mismo contenido:
    // - target_dir (target/<profile>, junto al .exe de este build): para
    //   poder correr/testear con `cargo run`/`tauri dev` sin instalar nada.
    // - runtime_dir (src-tauri/runtime/, FUERA de target/): fuente estable
    //   e independiente de profile para que tauri.conf.json (bundle.resources)
    //   la referencie al empaquetar el instalador. target/<profile> no sirve
    //   para eso: tauri_build::build() valida que los resources existan en
    //   CADA `cargo build` (incluso uno en debug), y target/release podria
    //   no existir todavia la primera vez.
    let runtime_dir = manifest_dir.join("runtime");
    let deps_bin = find_obs_deps_bin(&emberio_root.join(".deps"));

    for dest in [&target_dir, &runtime_dir] {
        copy_files_flat(&rundir.join("bin/64bit"), dest);
        copy_dir_recursive(&rundir.join("obs-plugins/64bit"), &dest.join("obs-plugins/64bit"));

        // graphics-hook64.dll, necesario para capturar juegos (game_capture).
        if hook_src.exists() {
            fs::copy(&hook_src, dest.join("graphics-hook64.dll")).ok();
            fs::copy(&hook_src, dest.join("obs-plugins/64bit/graphics-hook64.dll")).ok();
        }

        // "data/" va junto al .exe: es el mismo directorio que usa el
        // instalador NSIS para los recursos empaquetados (resource_dir() de
        // Tauri en Windows == directorio del ejecutable), asi que este layout
        // funciona identico en dev y una vez instalado. lib.rs compensa el
        // "../../data/libobs/" hardcodeado de libobs fijando el CWD en un
        // subdirectorio ficticio 2 niveles por debajo del .exe (ver
        // ensure_obs_platform_initialized).
        copy_dir_recursive(&rundir.join("data"), &dest.join("data"));

        // obs.dll (y los encoders) linkean en tiempo de carga contra el
        // runtime de obs-deps (ffmpeg, x264, curl, jansson, zlib...). Ese
        // "bin" no se copia solo al rundir de OBS, asi que lo copiamos
        // nosotros.
        if let Some(bin) = &deps_bin {
            copy_files_flat(bin, dest);
        }
    }

    write_nsis_hooks(&manifest_dir, &runtime_dir);
}

/// El bundle.resources de tauri.conf.json (usado para empaquetar el .exe
/// instalador) esta roto en esta version de tauri-bundler para arboles con
/// subcarpetas: un unico glob plano como "runtime/*.dll" funciona, pero
/// apenas se agrega un segundo resource (o se usa un glob recursivo/copia de
/// carpeta) los archivos terminan mezclados bajo un path corrupto -- y lo
/// mismo pasa si se usan DOS comandos NSIS `File /r` separados (uno para
/// obs-plugins/, otro para data/) dentro del mismo hook: el segundo termina
/// pisando/mezclando el destino del primero. Verificado extrayendo el .exe
/// generado con 7z en cada variante. Lo unico que resulto confiable fue UN
/// SOLO `File /r` recursivo sobre runtime/ completo (que ya tiene la
/// estructura final exacta que necesitamos junto al .exe) -- asi que ya no
/// usamos bundle.resources para nada de esto, solo este hook
/// (`installerHooks` en tauri.conf.json).
fn write_nsis_hooks(manifest_dir: &Path, runtime_dir: &Path) {
    let notify_wav = manifest_dir
        .parent()
        .expect("no se pudo resolver el directorio padre de src-tauri")
        .join("notify.wav");
    let hooks = format!(
        r#"!macro NSIS_HOOK_POSTINSTALL
  SetOutPath "$INSTDIR"
  File /r "{runtime}\*.*"
  File "{notify_wav}"
!macroend
"#,
        runtime = runtime_dir.display(),
        notify_wav = notify_wav.display(),
    );
    fs::write(manifest_dir.join("nsis-hooks.nsh"), hooks)
        .expect("no se pudo escribir nsis-hooks.nsh");
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
