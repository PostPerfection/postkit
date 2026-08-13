/// Directory holding the mpv import library on windows, which has no pkg-config.
const MPV_LIB_DIR_ENV: &str = "MPV_LIB_DIR";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
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
