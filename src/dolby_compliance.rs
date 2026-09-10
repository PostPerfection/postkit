use crate::cpl_xml::read_prefixed_tag;
use crate::dolby_vision::{
    BaseLayerSignalling, DolbyVisionLevel, allowed_base_layer_signalling, dolby_vision_level_for,
    read_dolby_vision,
};
use crate::preview::{is_jpeg2000_mxf, resolve_picture};
use crate::preview_colour::{DisplayPrimaries, DisplayTransfer, resolve_picture_colour};
use std::path::{Path, PathBuf};

// what a Dolby Vision check found, in the shape the compliance command prints
#[derive(Debug, Default, PartialEq)]
pub struct DolbyVisionCompliance {
    pub checked: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// Dolby Vision Profiles and levels V1.2.92 table 1 writes the VUI as EOTF,
// primaries, matrix, range
fn signalling_name(signalling: BaseLayerSignalling) -> String {
    format!(
        "{},{},{},{}",
        signalling.transfer_characteristics,
        signalling.colour_primaries,
        signalling.matrix_coefficients,
        u8::from(signalling.full_range)
    )
}

// an App 2E descriptor names its colour by UL, so the VUI code points come back
// from the pair the descriptor resolved to
fn signalling_of(
    transfer: DisplayTransfer,
    primaries: DisplayPrimaries,
) -> Option<BaseLayerSignalling> {
    let signalling = match (transfer, primaries) {
        (DisplayTransfer::Pq, DisplayPrimaries::Bt2020) => BaseLayerSignalling {
            transfer_characteristics: 16,
            colour_primaries: 9,
            matrix_coefficients: 9,
            full_range: false,
        },
        (DisplayTransfer::Bt709, DisplayPrimaries::Bt709) => BaseLayerSignalling {
            transfer_characteristics: 1,
            colour_primaries: 1,
            matrix_coefficients: 1,
            full_range: false,
        },
        _ => return None,
    };
    Some(signalling)
}

// V1.2.92 calls HDR10 without MaxCLL and MaxFALL PQ10
const HDR10_TRANSFER_CHARACTERISTICS: u8 = 16;

fn base_layer_finding(
    profile: u8,
    signalling: BaseLayerSignalling,
    result: &mut DolbyVisionCompliance,
) {
    let allowed = allowed_base_layer_signalling(profile);
    if allowed.contains(&signalling) {
        result.checked.push(format!(
            "base layer VUI {} is what table 1 allows profile {profile}",
            signalling_name(signalling)
        ));
        return;
    }
    let allowed_names: Vec<String> = allowed.iter().copied().map(signalling_name).collect();
    result.errors.push(format!(
        "base layer VUI {} is not what table 1 allows profile {profile}: {}",
        signalling_name(signalling),
        allowed_names.join(" or ")
    ));
}

fn level_finding(
    width: u32,
    height: u32,
    frames_per_second: f64,
    result: &mut DolbyVisionCompliance,
) {
    if width == 0 || height == 0 || frames_per_second <= 0.0 || frames_per_second.is_nan() {
        result.errors.push(format!(
            "the raster or frame rate could not be read: \
             {width}x{height} @ {frames_per_second:.3}fps"
        ));
        return;
    }
    match dolby_vision_level_for(width, height, frames_per_second) {
        Some(level) => result.checked.push(format!(
            "{width}x{height} @ {frames_per_second:.3}fps is Dolby Vision level {:02} {}, \
             main tier {} Mbps, high tier {} Mbps",
            level.id,
            level.name,
            level.main_tier_megabits_per_second,
            level.high_tier_megabits_per_second
        )),
        None => result.errors.push(format!(
            "{width}x{height} @ {frames_per_second:.3}fps is past uhd60, the highest \
             Dolby Vision level"
        )),
    }
}

// only a master is held to the tiers, a mezzanine sits above them by design
fn tier_finding(
    level: DolbyVisionLevel,
    megabits_per_second: f64,
    result: &mut DolbyVisionCompliance,
) {
    if megabits_per_second > f64::from(level.high_tier_megabits_per_second) {
        result.errors.push(format!(
            "{megabits_per_second:.1} Mbps is over the {} Mbps high tier of level {:02} {}",
            level.high_tier_megabits_per_second, level.id, level.name
        ));
    } else if megabits_per_second > f64::from(level.main_tier_megabits_per_second) {
        result.warnings.push(format!(
            "{megabits_per_second:.1} Mbps is over the {} Mbps main tier of level {:02} {}, \
             so it is a high tier stream",
            level.main_tier_megabits_per_second, level.id, level.name
        ));
    }
}

// a wrapped package carries no RPU, so the base layer signalling and the level
// are what is left to hold it to
pub fn check_package(package: &Path) -> DolbyVisionCompliance {
    let mut result = DolbyVisionCompliance::default();

    // resolve_picture reads a DCP CPL, and an IMP names its picture track
    // differently, so the essence type finds it instead
    let Some(picture) = package_picture(package) else {
        result
            .errors
            .push("no JPEG 2000 picture track file in this package".to_string());
        return result;
    };

    // an AS-DCP wrap is a DCP, whose picture is X'Y'Z' cinema essence. Table 1
    // makes every Dolby Vision base layer HEVC or AVC, and unsignalled colour
    // resolves to Rec.709, so grading one would pass it on a default.
    if !is_as02_jpeg2000(&picture) {
        result.errors.push(format!(
            "{} is a DCP picture track, and a Dolby Vision base layer is HEVC or AVC \
             in table 1, so the profile and level tables do not describe it",
            picture.display()
        ));
        return result;
    }

    let resolved = match resolve_picture(&picture) {
        Ok(resolved) => resolved,
        Err(e) => {
            result.errors.push(format!("no picture to check: {e}"));
            return result;
        }
    };
    let colour = match resolve_picture_colour(&resolved) {
        Ok(colour) => colour,
        Err(e) => {
            result
                .errors
                .push(format!("unreadable picture colour: {e}"));
            return result;
        }
    };

    level_finding(resolved.width, resolved.height, resolved.fps, &mut result);

    let Some(signalling) = signalling_of(colour.transfer, colour.primaries) else {
        result.errors.push(format!(
            "{:?} transfer with {:?} primaries is no Dolby Vision base layer in table 1",
            colour.transfer, colour.primaries
        ));
        return result;
    };
    result.checked.push(format!(
        "base layer VUI {} is a table 1 Dolby Vision base layer",
        signalling_name(signalling)
    ));

    if signalling.transfer_characteristics == HDR10_TRANSFER_CHARACTERISTICS {
        let light_levels = cpl_light_levels(package);
        match light_levels {
            Some((max_content, max_frame_average)) => result.checked.push(format!(
                "the CPL carries MaxCLL {max_content} and MaxFALL {max_frame_average}"
            )),
            None => result.warnings.push(
                "a PQ BT.2020 base layer is cross-compatibility 1, CTA HDR10, and the CPL \
                 carries no MaxCLL and MaxFALL, which V1.2.92 calls PQ10"
                    .to_string(),
            ),
        }
    }

    result
}

fn is_as02_jpeg2000(picture: &Path) -> bool {
    matches!(
        asdcplib::essence_type(&picture.to_string_lossy()),
        Ok(asdcplib::EssenceType::As02Jpeg2000)
    )
}

fn package_picture(package: &Path) -> Option<PathBuf> {
    if !package.is_dir() {
        return Some(package.to_path_buf());
    }
    let mut track_files: Vec<PathBuf> = std::fs::read_dir(package)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("mxf"))
        })
        .collect();
    track_files.sort();
    track_files.into_iter().find(|path| is_jpeg2000_mxf(path))
}

