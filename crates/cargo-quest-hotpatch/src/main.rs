//! quest-hotpatch-host — dev loop for subsecond hot-patching of a bevy
//! cargo-apk app on Android (phone or Quest), keeping `cargo apk build` as
//! the build path (OpenXR/Quest compatible).
//!
//!   build    cargo apk build with capture shims + (optional) deploy
//!   install  adb install + launch
//!   serve    watch + dioxus-protocol /_dioxus devserver + adb reverse;
//!            hot-patches tip-crate changes, full-rebuilds everything else

mod adb;
mod engine;
mod server;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use notify::Watcher;
use server::DevServer;

#[derive(Parser)]
#[command(name = "quest-hotpatch-host", about = "subsecond hot-patch dev loop for bevy+cargo-apk Android apps (OpenXR/Quest-ready)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Android device serial (default: first connected; wireless ok, e.g. 192.168.1.151:5555)
    #[arg(long, global = true, env = "DEVICE")]
    device: Option<String>,

    /// Path to the app crate (has Cargo.toml with [package.metadata.android])
    #[arg(long, global = true, default_value = ".")]
    app_dir: PathBuf,

    /// Rust `[lib]` name of the app crate (auto-detected from Cargo.toml when omitted)
    #[arg(long = "crate", global = true)]
    crate_name: Option<String>,

    /// The app's hot function names (comma-separated, matched against mangled
    /// symbol names). These + subsecond's dispatch trampolines are the only
    /// symbols allowed into the jump table.
    #[arg(long = "hot-funcs", global = true, value_delimiter = ',',
          default_value = "desired_color,hot_flag,rotate,paint_cube")]
    hot_funcs: Vec<String>,


    /// Devserver port (must match what the app dials: 127.0.0.1:8080 on Android)
    #[arg(long, global = true, default_value_t = 8080)]
    port: u16,

    /// APK package name
    #[arg(long, global = true, default_value = "dev.malek.questhotpatch")]
    package: String,

    /// Launcher activity
    #[arg(long, global = true, default_value = "android.app.NativeActivity")]
    activity: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fat build: cargo apk build with the capture shims active (baseline for patching)
    Build {
        /// Also install + launch on the device after building
        #[arg(long)]
        deploy: bool,
    },
    /// adb install + launch the existing APK
    Install,
    /// The dev loop: devserver + watcher + adb reverse (+ full-rebuild fallback)
    Serve,
}

/// Read `[lib] name` from the app crate's Cargo.toml (fallback: package name).
fn detect_lib_name(app_dir: &std::path::Path) -> Result<String> {
    let cargo_toml = app_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo_toml)
        .with_context(|| format!("read {}", cargo_toml.display()))?;
    let mut in_lib = false;
    let mut fallback = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_lib = line == "[lib]";
            if !in_lib && line.starts_with("[package]") {
                for l in text.lines() {
                    if let Some(v) = l.trim().strip_prefix("name = ") {
                        fallback = Some(v.trim_matches('"').to_string());
                        break;
                    }
                }
            }
            continue;
        }
        if in_lib {
            if let Some(v) = line.strip_prefix("name = ") {
                return Ok(v.trim_matches('"').to_string());
            }
        }
    }
    fallback.ok_or_else(|| anyhow::anyhow!("no [lib] name in {cargo_toml:?}"))
}

/// Workspace-aware scratch dir: the shims write captures relative to the app
/// path, but builds land in the workspace target root — so scratch must use
/// the workspace root, else the engine reads a stale dir.
fn scratch_root(app: &std::path::Path) -> PathBuf {
    ws_target_root(app).join(".questhotpatch")
}

/// Workspace-aware target root: for a crate inside a cargo workspace, cargo
/// builds into <workspace-root>/target. Falls back to <app>/target.
fn ws_target_root(app: &std::path::Path) -> PathBuf {
    let mut d = app.to_path_buf();
    loop {
        let cargo = d.join("Cargo.toml");
        if cargo.exists() {
            let txt = std::fs::read_to_string(&cargo).unwrap_or_default();
            if txt.contains("[workspace]") {
                return d.join("target");
            }
        }
        if let Some(parent) = d.parent() {
            d = parent.to_path_buf();
        } else {
            break;
        }
    }
    app.join("target")
}

/// Newest existing artifact among candidates.
fn pick_newest(cands: Vec<PathBuf>) -> Option<PathBuf> {
    cands
        .into_iter()
        .filter(|c| c.exists())
        .max_by_key(|c| std::fs::metadata(c).map(|m| m.modified().unwrap_or(std::time::UNIX_EPOCH)).unwrap_or(std::time::UNIX_EPOCH))
}

