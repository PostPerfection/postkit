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
