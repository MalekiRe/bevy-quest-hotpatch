//! Linker shim: records the link invocation into `<WHISKER_LINKER_CACHE_DIR>/*.json`
//! (format matches whisker's `CapturedLinkerInvocation`), then forwards to the
//! real linker given in `WHISKER_REAL_LINKER` (e.g. NDK clang).

use std::env;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(serde::Serialize)]
struct Captured {
    output: Option<String>,
    args: Vec<String>,
    timestamp_micros: u128,
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let output = args
        .windows(2)
        .find(|w| w[0] == "-o")
        .map(|w| w[1].clone());

    let cache = env::var("WHISKER_LINKER_CACHE_DIR").unwrap_or_else(|_| "/tmp/linker-capture".into());
    let _ = std::fs::create_dir_all(&cache);
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
    let rec = Captured { output, args, timestamp_micros: ts };
    let fname = format!("link-{ts}.json");
    let _ = std::fs::write(Path::new(&cache).join(fname), serde_json::to_vec(&rec).unwrap());

    let real = env::var("WHISKER_REAL_LINKER").unwrap_or_else(|_| "clang".into());
    let status = Command::new(real).args(env::args().skip(1)).status();
    match status { Ok(s) => std::process::exit(s.code().unwrap_or(1)), Err(e) => { eprintln!("linker-shim: {e}"); std::process::exit(1); } }
}
