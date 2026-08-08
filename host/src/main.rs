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
    #[arg(long, global = true, default_value = "../app")]
    app_dir: PathBuf,

    /// Rust `[lib]` name of the app crate (must match the built cdylib name)
    #[arg(long = "crate", global = true, default_value = "quest_hotpatch_app")]
    crate_name: String,

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
    Ok(())
}

fn deploy(cli: &Cli) -> Result<()> {
    let d = adb::device(cli.device.as_deref())?;
    let apk = app_dir(cli).join("target/debug/apk/questhotpatch.apk");
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

    let cli = Cli::parse();
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
    let original = app.join(format!(
        "target/aarch64-linux-android/debug/lib{}.so",
        cli.crate_name
    ));
    let ndk = std::env::var("ANDROID_NDK_HOME").unwrap_or_else(|_| {
        std::env::var("ANDROID_HOME")
            .map(|h| format!("{h}/ndk/30.0.14904198"))
            .unwrap_or_default()
    });
    let real_linker = format!(
        "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang"
    );
    let session =
        engine::PatchSession::load(&app, &cli.crate_name, &original, std::path::Path::new(&real_linker)).ok();
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
