use postkit::colour::{ColourSpace, DcdmTransform};
use postkit::dcdm::*;

#[test]
fn dcdm_end_to_end_writes_xyz_tiffs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    // three white 64x64 source frames
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=white:s=64x64:d=1:r=3",
            "-frames:v",
            "3",
        ])
        .arg(src.join("f_%03d.png"))
        .output()
        .expect("ffmpeg");
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );

    let out = dir.path().join("out");
    let result = create_dcdm(&DcdmOptions {
        input_dir: src,
        output_dir: out.clone(),
        encoding: DcdmColourEncoding::Xyz12Bit,
        width: 0,
        height: 0,
        colour_space: "rec709".into(),
        ..Default::default()
    });
    assert!(result.success, "{}", result.error);
    assert_eq!(result.frames_written, 3);

    let f = std::fs::File::open(out.join("dcdm_000000.tif")).unwrap();
    let mut dec = tiff::decoder::Decoder::new(f).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (64, 64));
    let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
        panic!("expected 16-bit samples");
    };
    // near-white lands near the DCI reference white code value, and is NOT the
    // untransformed rgb48 white the old passthrough produced
    assert!((px[1] as i32 - 3960).abs() < 60, "Y' = {}", px[1]);
    assert!(
        px[0] < px[1] && px[1] < px[2],
        "XYZ ordering: {:?}",
        &px[..3]
    );
    assert!(px.iter().all(|v| *v <= 4095), "12-bit code values");
}

/// The public per-frame transform must produce exactly what the file pipeline
/// writes, so the encoder and `create_dcdm` cannot drift apart.
#[test]
fn the_public_transform_reproduces_a_written_dcdm_frame() {
    const SIZE: u32 = 32;

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let frame = src.join("f_001.png");
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s={SIZE}x{SIZE}"),
            "-frames:v",
            "1",
        ])
        .arg(&frame)
        .output()
        .expect("ffmpeg");
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );

    for (name, space) in [
        ("p3", ColourSpace::P3),
        ("rec2020", ColourSpace::Rec2020),
        ("rec709", ColourSpace::Rec709),
    ] {
        let out = dir.path().join(format!("out_{name}"));
        let result = create_dcdm(&DcdmOptions {
            input_dir: src.clone(),
            output_dir: out.clone(),
            encoding: DcdmColourEncoding::Xyz12Bit,
            width: SIZE,
            height: SIZE,
            colour_space: name.into(),
            ..Default::default()
        });
        assert!(result.success, "{}", result.error);

        let f = std::fs::File::open(out.join("dcdm_000000.tif")).unwrap();
        let mut dec = tiff::decoder::Decoder::new(f).unwrap();
        let tiff::decoder::DecodingResult::U16(written) = dec.read_image().unwrap() else {
            panic!("expected 16-bit samples");
        };

        // the same source frame as rgb48le, the layout the transform takes
        let raw = std::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(&frame)
            .args(["-pix_fmt", "rgb48le", "-f", "rawvideo", "pipe:1"])
            .stderr(std::process::Stdio::null())
            .output()
            .expect("ffmpeg");
        assert!(
            raw.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&raw.stderr)
        );

        let mut expected = vec![0u16; written.len()];
        DcdmTransform::to_xyz(space)
            .unwrap()
            .frame_rgb48le(&raw.stdout, 4095, &mut expected);
        assert_eq!(expected, written, "{name} frame differs from create_dcdm");
    }
}
