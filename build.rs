fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_LIBMPV").is_none() {
        return;
    }
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("mpv")
        .expect("the libmpv feature needs libmpv development files (mpv-libs-devel / libmpv-dev)");
}
