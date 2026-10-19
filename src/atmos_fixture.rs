use crate::packaging::{ImfResource, write_resource};

pub(crate) const FRAMES_PER_SECOND: u32 = 24;
const IAB_TRACK_ID: &str = "a7000000-0000-4000-8000-00000000a000";
const REEL_ASSET_LIST_END: &str = "      </AssetList>\n";
const SEGMENT_SEQUENCE_LIST_END: &str = "      </SequenceList>\n";

// the AuxData dcpwizard adds to a reel of the CPL postkit's writer wrote, its edit rate 24 fps
pub(crate) struct AtmosReelAsset {
    pub(crate) id: String,
    pub(crate) intrinsic_duration: u64,
    pub(crate) entry_point: u64,
    pub(crate) duration: u64,
    pub(crate) key_id: Option<String>,
}

pub(crate) fn with_atmos_reels(cpl: &str, atmos_by_reel: &[Option<AtmosReelAsset>]) -> String {
    let reels: Vec<&str> = cpl.split(REEL_ASSET_LIST_END).collect();
    assert_eq!(reels.len(), atmos_by_reel.len() + 1, "one AssetList a reel");
    let mut spliced = String::new();
    for (reel, atmos) in reels.iter().zip(atmos_by_reel) {
        spliced.push_str(reel);
        if let Some(atmos) = atmos {
            spliced.push_str(&aux_data(atmos));
        }
        spliced.push_str(REEL_ASSET_LIST_END);
    }
    spliced.push_str(reels[atmos_by_reel.len()]);
    spliced
}

fn aux_data(atmos: &AtmosReelAsset) -> String {
    let key_id = atmos
        .key_id
        .as_ref()
        .map(|key_id| format!("          <KeyId>urn:uuid:{key_id}</KeyId>\n"))
        .unwrap_or_default();
    format!(
        "        <axd:AuxData xmlns:axd=\"http://www.dolby.com/schemas/2012/AD\">\n\
         \x20         <Id>urn:uuid:{}</Id>\n\
         \x20         <EditRate>{FRAMES_PER_SECOND} 1</EditRate>\n\
         \x20         <IntrinsicDuration>{}</IntrinsicDuration>\n\
         \x20         <EntryPoint>{}</EntryPoint>\n\
         \x20         <Duration>{}</Duration>\n\
         {key_id}\
         \x20         <axd:DataType>urn:smpte:ul:060e2b34.04010105.0e090604.00000000</axd:DataType>\n\
         \x20       </axd:AuxData>\n",
        atmos.id, atmos.intrinsic_duration, atmos.entry_point, atmos.duration
    )
}

// the IABSequence imfwizard adds to each segment of the CPL postkit's writer wrote
pub(crate) fn with_iab_sequences(
    cpl: &str,
    iab_by_segment: &[Option<ImfResource>],
    (fps_num, fps_den): (u32, u32),
) -> String {
    let segments: Vec<&str> = cpl.split(SEGMENT_SEQUENCE_LIST_END).collect();
    assert_eq!(
        segments.len(),
        iab_by_segment.len() + 1,
        "one SequenceList a segment"
    );
    let mut spliced = String::new();
    for (segment, resource) in segments.iter().zip(iab_by_segment) {
        spliced.push_str(segment);
        if let Some(resource) = resource {
            let mut resource_xml = String::new();
            write_resource(&mut resource_xml, resource, fps_num, fps_den);
            spliced.push_str(&format!(
                "        <iab:IABSequence xmlns:iab=\"http://www.smpte-ra.org/ns/2067-201/2019\">\n\
                 \x20         <Id>urn:uuid:{}</Id>\n\
                 \x20         <TrackId>urn:uuid:{IAB_TRACK_ID}</TrackId>\n\
                 \x20         <ResourceList>\n\
                 {resource_xml}\
                 \x20         </ResourceList>\n\
                 \x20       </iab:IABSequence>\n",
                uuid::Uuid::new_v4()
            ));
        }
        spliced.push_str(SEGMENT_SEQUENCE_LIST_END);
    }
    spliced.push_str(segments[iab_by_segment.len()]);
    spliced
}