// App 2E puts MaxCLL and MaxFALL in the CPL ExtensionProperties
pub fn cpl_light_levels(package: &Path) -> Option<(String, String)> {
    for entry in std::fs::read_dir(package).ok()?.flatten() {
        let path = entry.path();
        let is_xml = path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("xml"));
        if !is_xml {
            continue;
        }
        let Ok(xml) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !xml.contains("CompositionPlaylist") {
            continue;
        }
        // the CPL binds these to the App 2E namespace
        let max_content = read_prefixed_tag(&xml, "MaxCLL");
        let max_frame_average = read_prefixed_tag(&xml, "MaxFALL");
        if let (Some(max_content), Some(max_frame_average)) = (max_content, max_frame_average) {
            return Some((max_content, max_frame_average));
        }
    }
    None
}

// an HEVC master still has its RPU, so the profile picks the rows table 1 allows
pub fn check_master(master: &Path) -> DolbyVisionCompliance {
    let mut result = DolbyVisionCompliance::default();

    let summary = match read_dolby_vision(master) {
        Ok(Some(summary)) => summary,
        Ok(None) => {
            result
                .errors
                .push("no Dolby Vision RPU in this master".to_string());
            return result;
        }
        Err(e) => {
            result.errors.push(format!("unreadable RPU: {e}"));
            return result;
        }
    };
    result.checked.push(format!(
        "the RPU says Dolby Vision profile {} over {} frames",
        summary.profile, summary.frames
    ));

    let allowed = allowed_base_layer_signalling(summary.profile);
    if allowed.is_empty() {
        result.errors.push(format!(
            "table 1 lists no Dolby Vision profile {}",
            summary.profile
        ));
        return result;
    }

    let Some(probed) = probe_base_layer(master) else {
        result
            .errors
            .push("ffprobe read no video stream off this master".to_string());
        return result;
    };

    level_finding(
        probed.width,
        probed.height,
        probed.frames_per_second,
        &mut result,
    );
    if let (Some(level), Some(megabits_per_second)) = (
        dolby_vision_level_for(probed.width, probed.height, probed.frames_per_second),
        probed.megabits_per_second,
    ) {
        tier_finding(level, megabits_per_second, &mut result);
    }

    base_layer_finding(summary.profile, probed.signalling, &mut result);

    if probed.signalling.transfer_characteristics == HDR10_TRANSFER_CHARACTERISTICS {
        match (
            summary.max_content_light_level_nits,
            summary.max_frame_average_light_level_nits,
        ) {
            (Some(max_content), Some(max_frame_average)) => result.checked.push(format!(
                "the RPU level 6 block carries MaxCLL {max_content} and MaxFALL {max_frame_average}"
            )),
            _ => result.warnings.push(
                "a PQ BT.2020 base layer is cross-compatibility 1, CTA HDR10, and the RPU \
                 carries no level 6 block with MaxCLL and MaxFALL, which V1.2.92 calls PQ10"
                    .to_string(),
            ),
        }
    }

    result
}

