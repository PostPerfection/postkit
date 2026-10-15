use super::detection_filters;

/// Run blackdetect and freezedetect on a copy of the finished frames, leaving
/// `picture_filters` as the chain the encoder reads.
pub(crate) fn with_detection_branch(picture_filters: &str) -> String {
    format!(
        "{picture_filters},split=2[picture][detect];[detect]{},nullsink;[picture]null",
        detection_filters()
    )
}

// the same branch ending in a labelled output, for a graph built in process
pub(crate) fn with_detection_sink(picture_filters: &str, sink_label: &str) -> String {
    format!(
        "{picture_filters},split=2[picture][detect];[detect]{}[{sink_label}];[picture]null",
        detection_filters()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detection_branch_leaves_the_picture_chain_alone() {
        assert_eq!(
            with_detection_branch("fps=24,lut3d=/luts/hdr.cube"),
            "fps=24,lut3d=/luts/hdr.cube,split=2[picture][detect];\
             [detect]blackdetect=black_min_duration=2:pixel_black_th=0.1,\
             freezedetect=duration=2,nullsink;[picture]null"
        );
    }
}