fn original_binary_for(cli: &Cli) -> PathBuf {
    let name = cli.crate_name.as_deref().unwrap_or("quest_hotpatch_app");
    let ws = ws_target_root(&app_dir(cli));
    let app = app_dir(cli);
    let cands: Vec<PathBuf> = vec![
        ws.join(format!("aarch64-linux-android/debug/lib{name}.so")),
        app.join(format!("target/aarch64-linux-android/debug/lib{name}.so")),
    ];
    pick_newest(cands.clone()).unwrap_or_else(|| cands[0].clone())
}

fn apk_for(cli: &Cli) -> PathBuf {
    let app = app_dir(cli);
    let ws = ws_target_root(&app);
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(ws.join("debug/apk")) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "apk").unwrap_or(false) {
                cands.push(e.path());
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(app.join("target/debug/apk")) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "apk").unwrap_or(false) {
                cands.push(e.path());
            }
        }
    }
    pick_newest(cands).unwrap_or_else(|| app.join("target/debug/apk/questhotpatch.apk"))
}

fn app_dir(cli: &Cli) -> PathBuf {
    if cli.app_dir.is_absolute() {
        cli.app_dir.clone()
    } else {
        std::env::current_dir().unwrap().join(&cli.app_dir)
    }
}

/// Path to the host tool's own built shim binaries.
fn shim_root() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join(".")
}

/// Run `cargo apk build` with the rustc/linker capture shims active.
fn capture_fat_build(cli: &Cli) -> Result<()> {
    let app = app_dir(cli);
    let scratch = engine::scratch_dir(&app);
    let rustc_cache = scratch.join("rustc");
    let linker_cache = scratch.join("linker");
    // fresh captures every build (guards against cross-project pollution)
    let _ = std::fs::remove_dir_all(&rustc_cache);
    let _ = std::fs::remove_dir_all(&linker_cache);
    std::fs::create_dir_all(&rustc_cache)?;
    std::fs::create_dir_all(&linker_cache)?;

    let shim = shim_root();
    let rustc_shim = shim.join("rustc-shim");
    let linker_shim = shim.join("linker-shim");
    anyhow::ensure!(rustc_shim.exists() && linker_shim.exists(),
        "shim binaries not found next to this binary ({shim:?}); run `cargo build --bins` first");

    // NDK clang is the real linker the shim forwards to.
        let ndk_home = std::env::var("ANDROID_NDK_HOME").unwrap_or_else(|_| {
        std::env::var("ANDROID_HOME")
            .map(|h| format!("{h}/ndk/30.0.14904198"))
            .unwrap_or_default()
    });
    let real_linker = format!(
        "{ndk_home}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang"
    );
    anyhow::ensure!(std::path::Path::new(&real_linker).exists(), "NDK clang not found at {real_linker}");

    tracing::info!("fat build (capture) for {}", app.display());
    let status = Command::new("cargo")
        .arg("apk").arg("build")
        .current_dir(&app)
        .env("RUSTC_WORKSPACE_WRAPPER", &rustc_shim)
        .env("WHISKER_RUSTC_CACHE_DIR", &rustc_cache)
        .env("WHISKER_LINKER_CACHE_DIR", &linker_cache)
        .env("WHISKER_REAL_LINKER", &real_linker)
        // rustc-shim rewrites -Clinker=<ndk clang> to -Clinker=<linker-shim>
        .env("QUEST_HOTPATCH_LINK_SHIM", &linker_shim)
        .env("QUEST_HOTPATCH_EXPORT_DYNAMIC", "1")
        .status()
        .context("spawn `cargo apk build`")?;
    anyhow::ensure!(status.success(), "cargo apk build failed");

    // Promote the app crate's cdylib link invocation to a canonical
    // `fat_link.json` (dx run_fat_link parity): the patch build replays these
    // exact args with the tip crate's fresh objects, so dep addresses match
    // the deployed binary and the jump table needs no name filter.
    let scratch = engine::scratch_dir(&app);
    let mut best: Option<(std::time::SystemTime, serde_json::Value)> = None;
    for entry in std::fs::read_dir(&linker_cache)? {
        let path = entry?.path();
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path)?) else { continue };
        let Some(out) = rec.get("output").and_then(|o| o.as_str()) else { continue };
        if !out.contains("libquest_hotpatch_app.so")
            && !out.contains(&format!("lib{}.so", detect_lib_name(&app).unwrap_or_default()))
        { continue; }
        let mt = std::fs::metadata(&path)?.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, rec));
        }
    }
    if let Some((_, rec)) = best {
        std::fs::create_dir_all(&scratch)?;
        std::fs::write(scratch.join("fat_link.json"), serde_json::to_vec(&rec)?)?;
        tracing::info!("canonical fat_link.json written");
    }
    Ok(())
}

