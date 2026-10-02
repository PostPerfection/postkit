/// Directory holding the mpv import library on windows, which has no pkg-config.
const MPV_LIB_DIR_ENV: &str = "MPV_LIB_DIR";

const GROK_PLUGIN_BUILD_INFO_FUNCTION: &str = "grk_plugin_build_info";
const GROK_PLUGIN_INIT_INFO_START: &str = "typedef struct _grk_plugin_init_info";
const GROK_PLUGIN_INIT_INFO_END: &str = "} grk_plugin_init_info;";
const GROK_PLUGIN_NUM_THREADS_FIELD: &str = "num_threads";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(grok_plugin_build_info)");
    println!("cargo::rustc-check-cfg=cfg(grok_plugin_num_threads)");
    if std::env::var_os("CARGO_FEATURE_GROK_FFI").is_some() {
        let header_text = read_grok_header();
        detect_grok_plugin_build_info(&header_text);
        detect_grok_plugin_num_threads(&header_text);
    }
    postkit_ffmpeg_link_search::emit_ffmpeg_link_search();
    if std::env::var_os("CARGO_FEATURE_LIBMPV").is_none() {
        return;
    }
    let target_family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let families: Vec<&str> = target_family.split(',').collect();
    if families.contains(&"unix") {
        link_unix();
    } else if families.contains(&"windows") {
        link_windows();
    }
}

fn read_grok_header() -> String {
    let grok = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libgrokj2k")
        .expect("the grok-ffi feature needs libgrokj2k development files found through pkg-config");
    let header = grok
        .include_paths
        .iter()
        .map(|include_path| include_path.join("grok.h"))
        .find(|header| header.exists())
        .expect("no grok.h in the libgrokj2k include paths");
    println!("cargo::rerun-if-changed={}", header.display());
    std::fs::read_to_string(&header).expect("grok.h is readable")
}

// grokj2k-sys has no binding for this function when built against an older grok.h
fn detect_grok_plugin_build_info(header_text: &str) {
    if header_text.contains(GROK_PLUGIN_BUILD_INFO_FUNCTION) {
        println!("cargo::rustc-cfg=grok_plugin_build_info");
    }
}

fn detect_grok_plugin_num_threads(header_text: &str) {
    let init_info = header_text
        .split_once(GROK_PLUGIN_INIT_INFO_START)
        .and_then(|(_, rest)| rest.split_once(GROK_PLUGIN_INIT_INFO_END))
        .map(|(init_info, _)| init_info)
        .expect("grok.h declares grk_plugin_init_info");
    if init_info.contains(GROK_PLUGIN_NUM_THREADS_FIELD) {
        println!("cargo::rustc-cfg=grok_plugin_num_threads");
    }
}

fn link_unix() {
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("mpv")
        .expect(
            "the libmpv feature needs libmpv development files (mpv-libs-devel / libmpv-dev / brew install mpv)",
        );
}

fn link_windows() {
    println!("cargo::rerun-if-env-changed={MPV_LIB_DIR_ENV}");
    let Ok(lib_dir) = std::env::var(MPV_LIB_DIR_ENV) else {
        panic!(
            "the libmpv feature needs {MPV_LIB_DIR_ENV} set to the directory holding the mpv import library. \
             an msvc toolchain needs mpv.lib, generated from libmpv-2.dll with gendef and lib.exe. \
             a mingw toolchain uses libmpv.dll.a"
        );
    };
    println!("cargo::rustc-link-search=native={lib_dir}");
    println!("cargo::rustc-link-lib=dylib=mpv");
}
