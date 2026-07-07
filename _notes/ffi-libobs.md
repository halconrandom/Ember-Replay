# FFI Rust <-> libobs (funcionando)

Prerrequisitos instalados en esta maquina (via winget): `Kitware.CMake` y
`LLVM.LLVM` (bindgen necesita libclang; el path queda fijado en
`src-tauri/.cargo/config.toml` via `LIBCLANG_PATH`).

## Como esta armado

- `vendor/obs-studio` es un **git submodule** apuntando a
  `github.com/halconrandom/obs-studio-emberio` (fork propio de
  obsproject/obs-studio), rama `dev-halcon`. Ahi viven los 5 archivos
  parcheados (ver `_notes` de ese repo) para recortar el build a
  captura+audio+replay buffer. `origin` en ese submodule = nuestro fork,
  `upstream` = `obsproject/obs-studio` (para traer actualizaciones con
  `git fetch upstream` cuando haga falta).
- `src-tauri/build.rs`: corre `bindgen` contra `vendor/obs-studio/libobs/obs.h`,
  linkea contra `vendor/obs-studio/build_x64/libobs/RelWithDebInfo/obs.lib`, y
  copia a `target/<profile>/`:
  - `vendor/obs-studio/build_x64/rundir/RelWithDebInfo/bin/64bit/*` (obs.dll y
    las libobs-*.dll)
  - `vendor/obs-studio/build_x64/rundir/RelWithDebInfo/obs-plugins/64bit/`
    completo
  - `vendor/obs-studio/build_x64/rundir/RelWithDebInfo/data/` completo
  - `vendor/obs-studio/.deps/obs-deps-*-x64/bin/*` (runtime de
    ffmpeg/x264/curl/zlib que `obs.dll` necesita cargar en tiempo de
    arranque)

  Importante: `build_x64/` y `.deps/` son artefactos de build generados
  localmente (no vienen del submodule ni se commitean) — hay que compilar
  `vendor/obs-studio` una vez siguiendo los pasos de
  `vendor/obs-studio/_notes/build-minimo-logrado.md` antes de que
  `cargo build` funcione en un checkout nuevo.
- `src-tauri/src/obs_ffi.rs`: `include!` de los bindings generados
  (`obs_bindings.rs` en `OUT_DIR`), namespace separado del resto del app.
- Comando de prueba: `get_obs_version` en `lib.rs`, invocado desde un boton
  en `src/pages/index.astro` via `@tauri-apps/api/core`.

## Gotchas que costaron tiempo (para no repetir)

1. **No usar `.canonicalize()` en las rutas que le pasamos a clang.** En
   Windows devuelve paths con prefijo `\\?\` (verbatim), y clang no resuelve
   bien los `#include "relativo.h"` con ese prefijo — tiraba
   `'util/c99defs.h' file not found` aunque el archivo existia. Se uso
   `manifest_dir.join("../../Emberio")` sin canonicalizar, con un
   `assert!(...exists())` para el mensaje de error.

2. **`obs.dll` no arranca solo con lo que hay en `rundir/bin/64bit`.** Ese
   folder tiene las libobs-*.dll pero NO el runtime de terceros
   (avcodec/avutil/swscale/swresample/x264/curl/zlib) del que `obs.dll`
   depende en su tabla de imports — esas viven en
   `Emberio/.deps/obs-deps-<version>-x64/bin/`. Sin copiarlas, `app.exe`
   moria al instante con `STATUS_DLL_NOT_FOUND` (0xc0000135). `jansson` en
   cambio es estatica (solo hay `.lib`, no `.dll`), asi que esa no hace
   falta copiarla.

3. Cargo build scripts solo re-corren si cambia algo que trackean
   (`build.rs` en si mismo cuenta) — si tocas algo en el lado `Emberio/`
   (el vendor) sin tocar `build.rs`, puede que el copy de DLLs no se
   actualice solo. Si algo raro pasa con versiones de DLLs desactualizadas,
   `touch src-tauri/build.rs` o `cargo clean -p app` fuerza el rerun.

## Estado

Confirmado funcionando extremo a extremo: click en "Probar libobs" en la UI
-> `invoke("get_obs_version")` -> Rust llama `obs_get_version_string()` en
`obs.dll` -> devuelve `"31.1.0-emberio"` -> se muestra en pantalla.

## Proximo paso

Esto probo la plomeria (bindgen + link + runtime), pero `get_obs_version()`
no necesita `obs_startup()`. Lo que sigue es real: inicializar libobs
(`obs_startup` + reset de video/audio), cargar los modulos
(`obs_add_module`/`obs_load_all_modules` sobre `obs-plugins/64bit`), crear una
escena con la fuente de captura de monitor (`duplicator_capture`, ver
`Emberio/_notes/captura-y-replay-buffer.md`), agregar audio (`win-wasapi`), y
arrancar el output `replay_buffer` con un hotkey global para guardar.
