# ECL signed CPLs and PKLs

The CPL and PKL documents of the ECL set from
[github.com/ClairMeta/ClairMeta_Data](https://github.com/ClairMeta/ClairMeta_Data)
at commit `a78c4cbf86bb31388180cdfa7652ed7368c614cc`, copied with their
`DCP/ECL-SET/<package>/` layout intact so `POSTKIT_CLAIRMETA_DATA` can point the
same test at a full clone. These are real published DCPs signed by a third
party, which is what `real_ecl_dcps_verify` needs: the ECL documents are SHA-1
signed, and before postkit dispatched on the signature algorithm they all
falsely failed. Only the XML is here. The essence, the MXFs and the rest of the
corpus are 1.5 GB and no test reads them.

`LICENSE` beside this file is ClairMeta_Data's own BSD 3-clause licence, which
these documents are redistributed under.
