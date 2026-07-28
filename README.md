# Ember — Alpha 1.0.0-alpha.4

Ember es una app de escritorio para Windows que graba tu pantalla en segundo plano y te deja guardar los últimos segundos de juego/stream con un atajo de teclado, tipo "Shadowplay" pero propio. Corre sobre [Tauri](https://tauri.app) (Rust + Astro) con [libobs](https://github.com/obsproject/obs-studio) (el motor de OBS Studio) vendoreado como submódulo para capturar video/audio y codificar los clips.

Este documento describe **cómo funciona la alpha actual**, no el historial de desarrollo (para eso están los commits de `dev-halcon`).

## Qué hace

- Graba en un **buffer circular** (replay buffer) todo el tiempo en segundo plano, sin generar archivos hasta que vos decidís guardar algo.
- Con un atajo global guardás los últimos N segundos (configurable, 60s por defecto) como un `.mp4`.
- Un indicador circular permanente en la esquina inferior derecha del monitor que estás capturando te muestra el estado: **rojo** = no está grabando, **verde** = grabando.
- Todo corre desde la bandeja del sistema — cerrar la ventana la oculta, no mata el proceso ni corta la grabación.

## Atajos por defecto

| Atajo | Acción |
| --- | --- |
| `F9` | Guardar clip (los últimos N segundos del buffer) |
| `F10` | Prender/apagar la grabación en segundo plano |

Ambos son atajos **globales de Windows** (funcionan aunque Ember no tenga el foco) y se pueden reasignar desde la app.

## Qué se puede configurar

- **Fuente de video**: pantalla completa, ventana o captura de juego (usa `monitor_capture`, `window_capture` o `game_capture` de OBS).
- **Resolución**: original o 720p.
- **FPS**: configurable (60 por defecto).
- **Audio**: hasta 5 fuentes simultáneas (salida de escritorio y/o micrófonos vía WASAPI), cada una con volumen y mute independiente, pensado para setups tipo Voicemeeter con varios buses virtuales.
- **Overlays**: imagen, texto o navegador (browser source) superpuestos a la captura, con posición/escala/opacidad ajustables arrastrando directamente sobre el preview en vivo.
- **Carpeta de clips** y **duración del buffer**.
- **Codificador de video**: automático (prueba NVIDIA NVENC → AMD AMF → Intel QuickSync → x264 por software, hasta que uno arranque) o uno puntual a elección.

La configuración se persiste en `%APPDATA%\dev.halcondev.emberio\config.json`.

## Requisitos

- **Windows 10/11** (usa APIs nativas de Win32 para hotkeys globales, captura DXGI/WASAPI y el indicador de estado).
- **GPU** — recomendada una GPU con encoder de hardware (NVIDIA NVENC, AMD AMF o Intel QuickSync) para grabar sin cargar la CPU. Si no hay ninguna disponible, Ember cae automáticamente a codificación por software (x264), más lenta pero funciona en cualquier PC.

## Descargar

La última build compilada está en [**Releases**](https://github.com/halconrandom/emberio/releases) de este repo: descargá el `.exe` (NSIS) de la release más reciente, ejecutalo y seguí el asistente.

> Es una alpha: puede tener bugs, cambios de comportamiento entre versiones y todavía no hay auto-actualización.

Al abrir Ember por primera vez, el indicador de estado (rojo) aparece en la esquina inferior derecha de tu pantalla principal.

## Cómo compilarlo vos mismo

```sh
npm install
git submodule update --init   # trae vendor/obs-studio
npm run tauri build           # genera el instalador NSIS en src-tauri/target/release/bundle/nsis/
```

Para desarrollo día a día:

```sh
npm run tauri dev
```

## Stack

- **Frontend**: [Astro](https://docs.astro.build) + Tailwind, empaquetado como vistas nativas de Tauri (ventana principal + overlays de preview/toast/indicador de estado).
- **Backend**: Rust (`src-tauri/`), con bindings propios a libobs generados vía `bindgen` (`src-tauri/build.rs`).
- **Motor de captura/codificación**: libobs vendoreado en `vendor/obs-studio` (submódulo git), compilado aparte y empaquetado junto al ejecutable.
