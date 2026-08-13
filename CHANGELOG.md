# Changelog

## 0.6.0 - 2026-08-13

### Generated signer certificates were missing two required extensions

Regenerate any chain postkit produced earlier. Every leaf certificate came out
with no Basic Constraints and no Key Usage extension, which ST 430-2 requires,
so validators reject a package signed with one. rcgen writes no extensions at
all for `IsCa::NoCa`, and the leaf was the only certificate using it, so root
and intermediate were always correct and only the signer was affected.

### KDMs written before this release were schema-invalid

Regenerate any KDM postkit produced earlier. Three defects broke it against the
ST 430-1 schema, and each one on its own is enough for a conformant consumer to
reject the message.

- No `AuthorizedDeviceInfo` at all. ST 430-1 Annex B declares it with no
  `minOccurs`, so it is required, and postkit never wrote one.
- `Recipient/X509IssuerSerial` children were written unprefixed. The element is
  typed `ds:X509IssuerSerialType`, so `X509IssuerName` and `X509SerialNumber`
  belong in the xmldsig namespace.
- The ETM `Signer` carried a third `X509SubjectName` child. ST 430-3 types it as
  `ds:X509IssuerSerialType`, which permits issuer and serial only.

The `Recipient`'s own `X509SubjectName` was always correct and is unchanged. It
is a sibling of `X509IssuerSerial`, not a child of it.

### Certificates generated before this release fail DCI CTP 2.1.4

Regenerate them too. `generate_certificate` never set a serial number, so rcgen
fell back to 20 bytes of a public key hash. ST 430-2 5.2 requires an unsigned
integer of 64 bits or less, and CTP 2.1.4 fails anything larger. Serials are now
random 63-bit values.

### Added

- `KdmConfig.device_cert_files` and `RewrapConfig.device_cert_files` restrict a
  KDM to named playback devices by certificate thumbprint. Empty, the default,
  emits the DCI assume-trust thumbprint alone.
- `AccessibilityTrack::VisuallyImpairedText` for the ST 2067-2
  `VisuallyImpairedTextSequence`, separate from `AudioDescription` because it is
  text a renderer speaks rather than a narration channel carried as audio.

### Changed

- `AccessibilityTrack` is `#[non_exhaustive]`, so matches in other crates need a
  wildcard arm.
- `dci_max_bitrate_mbps(width)` is replaced by the constant
  `DCI_MAX_BITRATE_MBPS`, and the 4K limit drops from 500 Mb/s to 250. DCSS 4.3.3
  caps a 4K frame at the same 1,302,083 bytes as 24 fps 2K, and the 500 has no
  source in DCI, ST 429-4 or ST 429-2. A 4K package between 250 and 500 Mb/s that
  passed before is now reported over the limit.
- `analyse_bitrate` lost its `width` parameter, which fed only that branch.
