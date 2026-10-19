use super::decorrelation::DIRECT_PATH_DELAY;
use super::decorrelation::decorrelation_filters;
use super::direct_speakers::bed_channel_gains;
use super::panner::{Cartesian, ObjectPanner, ObjectParameters, ZONE_BOXES, in_excluded_zone};
use super::parse::split_frames;
use super::render::{adm_position, object_parameters};
use super::tests::{fixture_directory, read_zipped_stream};
use super::*;
use std::collections::BTreeMap;

const ORACLE_TOLERANCE: f64 = 1e-5;

fn ear_reference() -> serde_json::Value {
    let text = std::fs::read_to_string(fixture_directory().join("ear_gains.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn layout_named(name: &str) -> OutputLayout {
    match name {
        "0+2+0" => OutputLayout::Stereo,
        "0+5+0" => OutputLayout::FivePointOne,
        "0+7+0" => OutputLayout::SevenPointOne,
        other => panic!("no output layout {other}"),
    }
}

fn numbers(value: &serde_json::Value) -> Vec<f64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|number| number.as_f64().unwrap())
        .collect()
}

fn largest_difference(ours: &[f64], reference: &[f64]) -> f64 {
    assert_eq!(ours.len(), reference.len());
    ours.iter()
        .zip(reference)
        .map(|(ours, reference)| (ours - reference).abs())
        .fold(0.0, f64::max)
}

#[test]
fn the_oracle_uses_the_renderer_zones_and_channel_orders() {
    let reference = ear_reference();
    let zones: Vec<Vec<f64>> = reference["zones"]
        .as_array()
        .unwrap()
        .iter()
        .map(numbers)
        .collect();
    let ours: Vec<Vec<f64>> = ZONE_BOXES.iter().map(|zone| zone.to_vec()).collect();
    assert_eq!(zones, ours);
    for (name, labels) in reference["layouts"].as_object().unwrap() {
        let ours: Vec<&str> = layout_named(name)
            .loudspeakers()
            .iter()
            .map(|speaker| speaker.label)
            .collect();
        let labels: Vec<&str> = labels
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap())
            .collect();
        assert_eq!(ours, labels, "{name}");
    }
}

#[test]
fn object_gains_match_the_ear_oracle() {
    let reference = ear_reference();
    let mut panners: Vec<(OutputLayout, ObjectPanner)> = Vec::new();
    let mut failures = Vec::new();
    let cases = reference["objects"].as_array().unwrap();
    for case in cases {
        let layout = layout_named(case["layout"].as_str().unwrap());
        if !panners.iter().any(|(held, _)| *held == layout) {
            panners.push((layout, ObjectPanner::new(layout.loudspeakers())));
        }
        let panner = &mut panners
            .iter_mut()
            .find(|(held, _)| *held == layout)
            .unwrap()
            .1;
        let position = numbers(&case["position"]);
        let mut excluded_zones = [false; 9];
        for zone in case["excluded_zones"].as_array().unwrap() {
            excluded_zones[zone.as_u64().unwrap() as usize] = true;
        }
        let parameters = ObjectParameters {
            position: Cartesian {
                x: position[0],
                y: position[1],
                z: position[2],
            },
            extent: case["extent"].as_f64().unwrap(),
            excluded_zones,
            channel_lock: case["snap"].as_bool().unwrap(),
            diffuse: case["diffuse"].as_f64().unwrap() == 1.0,
        };
        let gains = panner.gains(&parameters);
        let direct = largest_difference(&gains.direct, &numbers(&case["direct_gains"]));
        let diffuse = largest_difference(&gains.diffuse, &numbers(&case["diffuse_gains"]));
        if direct.max(diffuse) > ORACLE_TOLERANCE {
            failures.push(format!(
                "{case} ours {:?} {:?}",
                gains.direct, gains.diffuse
            ));
        }
    }
    assert!(cases.len() > 800, "{} cases", cases.len());
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures[..failures.len().min(20)].join("\n")
    );
}

#[test]
fn bed_channel_gains_match_the_ear_oracle() {
    let reference = ear_reference();
    let cases = reference["direct_speakers"].as_array().unwrap();
    assert_eq!(cases.len(), 30);
    for case in cases {
        let layout = layout_named(case["layout"].as_str().unwrap());
        let channel_id = case["channel_id"].as_u64().unwrap() as u32;
        let ours = bed_channel_gains(layout.loudspeakers(), channel_id).unwrap();
        let difference = largest_difference(&ours, &numbers(&case["gains"]));
        assert!(difference <= ORACLE_TOLERANCE, "{case} ours {ours:?}");
    }
}

