// Fetch the OpenXR loader for Android and drop it where cargo-apk bundles it
// (runtime_libs/arm64-v8a/libopenxr_loader.so). Adapted from bevy_oxr's android
// example, with a cache so dev rebuilds don't re-download.
use std::env;
use std::fs;
use std::io::Read;
use std::path::Path;

pub const OPENXR_VERSION: &str = "1.1.38";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var_os("CARGO_CFG_TARGET_OS") != Some("android".into()) {
        return;
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let dest_path = Path::new(&manifest_dir)
        .join("./runtime_libs/arm64-v8a/libopenxr_loader.so");
    if dest_path.exists() {
        println!("cargo:warning=openxr loader already present, skipping download");
        return;
    }

    let url = format!(
        "https://github.com/KhronosGroup/OpenXR-SDK-Source/releases/download/release-{}/openxr_loader_for_android-{}.aar",
        OPENXR_VERSION, OPENXR_VERSION
    );
    println!("cargo:warning=downloading OpenXR loader from {url}");
    let mut resp = reqwest::blocking::get(&url).expect("download openxr loader aar");
    let mut buf = Vec::new();
    resp.read_to_end(&mut buf).unwrap();

    let tmp = Path::new(&manifest_dir).join("runtime_libs/arm64-v8a/.openxr_loader.aar.tmp");
    fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    fs::write(&tmp, &buf).unwrap();

    let mut zip = zip::ZipArchive::new(fs::File::open(&tmp).unwrap()).unwrap();
    let mut loader = zip
        .by_name("prefab/modules/openxr_loader/libs/android.arm64-v8a/libopenxr_loader.so")
        .unwrap();
    let mut out = Vec::new();
    loader.read_to_end(&mut out).unwrap();
    fs::write(&dest_path, &out).unwrap();
    fs::remove_file(&tmp).ok();
    println!("cargo:warning=openxr loader extracted: {} bytes", out.len());
}
