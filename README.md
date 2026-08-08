# quest-hotpatch

Live **subsecond hot-patching on Android** (Quest-ready) for Bevy apps built with
`cargo apk` — the exact workflow you'd use to push a VR app to a Quest. You edit a
Rust function body, save, and the running app hot-patches it **in place** (no
rebuild, no reinstall, no restart).

The demo: a Bevy cube is painted **BLUE** every frame through a hot-patchable
`desired_color()` function. Editing that function to return **RED** live-patches
the running Android app to red — verified stable (see `evidence_*.png`).

## Architecture

```
quest-hotpatch/
  app/    the Bevy 0.19 android app (cargo-apk), aligned with bevy_oxr's layout
  host/   quest-hotpatch-host — the devserver + thin-rebuild patch engine
  questdev  quick build/install/run helper script
```

- **`cargo apk build`** stays the APK path (so it can bundle `libopenxr_loader.so`
  + Quest manifest later). Only the *initial* APK is built this way; subsequent
  edits become hot-patches, not APKs.
- **`host`** captures the exact rustc/linker invocations during the build
  (`rustc-shim` / `linker-shim`), then on each edit compiles **only the changed
  tip crate** into a patch `.so`, links it against the running app's symbols via
  NDK clang with ARM64 trampoline stubs, and streams a subsecond **JumpTable** to
  the app over a WebSocket (`ws://host:8080/_dioxus`) through `adb reverse`.
- The app's hot points are routed through `subsecond::call(fn-ptr)`.

## Requirements / state of the machine

- Rust stable, `aarch64-linux-android` target, `cargo-apk`, `dioxus-cli` (dx),
  Android SDK + NDK, `adb`.
- `ANDROID_HOME`, `ANDROID_NDK_HOME` set.

## Usage

```sh
# 1. deploy the baseline app to the device (debug, hot-patch-capable)
./host/target/debug/quest-hotpatch-host build --deploy
#    (or: cd app && cargo apk build   then   DEVICE=<serial> ../questdev run)

# 2. run the dev loop (devserver + watcher + adb reverse)
DEVICE=192.168.1.151:5555 ./host/target/debug/quest-hotpatch-host serve
#    IMPORTANT: start serve FIRST, then launch the app (the app connects once at startup).

# 3. edit app/src/lib.rs  (e.g. flip desired_color's BLUE -> RED) and save.
#    The running app hot-patches within ~3 seconds.
```

## The hard-won integration notes (everything that made this actually work)

1. **`android.permission.INTERNET`** — cargo-apk generates *no* permissions, so the
   app's socket to the devserver dies with EPERM. Must be in the manifest.
2. **Symbol export** — the app `.so` only exports its `#[no_mangle]` anchors by
   default; the patch `.so` can't resolve the rest. `--export-dynamic` via the
   rustc shim makes every symbol visible to `dlopen`.
3. **Data-symbol stubs** — upstream whisker's stub generator skips *data* symbols
   (statics like `log::MAX_LOG_LEVEL_FILTER`), so patches fail with
   `dlopen: cannot locate symbol`. We emit absolute-address stubs for data.
4. **Deterministic mangling** — the thin-rebuild's rustc must reproduce the exact
   `-C metadata` of the deployed build, or symbol names diverge and the JumpTable
   matches nothing. Pin it in the shim.
5. **Anchor on `main`** — subsecond's `apply_patch()` computes the ASLR slide from
   `dlsym("main")`, so `JumpTable.aslr_reference`/`new_base_address` must be
   `main`'s build-time address (whisker's anchor symbol is a different address —
   mixing them silently skews every mapping).
6. **fn()-pointer dispatch** — Bevy 0.19's `FunctionSystem` never refreshes its
   `current_ptr` (`refresh_hotpatch` is dead code), so *automatic* system routing
   can't redirect. Route the hot function through
   `subsecond::call(coerced_fn_pointer)` — the `call_as_ptr` path *does* consult
   the JumpTable.
7. **ARM64 trampolines** — MOVK bit-encoding: base `0xF280_0010 | (hw << 21)`
   (a naive `0xF2A0_0010 | (hw << 21)` double-counts bit 21 and jumps to garbage).
8. **Filter the JumpTable** — remapping *every* tip-crate symbol makes Bevy's
   dispatchers (and dep-adjacent code on compute threads) try to run patched code
   through calling conventions they don't expect → SIGSEGV. Remap **only the
   intended hot functions** (the rest fall back to deployed code, which is correct).

## Evidence

`evidence_before_blue.png` — cube BLUE (R=0 G=53 B=212).
`evidence_after_red.png` — cube RED ~23s after the live patch (R=215 G=46 B=32), app alive.

## Tooling

- `questdev` — bash helper: `build | install | run | logs | stop`.
- `host/quest-hotpatch-host` subcommands: `build [--deploy]`, `install`, `serve`.
