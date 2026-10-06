// file and ZIP names from the 2009 KDM Naming Convention, and the per cinema ZIP
use super::formulation::ContentStandard;
use super::issue::IssuedKdm;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

const KDM_NAME_PREFIX: &str = "k";
const FIELD_SEPARATOR: &str = "_";
const WORD_SEPARATOR: char = '-';
const TITLE_LENGTH: usize = 14;
const THEATRE_NAME_LENGTH: usize = 20;
const SERIAL_NUMBER_LENGTH: usize = 20;
const CREATION_FACILITY_LENGTH: usize = 3;
const NAME_DATE_FORMAT: &str = "%Y%m%d";
const THREE_D_MARKER: &str = "3D";
const KDM_EXTENSION: &str = ".xml";
const ZIP_EXTENSION: &str = ".zip";

// ISDCF Digital Cinema Naming Convention field positions in a CPL ContentTitleText
const DCNC_FIELD_COUNT: usize = 12;
const DCNC_TITLE: usize = 0;
const DCNC_CONTENT_TYPE: usize = 1;
const DCNC_LANGUAGE: usize = 3;
const DCNC_TERRITORY_RATING: usize = 4;
const DCNC_AUDIO: usize = 5;
const DCNC_STANDARD: usize = 10;
const DCNC_PACKAGE_TYPE: usize = 11;
const DCNC_SMPTE: &str = "SMPTE";
const DCNC_INTEROP: &str = "IOP";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmNameFields {
    pub title: String,
    pub content_type: Option<String>,
    pub language: Option<String>,
    pub audio: Option<String>,
    pub territory_rating: Option<String>,
    pub package_type: Option<String>,
    pub standard: Option<ContentStandard>,
    pub is_3d: bool,
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// a title that is not a DCNC name keeps its text as the title and leaves the other fields unknown
pub fn name_fields_from_content_title(content_title: &str) -> KdmNameFields {
    let fields: Vec<&str> = content_title.trim().split('_').collect();
    let standard_field = fields.get(DCNC_STANDARD).copied().unwrap_or_default();
    let standard = if standard_field.starts_with(DCNC_SMPTE) {
        Some(ContentStandard::Smpte)
    } else if standard_field.starts_with(DCNC_INTEROP) {
        Some(ContentStandard::Interop)
    } else {
        None
    };
    if fields.len() < DCNC_FIELD_COUNT || standard.is_none() {
        return KdmNameFields {
            title: content_title.trim().to_string(),
            ..Default::default()
        };
    }
    let content_type_parts: Vec<&str> = fields[DCNC_CONTENT_TYPE].split(WORD_SEPARATOR).collect();
    let is_3d = content_type_parts.contains(&THREE_D_MARKER)
        || standard_field
            .split(WORD_SEPARATOR)
            .any(|part| part == THREE_D_MARKER);
    KdmNameFields {
        title: fields[DCNC_TITLE].to_string(),
        content_type: content_type_parts.first().and_then(|kind| non_empty(kind)),
        language: non_empty(fields[DCNC_LANGUAGE]),
        audio: non_empty(fields[DCNC_AUDIO]),
        territory_rating: non_empty(fields[DCNC_TERRITORY_RATING]),
        package_type: fields[DCNC_PACKAGE_TYPE]
            .split(WORD_SEPARATOR)
            .next()
            .and_then(non_empty),
        standard,
        is_3d,
    }
}

// letters canonical decomposition leaves whole
const LETTERS_WITHOUT_DECOMPOSITION: &[(char, &str)] = &[
    ('Æ', "AE"),
    ('æ', "ae"),
    ('Œ', "OE"),
    ('œ', "oe"),
    ('ß', "ss"),
    ('Ø', "O"),
    ('ø', "o"),
    ('Đ', "D"),
    ('đ', "d"),
    ('Ð', "D"),
    ('ð', "d"),
    ('Ł', "L"),
    ('ł', "l"),
    ('Þ', "TH"),
    ('þ', "th"),
    ('ı', "i"),
    ('ﬁ', "fi"),
    ('ﬂ', "fl"),
];

// decomposition splits an accented letter into its base and a combining mark, which is dropped
pub fn ascii_transliteration(value: &str) -> String {
    let decomposed = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd().normalize(value);
    let mut ascii = String::with_capacity(decomposed.len());
    for character in decomposed.chars() {
        if character.is_ascii() {
            ascii.push(character);
        } else if let Some((_, replacement)) = LETTERS_WITHOUT_DECOMPOSITION
            .iter()
            .find(|(letter, _)| *letter == character)
        {
            ascii.push_str(replacement);
        }
    }
    ascii
}

// fields hold letters, digits and hyphens only, the underscore separates fields
pub fn name_field(value: &str, maximum_length: usize) -> String {
    let mut field = String::new();
    for character in ascii_transliteration(value).chars() {
        if character.is_ascii_alphanumeric() {
            field.push(character);
        } else if (character.is_whitespace() || character == '_' || character == WORD_SEPARATOR)
            && !field.is_empty()
            && !field.ends_with(WORD_SEPARATOR)
        {
            field.push(WORD_SEPARATOR);
        }
    }
    let truncated: String = field.chars().take(maximum_length).collect();
    truncated.trim_end_matches(WORD_SEPARATOR).to_string()
}

fn title_field(fields: &KdmNameFields) -> String {
    let title = name_field(&fields.title, TITLE_LENGTH);
    if !fields.is_3d || title.contains(THREE_D_MARKER) {
        return title;
    }
    let shortened = name_field(&title, TITLE_LENGTH - THREE_D_MARKER.len());
    format!("{shortened}{THREE_D_MARKER}")
}

#[derive(Debug, Clone, Copy)]
pub struct KdmNaming<'a> {
    pub fields: &'a KdmNameFields,
    pub creation_facility: &'a str,
    pub active: NaiveDate,
    pub inactive: NaiveDate,
}

