use postkit::dcdm::*;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn dcdm_end_to_end_writes_xyz_tiffs() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

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
