use std::path::{Path, PathBuf};

const LOG_EXTENSION: &str = "log";
const LOG_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub fn job_log_path(output: &Path) -> Result<PathBuf, String> {
    let Some(folder_name) = output.file_name() else {
        return Err(format!(
            "Cannot name a job log beside the output folder {}: it has no folder name",
            output.display()
        ));
    };
    let mut log_name = folder_name.to_os_string();
    log_name.push(".");
    log_name.push(LOG_EXTENSION);
    // dcpdoctor and photon flag any file inside the package that the ASSETMAP does not list
    Ok(output.with_file_name(log_name))
}

pub fn encode_threads_status(encode_threads: u32) -> String {
    let count = crate::grok_encoder::encode_thread_count(encode_threads);
    if encode_threads == crate::grok_encoder::AUTOMATIC_ENCODE_THREADS {
        return format!("{count} (automatic)");
    }
    count.to_string()
}

pub fn picture_findings_status(detect_picture_findings: bool) -> &'static str {
    if detect_picture_findings { "on" } else { "off" }
}

pub fn log_timestamp() -> String {
    chrono::Local::now()
        .format(LOG_TIMESTAMP_FORMAT)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_named_after_the_package_folder_beside_it() {
        assert_eq!(
            job_log_path(Path::new("/x/my_dcp")).unwrap(),
            Path::new("/x/my_dcp.log")
        );
        assert_eq!(
            job_log_path(Path::new("/x/my.dcp/")).unwrap(),
            Path::new("/x/my.dcp.log")
        );
        assert_eq!(
            job_log_path(Path::new("my_dcp")).unwrap(),
            Path::new("my_dcp.log")
        );
    }

    #[test]
    fn an_output_with_no_folder_name_is_refused_naming_it() {
        for output in ["/", "..", "/x/.."] {
            let error = job_log_path(Path::new(output)).unwrap_err();
            assert!(
                error.contains(output),
                "the error does not name {output}: {error}"
            );
        }
    }

    #[test]
    fn the_thread_count_says_when_it_was_chosen_automatically() {
        assert_eq!(encode_threads_status(2), "2");
        let automatic = encode_threads_status(crate::grok_encoder::AUTOMATIC_ENCODE_THREADS);
        assert!(automatic.ends_with(" (automatic)"), "{automatic}");
        assert_ne!(automatic, "0 (automatic)");
    }
}
