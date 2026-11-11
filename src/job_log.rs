use std::path::{Path, PathBuf};

const LOG_EXTENSION: &str = "log";

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
}
