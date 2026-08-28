# Validation schemas

The XSDs the packaging tests validate generated documents against, so those
tests need nothing installed beyond xmllint. `dcp/` was copied from
`dcpdoctor/schemas`: the SMPTE 429-7 CPL, 429-8 PKL and 429-9 ASSETMAP schemas,
the Interop `PROTO-ASDCP-CPL-20040511.xsd` and its `437-Y` stereo-picture
import, and local copies of `xmldsig-core-schema.xsd` and `xml.xsd` that the
tests' XML catalog resolves the http references to. `imf/` was copied from
Photon (`dcpdoctor/extern/photon/src/main/resources/org`) with its directory
layout intact, because `packingList_schema.xsd`, `imf-cpl-20160411.xsd` and
`dcmlTypes.xsd` reach each other and the xmldsig schema by relative path.
`POSTKIT_DCP_XSD_DIR`, `POSTKIT_IMF_PKL_XSD` and `IMFWIZARD_IMF_XSD_DIR` each
point their test at a different copy.