#[test]
fn decorrelation_filters_match_the_ear_oracle() {
    let reference = ear_reference();
    let filters = decorrelation_filters(OutputLayout::SevenPointOne.loudspeakers());
    let expected = reference["decorrelation_filters"].as_array().unwrap();
    assert_eq!(filters.len(), expected.len());
    for (channel, (ours, expected)) in filters.iter().zip(expected).enumerate() {
        let ours: Vec<f64> = ours.iter().map(|tap| f64::from(*tap)).collect();
        let difference = largest_difference(&ours, &numbers(expected));
        assert!(
            difference <= ORACLE_TOLERANCE,
            "channel {channel} differs by {difference}"
        );
    }
}

#[test]
fn cube_codes_map_to_adm_coordinates() {
    let front_left_floor = adm_position(32_767, 32_767, 0);
    assert_eq!(
        front_left_floor,
        Cartesian {
            x: -1.0,
            y: 1.0,
            z: 0.0
        }
    );
    let back_right_ceiling = adm_position(65_535, 65_535, 65_535);
    assert_eq!(
        back_right_ceiling,
        Cartesian {
            x: 1.0,
            y: -1.0,
            z: 1.0
        }
    );
    let middle = adm_position(49_151, 49_151, 0);
    assert_eq!((middle.x, middle.y), (0.0, 0.0));
}

fn zone_members(layout: OutputLayout, zone: usize) -> Vec<&'static str> {
    let mut excluded_zones = [false; 9];
    excluded_zones[zone] = true;
    layout
        .loudspeakers()
        .iter()
        .filter(|speaker| !speaker.is_lfe && in_excluded_zone(speaker, &excluded_zones))
        .map(|speaker| speaker.label)
        .collect()
}

#[test]
fn each_zone_holds_the_loudspeakers_of_its_part_of_the_room() {
    let seven_one: Vec<Vec<&str>> = (0..9)
        .map(|zone| zone_members(OutputLayout::SevenPointOne, zone))
        .collect();
    assert_eq!(
        seven_one,
        vec![
            vec!["M+030"],
            vec!["M+000"],
            vec!["M-030"],
            vec!["M+090"],
            vec!["M-090"],
            vec!["M+135"],
            vec!["M-135"],
            vec![],
            vec![],
        ]
    );
    let five_one: Vec<Vec<&str>> = (0..9)
        .map(|zone| zone_members(OutputLayout::FivePointOne, zone))
        .collect();
    assert_eq!(
        five_one,
        vec![
            vec!["M+030"],
            vec!["M+000"],
            vec!["M-030"],
            vec![],
            vec![],
            vec!["M+110"],
            vec!["M-110"],
            vec![],
            vec![],
        ]
    );
}

// 29 has RDD 57 Table 4 zone set 1a, 27 snapped objects and 35 maximum decorrelation
const PROFILE_1_STREAMS: [u32; 4] = [1, 27, 29, 35];
const LAYOUTS: [OutputLayout; 3] = [
    OutputLayout::Stereo,
    OutputLayout::FivePointOne,
    OutputLayout::SevenPointOne,
];

fn stream_frames(number: u32) -> Vec<Frame> {
    let bytes = read_zipped_stream(
        &fixture_directory().join(format!("IABValidationStream_{number}.iab.zip")),
    );
    split_frames(&bytes)
        .unwrap()
        .into_iter()
        .map(|frame| parse_frame(frame).unwrap())
        .collect()
}

#[test]
fn every_frame_of_the_profile_1_streams_renders_on_every_layout() {
    for number in PROFILE_1_STREAMS {
        let frames = stream_frames(number);
        assert_eq!(frames.len(), 48, "stream {number}");
        for layout in LAYOUTS {
            let mut renderer = Renderer::new(layout);
            let mut energy = 0.0f64;
            for (index, frame) in frames.iter().enumerate() {
                let audio = decode_audio(frame).unwrap();
                let output = renderer
                    .render(frame, &audio)
                    .unwrap_or_else(|error| panic!("stream {number} frame {index}: {error}"));
                assert_eq!(output.len(), 2000 * layout.channel_count());
                assert!(output.iter().all(|sample| sample.is_finite()));
                energy += output
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>();
            }
            assert!(
                energy > 0.0,
                "stream {number} renders silence on {layout:?}"
            );
        }
    }
}

#[test]
fn maximum_decorrelation_reaches_the_output_through_the_filters() {
    let frames = stream_frames(35);
    assert!(frames.iter().any(|frame| {
        frame
            .objects
            .iter()
            .any(|object| object.pan_sub_blocks[0].decorrelation == Decorrelation::Maximum)
    }));
    let mut renderer = Renderer::new(OutputLayout::SevenPointOne);
    let mut decorrelated_only = audio_of_decorrelated_objects_only(&frames);
    let mut energy = 0.0f64;
    for (frame, audio) in frames.iter().zip(decorrelated_only.iter_mut()) {
        let output = renderer.render(frame, audio).unwrap();
        energy += output
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>();
    }
    assert!(energy > 0.0);
}