fn deploy(cli: &Cli) -> Result<()> {
    let d = adb::device(cli.device.as_deref())?;
    let apk = apk_for(cli);
    tracing::info!("installing {apk:?} on {d}");
    adb::install(&d, &apk)?;
    adb::launch(&d, &cli.package, &cli.activity)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut cli = Cli::parse();
    if cli.crate_name.is_none() {
        cli.crate_name = Some(detect_lib_name(&cli.app_dir)?);
        tracing::info!("detected app crate: {}", cli.crate_name.as_deref().unwrap());
    }
    match &cli.cmd {
        Cmd::Build { deploy: deploy_flag } => {
            capture_fat_build(&cli)?;
            if *deploy_flag {
                deploy(&cli)?;
            }
        }
        Cmd::Install => deploy(&cli)?,
        Cmd::Serve => serve_loop(&cli).await?,
    }
    Ok(())
}

async fn serve_loop(cli: &Cli) -> Result<()> {
    let app = app_dir(cli);
    let d = adb::device(cli.device.as_deref())?;
    adb::reverse(&d, cli.port)?;
    tracing::info!("adb reverse tcp:{port} <-> tcp:{port}", port = cli.port);

    // If we have a prior capture, load the patch engine; otherwise the loop
    // starts in full-rebuild-only mode until a `build` has been run.
    let scratch = engine::scratch_dir(&app);
    let original = original_binary_for(cli);
    let ndk = std::env::var("ANDROID_NDK_HOME").unwrap_or_else(|_| {
        std::env::var("ANDROID_HOME")
            .map(|h| format!("{h}/ndk/30.0.14904198"))
            .unwrap_or_default()
    });
    let real_linker = format!(
        "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang"
    );
    let session = engine::PatchSession::load(
        &app,
        cli.crate_name.as_deref().unwrap_or("quest_hotpatch_app"),
        &cli.hot_funcs,
        &original,
        std::path::Path::new(&real_linker),
    )
    .ok();
    if let Some(s) = &session {
        tracing::info!(ready = s.is_ready(), "patch engine loaded");
    } else {
        tracing::warn!("no capture state yet — run `quest-hotpatch-host build --deploy` first; loop starts in full-rebuild mode");
    }

    let dev = DevServer::new();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cli.port));
    let server_handle = tokio::spawn(dev.clone().serve(addr));

    // --- file watcher ---
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        use notify::EventKind;
        if let Ok(ev) = res {
            if matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                let _ = tx.send(ev);
            }
        }
    })?;
    watcher.watch(&app.join("src"), notify::RecursiveMode::Recursive)?;
    tracing::info!("watching {}", app.join("src").display());

    for ev in rx {
        let is_rust = ev
            .paths
            .iter()
            .any(|p| p.extension().map(|e| e == "rs").unwrap_or(false));
        if !is_rust {
            continue;
        }
        tracing::info!("change: {:?}", ev.paths);

        // debounce
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let Some(session) = &session else { continue; };
        if !session.is_ready() {
            tracing::warn!("patch engine not ready — full rebuild required (no subsecond patch applied)");
            continue;
        }

        match dev.current_session() {
            Some(cs) => match session.build_patch(cs.aslr_reference).await {
                Ok((mut jt, patch_path)) => {
                    tracing::info!("pushing patch ({})", jt.map.len());
                    // The app's subsecond runtime needs the patch .so on ITS filesystem:
                    // adb push it and point table.lib at the on-device path.
                    let dev_path = "/data/local/tmp/questhotpatch/libquest_hotpatch_app_patch.so";
                    let push = Command::new("adb")
                        .args(["-s", &d, "push"])
                        .arg(&patch_path).arg(dev_path)
                        .status();
                    match push {
                        Ok(st) if st.success() => {
                            jt.lib = std::path::PathBuf::from(dev_path);
                            dev.send_patch_for(cs.aslr_reference, jt);
                        }
                        Ok(_) => tracing::error!("adb push of patch failed"),
                        Err(e) => tracing::error!("adb push error: {e}"),
                    }
                }
                Err(e) => {
                    tracing::error!("patch build failed: {e:#}");
                    tracing::warn!("falling back to full rebuild");
                    full_rebuild(cli, &d, &dev).await;
                }
            },
            None => {
                tracing::warn!("app not connected; skipping patch");
            }
        }
    }
    server_handle.await??;
    Ok(())
}

async fn full_rebuild(cli: &Cli, device: &str, dev: &DevServer) {
    tracing::info!("full rebuild (cargo apk build + reinstall + relaunch)");
    if let Err(e) = capture_fat_build(cli) {
        tracing::error!("rebuild failed: {e:#}");
        dev.request_full_reload();
        return;
    }
    if let Err(e) = deploy(cli) {
        tracing::error!("reinstall failed: {e:#}");
    }
    let _ = device;
}
