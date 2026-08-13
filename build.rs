fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_LIBMPV").is_none() {
        return;
    }
    // mpv_render only compiles on linux, so only linux builds need the library
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("mpv")
        .expect("the libmpv feature needs libmpv development files (mpv-libs-devel / libmpv-dev)");
}
