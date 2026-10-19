# Test codestreams

Real JPEG 2000 codestreams, so a test can parse a SIZ, COD and QCD marker rather
than the synthetic SOC+SIZ stubs the older tests build.

- `cinema2k_64x64.j2c`: Rsiz 0x0003 (DCI Cinema 2K), 64x64, 3 components, 12-bit
  X'Y'Z'. Made with `grk_compress -w 24 --xyz` from a 64x64 ffmpeg `testsrc2`
  frame.
- `imf4k_black_3840x2160.j2c`: Rsiz 0x0536 (IMF 4K, mainlevel 6, sublevel 3),
  3840x2160, 3 components, 12-bit RGB 4:4:4. Frame 0 of the picture track of
  Netflix Open Content's Sol Levante IMF
  (`SolLevante_IMF_DolbyVision_PQP3D65_UHD_24fps`), CC BY 4.0. It is a black
  leader frame, so it is 6.5 KB.

# Facility List Messages

- `flm/st430-16b-2017.xml`: the example instance published with SMPTE ST
  430-16:2017 (`st430-16-20170714-pub.zip` on pub.smpte.org), unchanged, under
  the BSD-style notice it carries. Its SM device chain is a CS role leaf that
  expired in 2025, and the intermediate certificate is cut short (1152 of its
  1155 DER bytes), so it parses but fails the certificate checks.

# IAB validation streams

- `iab/IABValidationStream_N.iab.zip` and `iab/StreamN_cA.json`: streams 1, 16,
  27, 29, 35, 38, 41, 43, 49, 50, 59, 62, 91, 126, 127, 128 and 129 of the 148 in DTS's
  [iab-validator](https://github.com/DTSProAudio/iab-validator) at commit
  694b378 (`test/bitstreams` and `test/reference/cA`), unchanged, under the MIT
  licence in `iab/LICENSE-iab-validator`. 1 has beds, objects and DLC, 16 a bed
  remap, 27 snapped objects, 29 the nine zone gains, 35 maximum object decorrelation, 38 child objects, 41 ObjectZoneDefinition19, 50
  96 kHz DLC, 59 96 fps DLC, 91 48 kHz PCM, 126 an undefined sub-element, 127
  96 kHz PCM, 128 16-bit PCM and 129 PCM at 23.976 fps. The reference marks 43
  (96 kHz DLC in a 48 kHz frame) and 129 invalid and both still parse. 49
  (Version 2) and 62 (DLC at 23.976 fps) fail to parse at the first frame in
  IABLib and in postkit. 1, 27, 29 and 35 meet SMPTE RDD 57 IAB Application
  Profile 1, the set the renderer tests play.
- `iab/iablib_decoded_digests_sha256.txt`: for each of the 148 streams, the
  SHA-256 of what `iab/dump_decoded_digests.cpp` prints for it. The program
  prints one line per frame and AudioDataID with the sample count and the FNV-1a
  64 digest of IABLib's decoded samples as little-endian int32, then a
  `rejected` line at the frame where IABLib fails to parse or decode, or where
  two elements share an AudioDataID. Made against
  [IABLib](https://github.com/DTSProAudio/iab-renderer) at commit 1232c55
  (BSD-3-Clause), with the streams `scripts/fetch-iab-validation-streams.sh`
  fetches in `streams`:

  ```bash
  cmake -S iab-renderer -B iab-renderer-build -DCMAKE_BUILD_TYPE=Release
  cmake --build iab-renderer-build --target IABLib
  g++ -std=c++11 -O2 -I iab-renderer/include -I iab-renderer/src/lib \
    dump_decoded_digests.cpp iab-renderer-build/libIABLib.a -o dump
  for n in $(seq 1 148); do
    unzip -p streams/IABValidationStream_$n.iab.zip > stream.iab
    printf 'IABValidationStream_%s %s\n' $n \
      "$(./dump IABValidationStream_$n stream.iab | sha256sum | cut -d' ' -f1)"
  done > iablib_decoded_digests_sha256.txt
  ```
- `iab/ear_gains.json`: the gains EBU's
  [ear](https://github.com/ebu/ebu_adm_renderer) 2.1.0 (BSD-3-Clause-Clear), the
  ITU-R BS.2127 reference renderer, computes for the object, bed channel and
  decorrelation filter cases in `iab/ear_gains.py`, made with:

  ```bash
  uv venv --python 3.12 ear-oracle
  uv pip install --python ear-oracle/bin/python ear==2.1.0 'setuptools<81'
  ear-oracle/bin/python -I -W ignore iab/ear_gains.py > iab/ear_gains.json
  ```