// silences every bed channel and every object not decorrelated in all its sub-blocks
fn audio_of_decorrelated_objects_only(frames: &[Frame]) -> Vec<BTreeMap<u32, Vec<i32>>> {
    frames
        .iter()
        .map(|frame| {
            let mut audio = decode_audio(frame).unwrap();
            let kept: Vec<u32> = frame
                .objects
                .iter()
                .filter(|object| {
                    object
                        .pan_sub_blocks
                        .iter()
                        .all(|pan| pan.decorrelation == Decorrelation::Maximum)
                })
                .map(|object| object.audio_data_id)
                .collect();
            for (audio_data_id, samples) in audio.iter_mut() {
                if !kept.contains(audio_data_id) {
                    samples.fill(0);
                }
            }
            audio
        })
        .collect()
}

#[test]
fn a_silent_frame_renders_silence() {
    let frame = &stream_frames(1)[0];
    let mut audio = decode_audio(frame).unwrap();
    for samples in audio.values_mut() {
        samples.fill(0);
    }
    for layout in LAYOUTS {
        let output = Renderer::new(layout).render(frame, &audio).unwrap();
        assert!(output.iter().all(|sample| *sample == 0.0), "{layout:?}");
    }
}

#[test]
fn reset_clears_the_delayed_and_filtered_samples() {
    let frames = stream_frames(35);
    let mut renderer = Renderer::new(OutputLayout::FivePointOne);
    for frame in &frames[..4] {
        renderer
            .render(frame, &decode_audio(frame).unwrap())
            .unwrap();
    }
    renderer.reset();
    let mut silent = decode_audio(&frames[4]).unwrap();
    for samples in silent.values_mut() {
        samples.fill(0);
    }
    let output = renderer.render(&frames[4], &silent).unwrap();
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn no_sample_step_across_a_sub_block_exceeds_the_interpolated_gain_change() {
    const LEVEL: i32 = 1 << 29;
    let level = LEVEL as f64 / 2_147_483_648.0;
    let frames = stream_frames(1);
    assert_eq!(frames[0].frame_rate.pan_sub_block_count(), 8);
    let layout = OutputLayout::FivePointOne;
    let channel_count = layout.channel_count();
    let mut renderer = Renderer::new(layout);
    let mut panner = ObjectPanner::new(layout.loudspeakers());
    // no Profile 1 fixture moves an object, so this sweeps one from the left wall to the right front
    let mut frames = frames;
    let meta_id = frames[0].objects[0].meta_id;
    let sweep_length = (frames.len() * 8) as f64;
    for (frame_index, frame) in frames.iter_mut().enumerate() {
        let object = frame
            .objects
            .iter_mut()
            .find(|object| object.meta_id == meta_id)
            .unwrap();
        for (sub_block, pan) in object.pan_sub_blocks.iter_mut().enumerate() {
            let progress = (frame_index * 8 + sub_block) as f64 / sweep_length;
            pan.position.x = 32_767 + (32_768.0 * progress) as u16;
            pan.position.y = 32_767 + (16_384.0 * (1.0 - progress)) as u16;
        }
    }
    let mut output = Vec::new();
    // per channel, the largest gain change from one sample to the next
    let mut largest_step = vec![0.0f64; channel_count];
    let mut previous_gains: Option<Vec<f64>> = None;
    let mut changed = false;
    for frame in &frames {
        let object = frame
            .objects
            .iter()
            .find(|object| object.meta_id == meta_id)
            .unwrap();
        let mut audio = decode_audio(frame).unwrap();
        for (audio_data_id, samples) in audio.iter_mut() {
            samples.fill(if *audio_data_id == object.audio_data_id {
                LEVEL
            } else {
                0
            });
        }
        output.extend(renderer.render(frame, &audio).unwrap());
        for (pan, length) in object
            .pan_sub_blocks
            .iter()
            .zip(frame.frame_rate.pan_sub_block_lengths(frame.sample_rate))
        {
            let parameters = object_parameters(pan, meta_id).unwrap();
            let gains = panner.gains(&parameters).direct;
            let from = previous_gains.clone().unwrap_or_else(|| gains.clone());
            for channel in 0..channel_count {
                let step = (gains[channel] - from[channel]).abs() / length as f64;
                changed |= step > 0.0;
                largest_step[channel] = largest_step[channel].max(step);
            }
            previous_gains = Some(gains);
        }
    }
    assert!(changed, "the object never moves");
    let sample_count = output.len() / channel_count;
    for channel in 0..channel_count {
        let bound = level * largest_step[channel] + 1e-6;
        // the delayed direct path starts with a step from silence
        for index in DIRECT_PATH_DELAY + 1..sample_count {
            let step = output[index * channel_count + channel]
                - output[(index - 1) * channel_count + channel];
            assert!(
                f64::from(step).abs() <= bound,
                "channel {channel} sample {index} steps {step}, the gains change by {bound} a sample"
            );
        }
    }
}

fn error_with_changed_pan(change: &dyn Fn(&mut ObjectPan)) -> IabError {
    let mut frame = stream_frames(1).swap_remove(0);
    change(&mut frame.objects[0].pan_sub_blocks[3]);
    let audio = decode_audio(&frame).unwrap();
    Renderer::new(OutputLayout::Stereo)
        .render(&frame, &audio)
        .unwrap_err()
}

fn excluded_feature(error: IabError) -> ExcludedFeature {
    match error {
        IabError::OutsideProfile1 { feature, .. } => feature,
        other => panic!("{other} is not an excluded feature"),
    }
}

#[test]
fn features_outside_profile_1_return_the_named_error() {
    let pan_error =
        |change: &dyn Fn(&mut ObjectPan)| excluded_feature(error_with_changed_pan(change));
    assert_eq!(
        pan_error(&|pan| pan.spread = ObjectSpread::LowResolution(3)),
        ExcludedFeature::LowResolutionSpread
    );
    assert_eq!(
        pan_error(&|pan| pan.spread = ObjectSpread::ThreeDimensions { x: 1, y: 2, z: 3 }),
        ExcludedFeature::ThreeDimensionalSpread
    );
    assert_eq!(
        pan_error(&|pan| pan.snap = ObjectSnap::Tolerance(100)),
        ExcludedFeature::ObjectSnapTolerance
    );
    assert_eq!(
        pan_error(&|pan| pan.decorrelation = Decorrelation::Coded(128)),
        ExcludedFeature::ObjectDecorrelationCoefficient
    );
    assert_eq!(
        pan_error(&|pan| pan.zone_gains = Some([ZoneGain::Coded(512); 9])),
        ExcludedFeature::ZoneGainCoefficient
    );

    let render_changed = |change: &dyn Fn(&mut Frame)| {
        let mut frame = stream_frames(1).swap_remove(0);
        change(&mut frame);
        let audio = decode_audio(&frame).unwrap();
        Renderer::new(OutputLayout::SevenPointOne).render(&frame, &audio)
    };
    let error = render_changed(&|frame| frame.beds[0].channels[1].channel_id = 0x1).unwrap_err();
    assert_eq!(excluded_feature(error), ExcludedFeature::ChannelId(0x1));
    let error = render_changed(&|frame| {
        frame.beds[0].channels[0].decorrelation = Some(Decorrelation::Maximum)
    })
    .unwrap_err();
    assert_eq!(
        excluded_feature(error),
        ExcludedFeature::BedChannelDecorrelation
    );
    let error = render_changed(&|frame| {
        frame.beds[0].remaps.push(BedRemap {
            meta_id: 9,
            use_case: 0x01,
            source_channel_count: 10,
            destination_channel_count: 6,
            sub_blocks: Vec::new(),
        })
    })
    .unwrap_err();
    assert_eq!(excluded_feature(error), ExcludedFeature::BedRemap);
}

#[test]
fn a_stream_outside_profile_1_returns_the_named_error() {
    let frame = &stream_frames(41)[0];
    let error = Renderer::new(OutputLayout::SevenPointOne)
        .render(frame, &decode_audio(frame).unwrap())
        .unwrap_err();
    assert!(matches!(
        error,
        IabError::OutsideProfile1 {
            element: "ObjectDefinition",
            feature: ExcludedFeature::ObjectZoneDefinition19,
            ..
        }
    ));
}

// one object alone, conditional on use_case, and how loud each layout renders it
fn rendered_energy(use_case: Option<u8>, layout: OutputLayout) -> f64 {
    let mut frame = stream_frames(1).swap_remove(0);
    frame.beds.clear();
    frame.objects.truncate(1);
    frame.objects[0].use_case = use_case;
    let audio = decode_audio(&frame).unwrap();
    Renderer::new(layout)
        .render(&frame, &audio)
        .unwrap()
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum()
}

#[test]
fn a_conditional_object_renders_only_for_its_use_case() {
    let use_cases = [
        (OutputLayout::Stereo, 0x30),
        (OutputLayout::FivePointOne, 0x01),
        (OutputLayout::SevenPointOne, 0x02),
    ];
    for (layout, own_use_case) in use_cases {
        assert!(rendered_energy(None, layout) > 0.0);
        assert!(rendered_energy(Some(0xFF), layout) > 0.0);
        assert!(
            rendered_energy(Some(own_use_case), layout) > 0.0,
            "{layout:?}"
        );
        for (_, other) in use_cases
            .iter()
            .filter(|(other_layout, _)| *other_layout != layout)
        {
            assert_eq!(
                rendered_energy(Some(*other), layout),
                0.0,
                "{layout:?} {other:#x}"
            );
        }
    }
}