impl KdmNaming<'_> {
    fn name(&self, recipient_field: String) -> String {
        let optional = |value: &Option<String>| {
            value
                .as_deref()
                .map(|value| name_field(value, usize::MAX))
                .filter(|value| !value.is_empty())
        };
        let creation_facility =
            name_field(self.creation_facility, CREATION_FACILITY_LENGTH).to_uppercase();
        let segments = [
            Some(KDM_NAME_PREFIX.to_string()),
            Some(title_field(self.fields)),
            optional(&self.fields.content_type),
            optional(&self.fields.language),
            optional(&self.fields.audio),
            Some(recipient_field),
            Some(self.active.format(NAME_DATE_FORMAT).to_string()),
            Some(self.inactive.format(NAME_DATE_FORMAT).to_string()),
            (!creation_facility.is_empty()).then_some(creation_facility),
            optional(&self.fields.package_type),
            optional(&self.fields.territory_rating),
        ];
        segments
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(FIELD_SEPARATOR)
    }

    pub fn kdm_file_name(&self, media_block_serial: &str) -> String {
        let serial = name_field(media_block_serial, SERIAL_NUMBER_LENGTH);
        format!("{}{KDM_EXTENSION}", self.name(serial))
    }

    // the ZIP file name without its extension, which is also the email subject
    pub fn zip_name(&self, theatre_name: &str) -> String {
        self.name(name_field(theatre_name, THEATRE_NAME_LENGTH))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CinemaBundle {
    pub cinema: String,
    pub emails: Vec<String>,
    pub zip_name: String,
    pub zip_path: PathBuf,
    pub kdms: Vec<IssuedKdm>,
}

pub fn zip_files(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in files {
            zip.start_file(name, opts)
                .map_err(|e| format!("zip entry '{name}': {e}"))?;
            zip.write_all(bytes)
                .map_err(|e| format!("zip write '{name}': {e}"))?;
        }
        zip.finish().map_err(|e| format!("finish zip: {e}"))?;
    }
    Ok(buf.into_inner())
}

pub fn write_files(directory: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|e| format!("cannot create {}: {e}", directory.display()))?;
    for (name, bytes) in files {
        crate::fs::write_atomic(&directory.join(name), bytes)?;
    }
    Ok(())
}

pub fn write_zip(
    output_dir: &Path,
    zip_name: &str,
    files: &[(String, Vec<u8>)],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("cannot create {}: {e}", output_dir.display()))?;
    let path = output_dir.join(format!("{zip_name}{ZIP_EXTENSION}"));
    crate::fs::write_atomic(&path, &zip_files(files)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn a_dcnc_title_gives_every_field_and_long_names_are_cut() {
        let fields = name_fields_from_content_title(
            "TheVeryLongFeatureTitle_FTR-2-3D_F-185_EN-FR_US-13_71-HI-VI_4K_STU_20261001_FAC_SMPTE_VF",
        );
        assert_eq!(fields.standard, Some(ContentStandard::Smpte));
        assert!(fields.is_3d);
        let naming = KdmNaming {
            fields: &fields,
            creation_facility: "dist",
            active: date("2026-11-01"),
            inactive: date("2026-11-08"),
        };
        assert_eq!(
            naming.kdm_file_name("IMB-123456789012345678901234"),
            "k_TheVeryLongF3D_FTR_EN-FR_71-HI-VI_IMB-1234567890123456_20261101_20261108_DIS_VF_US-13.xml"
        );
        assert_eq!(
            naming.zip_name("The Grand Picture Palace of Dreams"),
            "k_TheVeryLongF3D_FTR_EN-FR_71-HI-VI_The-Grand-Picture-Pa_20261101_20261108_DIS_VF_US-13"
        );
    }

    #[test]
    fn a_free_text_title_keeps_only_what_is_known() {
        let fields = name_fields_from_content_title("Le Café des Étoiles: Director's Cut");
        assert_eq!(fields.standard, None);
        let naming = KdmNaming {
            fields: &fields,
            creation_facility: "XYZ",
            active: date("2026-12-24"),
            inactive: date("2027-01-02"),
        };
        assert_eq!(
            naming.kdm_file_name("1001"),
            "k_Le-Cafe-des-Et_1001_20261224_20270102_XYZ.xml"
        );
    }

    #[test]
    fn accents_and_ligatures_become_ascii_letters() {
        assert_eq!(
            ascii_transliteration("Café Ærøskøbing Œuvre Straße Łódź Ñandú"),
            "Cafe AEroskobing OEuvre Strasse Lodz Nandu"
        );
        assert_eq!(
            name_field("Kinoteatr Wrocław Città", 20),
            "Kinoteatr-Wroclaw-Ci"
        );
    }

    #[test]
    fn name_fields_hold_letters_digits_and_single_hyphens() {
        assert_eq!(name_field("  a__b  c-/-d ", usize::MAX), "a-b-c-d");
        assert_eq!(name_field("abcdef", 3), "abc");
        assert_eq!(name_field("ab cdef", 3), "ab");
    }

    #[test]
    fn the_zip_holds_every_entry_with_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            ("a.kdm.xml".to_string(), b"<kdm-a/>".to_vec()),
            ("b.kdm.xml".to_string(), b"<kdm-b/>".to_vec()),
        ];
        let path = write_zip(dir.path(), "k_Title_Rex", &files).unwrap();
        assert_eq!(path.file_name().unwrap(), "k_Title_Rex.zip");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        let mut content = String::new();
        archive
            .by_name("b.kdm.xml")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "<kdm-b/>");
    }
}
