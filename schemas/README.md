# Vendored schemas

Only what `kdm_required_extensions_pass_the_st_430_1_xsd` needs to validate a
generated KDM offline. `SMPTE-430-1-2006-KDM.xsd` imports the ETM schema, which
imports xenc, which imports xmldsig. The two W3C schemas are referenced by their
published URL, so `catalog.xml` maps those URLs to the local copies and the test
passes `XML_CATALOG_FILES` and `--nonet`.

`SMPTE-430-3-2006-ETM.xsd` carries a fix: the published transcription wraps the
`UUID` pattern across a line break in the middle of a character class, which
libxml2 reads as a literal space and then rejects as an invalid regex.
