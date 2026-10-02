const FFMPEG_DIR_VARIABLE: &str = "FFMPEG_DIR";

// a pkg-config crate's /usr/lib64 otherwise links a distro FFmpeg
pub fn emit_ffmpeg_link_search() {
    println!("cargo:rerun-if-env-changed={FFMPEG_DIR_VARIABLE}");
    if let Some(directory) = std::env::var_os(FFMPEG_DIR_VARIABLE) {
        let library_directory = std::path::Path::new(&directory).join("lib");
        println!(
            "cargo:rustc-link-search=native={}",
            library_directory.display()
        );
    }
}