struct ProbedBaseLayer {
    width: u32,
    height: u32,
    frames_per_second: f64,
    // absent on a raw elementary stream, which carries no bitrate
    megabits_per_second: Option<f64>,
    signalling: BaseLayerSignalling,
}

const BITS_PER_MEGABIT: f64 = 1_000_000.0;

fn probe_base_layer(master: &Path) -> Option<ProbedBaseLayer> {
    let probed = crate::probe::probe_video(master)?;
    Some(ProbedBaseLayer {
        width: probed.width,
        height: probed.height,
        frames_per_second: f64::from(probed.fps_num) / f64::from(probed.fps_den),
        megabits_per_second: probed.bit_rate.map(|bits| bits as f64 / BITS_PER_MEGABIT),
        signalling: BaseLayerSignalling {
            transfer_characteristics: transfer_code(&probed.color_transfer),
            colour_primaries: primaries_code(&probed.color_primaries),
            matrix_coefficients: matrix_code(&probed.color_space),
            full_range: probed.color_range == "pc",
        },
    })
}

// H.265 VUI code points, 2 is the unspecified both ffprobe and table 1 use
fn transfer_code(name: &str) -> u8 {
    match name {
        "bt709" => 1,
        "smpte2084" => 16,
        "arib-std-b67" => 18,
        _ => 2,
    }
}

fn primaries_code(name: &str) -> u8 {
    match name {
        "bt709" => 1,
        "bt2020" => 9,
        _ => 2,
    }
}

