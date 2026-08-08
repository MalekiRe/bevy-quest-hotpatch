# quest-hotpatch

> ⚠️ **AI-GENERATED DISCLOSURE**: This repository — its code, tooling, and this
> documentation — was built **interactively by an AI agent running
> DeepSeek V4 Flash**, working step-by-step with the user on real hardware
> (Android phones + a Meta Quest 3). It is an end-to-end *engineering
> experiment*, not a polished library. Expect rough edges; the README tells you
> exactly what we know works and what we deliberately left alone.

---

**Live [Subsecond](https://github.com/DioxusLabs/dioxus/tree/main/packages/subsecond)
hot-patching of Bevy apps on Android / Quest, built with `cargo apk`.**

Edit a Rust function body, save, and the **running** app patched the new code
in place — no rebuild, no reinstall, no restart. The flagship demo:

- a Bevy cube is painted **BLUE** every frame through a hot-patchable
  `desired_color()` function;
- flip it to **RED** and save → the running app turns the cube red.

Verified on:
- a Samsung S24 Ultra (plain phone, wireless adb)
- a **Meta Quest 3 running the official bevy_oxr OpenXR example** (Bevy 0.19),
  patched live in the headset over wireless adb — no crash, stable.

---

## Table of contents

1. [Prerequisites](#prerequisites)
2. [Repository layout](#repository-layout)
3. [Quick start](#quick-start)
4. [How it works](#how-it-works)
5. [Detailed usage](#detailed-usage)
6. [Troubleshooting](#troubleshooting)
7. [Credits & where the code came from](#credits--where-the-code-came-from)
8. [Licenses](#licenses)

---

## Prerequisites

- **Rust** (stable is fine; the earlier bevy_oxr examples recommended nightly
  for speed, but hot-patching itself works on stable) with the target:
  `rustup target add aarch64-linux-android`
- **`cargo-apk`** (the `cargo apk` cargo subcommand) — the APK builder
  - e.g. `cargo install cargo-apk`
- **Android SDK** (`ANDROID_HOME`) with platform **android-32** (the OpenXR
  sample targets API 32 — if you change it, `sdkmanager` a matching platform) and
  **build-tools** (for `apksigner`/`aapt`)
- **Android NDK** (`ANDROID_NDK_HOME`) — tested with NDK r29/r30
- **OpenJDK** (for apk signing)
- **adb** (with `platform-tools` on PATH), and a device with **USB debugging**
  enabled (phone: Developer Options; Quest: Developer Hub in the Meta mobile app)

You do **not** need the dioxus CLI (`dx`) — this project re-implements the small
slice of its devserver protocol it needs, so it also works for non-Dioxus (Bevy)
apps and, crucially, for apps whose APK *must* be produced by `cargo apk`
(OpenXR apps need `runtime_libs` + a Quest manifest that `dx` can't produce).

---

## Repository layout

```
quest-hotpatch/
  app/          Bevy 0.19 "phone" demo (plain Android app, no OpenXR)
      src/lib.rs        hand-rolled entry + hot-patchable desired_color()
      Cargo.toml       cargo-apk metadata, INTERNET perm, dev profile
  oxr-app/      Bevy 0.19 + OpenXR Quest sample (from bevy_oxr's android example)
      src/lib.rs        passthrough + hand-tracking scene + hot-patchable cube
      build.rs          fetches & bundles libopenxr_loader.so (OpenXR 1.1.38)
      runtime_libs/     cached OpenXR loader for aarch64
  host/         the patch engine + dev loop
      src/engine.rs     thin-rebuild, stubs, jump-table build/filter
      src/server.rs     dioxus-protocol WebSocket devserver (/_dioxus)
      src/main.rs       CLI: build | install | serve
      src/bin/rustc-shim.rs   RUSTC_WORKSPACE_WRAPPER capture shim
      src/bin/linker-shim.rs  linker capture shim
  questdev      tiny bash helper: build | install | run | logs | stop
  evidence_before_blue.png / evidence_after_red.png   proof it works
```

---

## Quick start

### 1. Build & deploy the baseline app

```sh
# from the repo root
export ANDROID_HOME=...  ANDROID_NDK_HOME=...
export DEVICE=192.168.1.151:5555        # your device's adb serial

# phone demo
./host/target/debug/quest-hotpatch-host --app-dir app --crate quest_hotpatch_app build --deploy

# OR the Quest OpenXR app:
# ./host/target/debug/quest-hotpatch-host --app-dir oxr-app --crate quest_oxr_app build --deploy
```

`build` runs `cargo apk build` **through the capture shims** (this is what
records the exact rustc/linker invocations the patch engine replays). `--deploy`
installs and launches it.

### 2. Run the dev loop

```sh
# MUST be running BEFORE the app starts, because the app dials the devserver
# exactly once at startup.
./host/target/debug/quest-hotpatch-host --app-dir app --crate quest_hotpatch_app serve
# ... or --app-dir oxr-app --crate quest_oxr_app serve for the Quest app
```

Watch for `app connected` in the log.

### 3. Hot-patch

Edit `app/src/lib.rs` (or `oxr-app/src/lib.rs`) — flip `desired_color()` from
blue to red — and save. Within ~3–6 seconds the serve loop:

```
change: .../src/lib.rs
stub: ... (warnings about libc symbols are normal)
jump table filtered before=2926 after=1
pushing patch (1)
```

…and the running app's cube turns red. No rebuild, no restart.

---

## How it works

```
┌─ host (your PC) ────────────────────────┐     ┌─ device (phone/Quest) ────────────┐
│ 1. build  cargo apk build + capture     │     │ APK (debug, hotpatching feature)  │
│           shims record rustc/linker args│     │  - subsecond + connect_subsecond  │
│ 2. serve  devserver on :8080 /_dioxus   │◄───►│  - dials ws://127.0.0.1:8080/…     │
│           (adb reverse tcp:8080)        │ ws  │  (INTERNET perm + --export-dynamic)│
│ 3. patch  watcher → thin-rebuild of     │     │  - hot points via                  │
│           ONLY the tip crate (the .rs   │     │    bevy::app::hotpatch::call(fn)   │
│           you edit), NDK-link into a    │     │  - apply_patch(jump_table)         │
│           patch .so (stubs for deps)    │◄────│                                    │
│           → JumpTable {old_addr→new}    │ push│  - call_as_ptr(): look up fn in    │
│           → push over WebSocket         │     │    table → run new body            │
└─────────────────────────────────────────┘     └────────────────────────────────────┘
```

Key concepts:

- **Thin rebuild**: subsecond only patches the *tip crate* (the crate you
  edit). On save, the host replays the captured rustc invocation with
  `--emit=obj` → one object file for your crate → links it with the NDK clang
  into a small patch `.so`, using **stub trampolines/absolute stubs** so
  references to the rest of the app (Bevy, wgpu, `log`, statics…) resolve at
  load time.
- **Jump table**: a map of `old function address → new function address`.
  `apply_patch` loads the patch `.so` (via memfd + dlopen on Android) and
  installs the table; every hot call site looks its function up in the table.
- **ASLR anchoring**: the app exports a sentinel `main` (`dlsym("main")`), and
  the JumpTable carries `main`'s build-time address so the device can translate
  table entries across the runtime ASLR slide.
- **Dispatch boundary**: Bevy 0.19's *automatic* system hotpatching doesn't
  refresh its cached pointers, so hot code is called through
  `subsecond::call(fn_pointer)` from the tip crate — the `call_as_ptr` path that
  actually consults the table.
- **Filtered table**: we deliberately remap **only the intended hot
  functions** (e.g. `desired_color`). Remapping everything crashes the app:
  unrelated dispatchers run patched code through calling conventions they don't
  expect.

---

## Detailed usage

### CLI

```
quest-hotpatch-host (global: --app-dir <dir>, --crate <libname>, --device <serial>, --port <8080>)

  build [--deploy]   cargo apk build through the capture shims (+ install/launch)
  install            adb install -r -t + am start
  serve              devserver + watcher + adb reverse + patch loop
```

### `questdev` helper (phone demo)

```sh
./questdev build | install | run | logs | stop
# DEVICE=... ./questdev run
```

### Quest 3 specifics

1. Enable Developer Mode in the Meta app; toggle **USB debugging** in the
   headset's Developer settings; accept the RSA prompt.
2. Wireless adb (once):
   ```sh
   adb devices                      # must list the Quest (serial like 2G0YC5ZGB908JQ)
   adb -s <serial> tcpip 5555
   adb -s <serial> shell ip -o -4 addr show wlan0    # e.g. 192.168.1.172
   adb connect <that-ip>:5555
   adb devices -l                   # wireless entry appears
   ```
3. `quest-hotpatch-host --app-dir oxr-app --crate quest_oxr_app build --deploy`
4. `… serve` (start first!), then launch the app from the headset library.
5. Watch for `app connected`, then edit `oxr-app/src/lib.rs`.

### Changing what's hot

Point `HOT_FUNCS` in `host/src/engine.rs` at your function name(s), and route
those functions through `bevy::app::hotpatch::call(fn_pointer)`.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| App never shows `app connected` | Missing `android.permission.INTERNET` in the manifest; or serve started *after* the app (relaunch the app); or a stale serve zombie holds :8080 (`pkill -f quest-hotpatch-host`) |
| `cannot locate symbol …log::MAX_LOG_LEVEL_FILTER…` | The app `.so` must be built with `--export-dynamic` (rustc-shim does this) so patches can link against it |
| `dlopen failed … cannot locate symbol <non-log symbol>` | Stub generation: data symbols need absolute stubs (engine does this); missing libc symbols (`malloc`, `memcpy`…) in warnings are **normal** |
| Patch "applies" but nothing changes | Determinism: `-C metadata` must be identical between deployed build and thin rebuild (pinned by rustc-shim); on first run `build --deploy` again after edits |
| SIGSEGV right after patch | Jump table was remapping everything → run the *filtered* engine (or fix your trampoline MOVK encodings if you touch `arm64_jump_stub`) |
| `mapped_oldaddr_in_table=false` in DIAG logs | Thin rebuild mangling diverged (metadata/capture drift) → re-run `build` to re-capture |
| Full-reload fallback | Any change subsecond can't patch (signatures, new struct fields, deps): `quest-hotpatch-host … build --deploy` |

---

## Credits & where the code came from

This project is a *mashup of pre-existing open-source ideas*, heavily modified
and debugged on real hardware. Specific sources:

1. **Subsecond + the Dioxus devtools protocol**
   — Dioxus team ([DioxusLabs/dioxus](https://github.com/DioxusLabs/dioxus),
   [`packages/subsecond`](https://github.com/DioxusLabs/dioxus/tree/main/packages/subsecond),
   [`dioxus-devtools`](https://crates.io/crates/dioxus-devtools),
   [`dioxus-cli-config`](https://crates.io/crates/dioxus-cli-config),
   `dioxus-devtools-types` 0.7.10).
   The jump-table scheme, `HotFn`/`call_as_ptr`/`call_it` semantics,
   `apply_patch` + memfd-on-Android, and the `ws://127.0.0.1:8080/_dioxus` wire
   format (which Bevy's own `connect_subsecond()` speaks) all come from here.
2. **Bevy 0.19** ([bevyengine/bevy](https://github.com/bevyengine/bevy))
   — the `hotpatching` feature, `bevy_app::hotpatch::{call, HotPatchPlugin}`,
   `FunctionSystem`'s `current_ptr`, and the Android entry pattern via
   `bevy::android::ANDROID_APP`. We observed that 0.19 never calls
   `refresh_hotpatch()`, which is why we route hot code through
   `subsecond::call(fn-ptr)` instead of relying on automatic system routing.
3. **whisker** ([whiskerrs/whisker](https://github.com/whiskerrs/whisker),
   [`whisker-dev-server`](https://crates.io/crates/whisker-dev-server))
   — the open-source model for the host-side patch engine: symbol-table
   parsing, old/new diffing (`build_jump_table`), the stub-object/trampoline
   approach for resolving patch references, and NDK toolchain handling. The
   host depends on it and extends it (data-symbol absolute stubs; corrected
   ARM64 MOVK trampoline encoding).
4. **bevy_oxr / BevyXR** ([awtterpip/bevy_oxr](https://github.com/awtterpip/bevy_oxr))
   — the entire `oxr-app/` sample: the OpenXR loader `build.rs`,
   `runtime_libs` bundling, the Quest manifest
   (`uses_feature`/`uses_permission`/VR intent filters), passthrough +
   hand-tracking + `HandGizmosPlugin` scene wiring, and the
   `bevy_mod_openxr` / `bevy_xr_utils` / `bevy_mod_xr` crates (0.6.x = the
   Bevy 0.19 line).
5. **cargo-apk** ([rust-mobile ecosystem](https://github.com/rust-mobile))
   — the APK packaging path; its `package.metadata.android` schema powers the
   manifests above.
6. **OpenXR** ([KhronosGroup/OpenXR-SDK-Source](https://github.com/KhronosGroup/OpenXR-SDK-Source))
   — the Android loader AAR (`libopenxr_loader.so`, v1.1.38) fetched by
   `oxr-app/build.rs`.
7. **ARM64 ABI** — MOVZ/MOVK/BR immediates for the stub trampolines (also
   documented in whisker's implementation).

Our own novel bits (built by debugging on-device, not copied from anywhere):
hand-rolled `android_main` + sentinel `main` + `whisker_aslr_anchor` (replacing
`#[bevy_main]` so Subsecond has its anchor); the rustc/linker **capture shims**
with pinned `-C metadata` and `--export-dynamic`; the **data-symbol absolute
stubs**; the **main-anchored ASLR math**; and the **jump-table filtering** that
makes the whole thing crash-free.

---

## Licenses

- This project's own code: MIT OR Apache-2.0 (matching upstream).
- `subsecond`, `dioxus-devtools*`: MIT / Apache-2.0 (Dioxus team).
- Bevy: MIT / Apache-2.0.
- bevy_oxr / bevy_mod_openxr / bevy_mod_xr / bevy_xr_utils: MIT / Apache-2.0.
- whisker-dev-server: MIT / Apache-2.0.
- cargo-apk: MIT / Apache-2.0.
- OpenXR loader: Apache-2.0 (plus API-dependent terms), see KhronosGroup/OpenXR-SDK-Source.

## Evidence

`evidence_before_blue.png` — cube BLUE (R=0 G=53 B=212).
`evidence_after_red.png` — cube RED ~23s after the live patch (R=215 G=46 B=32),
app still running (phone demo). The same recipe then worked unmodified on a
Meta Quest 3 running the OpenXR sample.
