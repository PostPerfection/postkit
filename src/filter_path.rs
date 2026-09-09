use std::path::Path;

// a windows drive colon ends a filter option, and the graph parser strips one
// backslash before the option parser sees the value
pub(crate) fn filter_option_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "/")
        .replace(':', "\\\\:")
}