fn matrix_code(name: &str) -> u8 {
    match name {
        "bt709" => 1,
        "bt2020nc" | "bt2020_ncl" => 9,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pq_bt2020_picture_is_the_hdr10_base_layer_table_1_names() {
        let signalling =
            signalling_of(DisplayTransfer::Pq, DisplayPrimaries::Bt2020).expect("a table 1 row");
        assert_eq!(signalling_name(signalling), "16,9,9,0");
        assert!(allowed_base_layer_signalling(8).contains(&signalling));
    }

    #[test]
    fn an_hdr10_base_layer_is_what_table_1_allows_profile_8() {
        let hdr10 =
            signalling_of(DisplayTransfer::Pq, DisplayPrimaries::Bt2020).expect("a table 1 row");
        let mut result = DolbyVisionCompliance::default();
        base_layer_finding(8, hdr10, &mut result);
        assert!(result.errors.is_empty(), "{result:?}");
        assert!(result.warnings.is_empty(), "{result:?}");
        assert_eq!(result.checked.len(), 1, "{result:?}");
        assert!(result.checked[0].contains("16,9,9,0"), "{result:?}");
    }

    #[test]
    fn a_p3d65_picture_is_no_dolby_vision_base_layer() {
        assert!(signalling_of(DisplayTransfer::Pq, DisplayPrimaries::P3D65).is_none());
    }

    #[test]
    fn an_hlg_picture_is_no_dolby_vision_base_layer() {
        assert!(signalling_of(DisplayTransfer::Hlg, DisplayPrimaries::Bt2020).is_none());
    }

    #[test]
    fn ffprobe_colour_names_become_the_vui_code_points() {
        assert_eq!(transfer_code("smpte2084"), 16);
        assert_eq!(transfer_code("arib-std-b67"), 18);
        assert_eq!(primaries_code("bt2020"), 9);
        assert_eq!(matrix_code("bt2020nc"), 9);
        assert_eq!(transfer_code("unknown"), 2);
    }

    // uhd60 is the last row of table 3
    #[test]
    fn a_raster_past_uhd60_is_named_as_past_the_table() {
        let mut result = DolbyVisionCompliance::default();
        level_finding(3840, 2160, 120.0, &mut result);
        assert_eq!(result.checked.len(), 0);
        assert_eq!(result.errors.len(), 1, "{result:?}");
        assert!(result.errors[0].contains("past uhd60"), "{result:?}");
    }

    #[test]
    fn an_unreadable_raster_is_named_as_unread_rather_than_past_the_table() {
        let mut result = DolbyVisionCompliance::default();
        level_finding(0, 0, 0.0, &mut result);
        assert_eq!(result.errors.len(), 1, "{result:?}");
        assert!(result.errors[0].contains("could not be read"), "{result:?}");
        assert!(!result.errors[0].contains("uhd60"), "{result:?}");
    }

    // level 09 uhd60: 40 Mbps main tier, 130 Mbps high tier
    fn uhd60() -> DolbyVisionLevel {
        dolby_vision_level_for(3840, 2160, 60.0).expect("the last row of table 3")
    }

    #[test]
    fn a_main_tier_bitrate_is_no_finding_at_all() {
        let mut result = DolbyVisionCompliance::default();
        tier_finding(uhd60(), 39.0, &mut result);
        assert_eq!(result, DolbyVisionCompliance::default());
    }

    #[test]
    fn a_bitrate_between_the_tiers_warns_that_it_is_a_high_tier_stream() {
        let mut result = DolbyVisionCompliance::default();
        tier_finding(uhd60(), 90.0, &mut result);
        assert!(result.errors.is_empty(), "{result:?}");
        assert_eq!(result.warnings.len(), 1, "{result:?}");
        assert!(result.warnings[0].contains("09 uhd60"), "{result:?}");
    }

    #[test]
    fn a_bitrate_over_the_high_tier_is_an_error() {
        let mut result = DolbyVisionCompliance::default();
        tier_finding(uhd60(), 131.0, &mut result);
        assert!(result.warnings.is_empty(), "{result:?}");
        assert_eq!(result.errors.len(), 1, "{result:?}");
        assert!(result.errors[0].contains("09 uhd60"), "{result:?}");
    }
}
