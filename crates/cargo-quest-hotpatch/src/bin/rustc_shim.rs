//! RUSTC_WORKSPACE_WRAPPER shim: records the rustc invocation for the workspace
//! crate(s) into `<WHISKER_RUSTC_CACHE_DIR>/*.json` files, then forwards to the
//! real rustc. Format matches whisker's `CapturedRustcInvocation`.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(serde::Serialize)]
struct Captured {
    crate_name: String,
    args: Vec<String>,
    envs: std::collections::BTreeMap<String, String>,
    timestamp_micros: u128,
}

fn main() {
    // RUSTC_WORKSPACE_WRAPPER convention: argv[0] = real rustc path, argv[1..] = rustc args.
    let all: Vec<String> = env::args().skip(1).collect();
    let real_rustc = all.first().cloned().unwrap_or_else(|| "rustc".into());
    let mut args = all[1..].to_vec();

    // Absolutize the crate source path so the thin-rebuild replay works from any cwd
    // (the capture may run with a different working dir than the engine's replay).
    if let Some(i) = args.iter().position(|a| a.ends_with(".rs") && !a.starts_with('-')) {
        let src = std::path::Path::new(&args[i]);
        if src.is_relative() {
            // Trust the real process cwd, NOT the PWD env var (which can be a
            // stale value from the parent shell on shared machines).
            let base = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            args[i] = std::path::Path::new(&base).join(src).to_string_lossy().into_owned();
        }
    }

    // Determinism: pin -C metadata so symbol mangling is identical between the
    // deployed build and the thin-rebuild replay (else the jump-table name->map
    // diff never matches and patching silently no-ops).
    for i in 0..args.len() {
        if args[i] == "-C" && i + 1 < args.len() && args[i + 1].starts_with("metadata=") {
            args[i + 1] = "metadata=5151515151cafe5".to_string();
        } else if args[i].starts_with("-Cmetadata=") {
            args[i] = "-Cmetadata=5151515151cafe5".to_string();
        }
    }

    // If running as the capture fat build, redirect the final link through
    // our linker-shim so the exact link invocation gets recorded. The shim
    // forwards to the real NDK clang (WHISKER_REAL_LINKER).
    if env::var("QUEST_HOTPATCH_EXPORT_DYNAMIC").as_deref() == Ok("1")
        && args.iter().any(|a| a.ends_with(".rcgu.o") || a.ends_with("fcgi.o") || a.contains("--crate-type"))
    {
        // export everything so subsecond patch .so's can resolve deps via dlopen
        args.push("-C".into());
        args.push("link-arg=-Wl,--export-dynamic".into());
    }
    if let Ok(shim) = env::var("QUEST_HOTPATCH_LINK_SHIM") {
        // rustc receives the linker either as `-Clinker=<path>` (single token)
        // or as `-C` + `linker=<path>` (separated). Handle both.
        if let Some(i) = args.iter().position(|a| a.starts_with("linker=")) {
            let _ = i;
        }
        if let Some(i) = args.iter().position(|a| a == "-Clinker=" || a.starts_with("linker=")) {
            // replace the value part
            if args[i].starts_with("-Clinker=") {
                args[i] = format!("-Clinker={shim}");
            } else {
                args[i] = format!("linker={shim}");
            }
        }
    }

    let crate_name = args
        .windows(2)
        .find(|w| w[0] == "--crate-name")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "unknown".into());

    let cache = env::var("WHISKER_RUSTC_CACHE_DIR").unwrap_or_else(|_| "/tmp/rustc-capture".into());

    // Only capture invocations whose source file actually exists (guards against
    // unrelated concurrent builds on the same machine writing into our cache).
    let src = args.iter().find(|a| a.ends_with(".rs") && !a.starts_with('-'));
    if let Some(src) = src {
        if !Path::new(src).exists() {
            std::process::exit(match std::process::Command::new(&real_rustc).args(&args).status() {
                Ok(s) => s.code().unwrap_or(1),
                Err(e) => { eprintln!("rustc-shim: {e}"); 1 }
            });
        }
    }

    let _ = std::fs::create_dir_all(&cache);
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
    // keep a few envs that rustc needs to replay (CARGO_/OUT_DIR/DIOXUS_/ANDROID_*)
    let envs = env::vars()
        .filter(|(k, _)| {
            k.starts_with("CARGO_") || k.starts_with("OUT_DIR") || k.starts_with("DIOXUS_")
                || k.starts_with("ANDROID_") || k.starts_with("NDK") || k == "ANDROID_HOME"
        })
        .collect();
    let rec = Captured { crate_name: crate_name.clone(), args: args.clone(), envs, timestamp_micros: ts };
    let _ = &crate_name;
    let fname = format!("{crate_name}-{ts}.json");
    let _ = std::fs::write(Path::new(&cache).join(fname), serde_json::to_vec(&rec).unwrap());

    let status = std::process::Command::new(&real_rustc).args(&args).status();
    match status { Ok(s) => std::process::exit(s.code().unwrap_or(1)), Err(e) => { eprintln!("rustc-shim: {e}"); std::process::exit(1); } }
}
