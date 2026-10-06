// the distributor's cinemas, titles, bookings and issued KDMs in one sqlite file
use super::bundle::name_fields_from_content_title;
use super::cinema::{AuthorizedDevice, CertSource, Cinema, CinemaDb, Screen};
use super::email::{SmtpConfig, send_bundle};
use super::formulation::ContentStandard;
use super::history;
use super::issue::{
    DkdmIssue, DkdmIssueOutcome, IssuePlan, IssuedKdm, KdmSigner, ScreenTarget, issue_from_dkdm,
    plan_issue,
};
use super::window::LocalWindow;
use crate::certificate::{
    AudioForensicMarking, KdmFormulation, PictureForensicMarking, cert_info_from_pem,
    chain_from_pem, parse_kdm,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub type CinemaId = i64;
pub type ScreenId = i64;
pub type TitleId = i64;
pub type BookingId = i64;

const LOCAL_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";
const SMPTE_STANDARD: &str = "smpte";
const INTEROP_STANDARD: &str = "interop";

// index i upgrades a database at schema version i to version i + 1
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE certificates (
    id INTEGER PRIMARY KEY,
    thumbprint TEXT NOT NULL UNIQUE,
    subject TEXT NOT NULL,
    serial TEXT NOT NULL,
    not_before TEXT NOT NULL,
    not_after TEXT NOT NULL,
    chain_pem TEXT NOT NULL
);
CREATE TABLE cinemas (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    facility_id TEXT,
    time_zone TEXT,
    emails TEXT NOT NULL,
    notes TEXT NOT NULL,
    contacts TEXT NOT NULL,
    address TEXT
);
CREATE TABLE screens (
    id INTEGER PRIMARY KEY,
    cinema_id INTEGER NOT NULL REFERENCES cinemas(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    device_serial TEXT,
    recipient_certificate_id INTEGER NOT NULL REFERENCES certificates(id),
    UNIQUE (cinema_id, name)
);
CREATE TABLE authorized_devices (
    screen_id INTEGER NOT NULL REFERENCES screens(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    device_type TEXT NOT NULL,
    serial TEXT,
    certificate_id INTEGER NOT NULL REFERENCES certificates(id),
    PRIMARY KEY (screen_id, position)
);
CREATE TABLE titles (
    id INTEGER PRIMARY KEY,
    cpl_id TEXT NOT NULL UNIQUE,
    content_title TEXT NOT NULL,
    content_standard TEXT,
    dkdm_xml TEXT NOT NULL,
    dkdm_not_valid_before TEXT NOT NULL,
    dkdm_not_valid_after TEXT NOT NULL
);
CREATE TABLE bookings (
    id INTEGER PRIMARY KEY,
    title_id INTEGER NOT NULL REFERENCES titles(id),
    local_start TEXT NOT NULL,
    local_end TEXT NOT NULL,
    formulation TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE booking_screens (
    booking_id INTEGER NOT NULL REFERENCES bookings(id) ON DELETE CASCADE,
    screen_id INTEGER NOT NULL REFERENCES screens(id),
    PRIMARY KEY (booking_id, screen_id)
);
CREATE TABLE issues (
    id INTEGER PRIMARY KEY,
    issued_at TEXT NOT NULL,
    cpl_id TEXT NOT NULL,
    content_title TEXT NOT NULL,
    booking_id INTEGER REFERENCES bookings(id) ON DELETE SET NULL,
    cinema TEXT,
    screen TEXT,
    recipient_subject TEXT NOT NULL,
    recipient_serial TEXT NOT NULL,
    recipient_thumbprint TEXT,
    formulation TEXT,
    valid_from TEXT NOT NULL,
    valid_to TEXT NOT NULL,
    file_name TEXT NOT NULL
);
"#,
    r#"
CREATE TABLE deliveries (
    id INTEGER PRIMARY KEY,
    delivered_at TEXT NOT NULL,
    booking_id INTEGER REFERENCES bookings(id) ON DELETE SET NULL,
    cinema TEXT NOT NULL,
    zip_name TEXT NOT NULL,
    zip_path TEXT NOT NULL,
    recipients TEXT NOT NULL,
    result TEXT NOT NULL
);
"#,
];

#[derive(Debug, Clone, PartialEq)]
pub struct StoredCinema {
    pub id: CinemaId,
    pub cinema: Cinema,
    // in the order of cinema.screens
    pub screen_ids: Vec<ScreenId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Title {
    pub id: TitleId,
    pub cpl_id: String,
    pub content_title: String,
    pub standard: Option<ContentStandard>,
    pub dkdm_xml: String,
    pub dkdm_not_valid_before: String,
    pub dkdm_not_valid_after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Booking {
    pub id: BookingId,
    pub title_id: TitleId,
    pub screen_ids: Vec<ScreenId>,
    // in each cinema's own time zone
    pub window: LocalWindow,
    pub formulation: Option<KdmFormulation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRecord {
    pub issued_at: String,
    pub cpl_id: String,
    pub content_title: String,
    pub booking_id: Option<BookingId>,
    pub cinema: Option<String>,
    pub screen: Option<String>,
    pub recipient_subject: String,
    pub recipient_serial: String,
    pub recipient_thumbprint: Option<String>,
    pub formulation: Option<KdmFormulation>,
    pub valid_from: String,
    pub valid_to: String,
    pub file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum DeliveryResult {
    // written to the output folder and not emailed
    Written,
    Sent,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecord {
    pub delivered_at: String,
    pub booking_id: Option<BookingId>,
    pub cinema: String,
    pub zip_name: String,
    pub zip_path: String,
    pub recipients: Vec<String>,
    pub result: DeliveryResult,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub cinemas: usize,
    pub screens: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IssueSettings {
    pub signer: KdmSigner,
    pub dkdm_recipient_key: PathBuf,
    pub creation_facility: String,
    pub output_dir: PathBuf,
    pub picture_forensic_marking: PictureForensicMarking,
    pub audio_forensic_marking: AudioForensicMarking,
}

pub struct DistributionDatabase {
    connection: Connection,
}

// a booking with everything its screens need, loaded once
struct BookingParts {
    booking: Booking,
    title: Title,
    cinemas: Vec<StoredCinema>,
    // (index into cinemas, index into that cinema's screens) per booked screen
    target_indexes: Vec<(usize, usize)>,
}

impl BookingParts {
    fn targets(&self) -> Vec<ScreenTarget<'_>> {
        self.target_indexes
            .iter()
            .map(|(cinema_index, screen_index)| {
                let cinema = &self.cinemas[*cinema_index].cinema;
                ScreenTarget {
                    cinema,
                    screen: &cinema.screens[*screen_index],
                    window: self.booking.window,
                }
            })
            .collect()
    }

    fn issue<'a>(
        &'a self,
        settings: &'a IssueSettings,
        issue_date: DateTime<Utc>,
    ) -> DkdmIssue<'a> {
        DkdmIssue {
            dkdm_xml: &self.title.dkdm_xml,
            dkdm_recipient_key: &settings.dkdm_recipient_key,
            signer: &settings.signer,
            standard: self.title.standard,
            formulation: self.booking.formulation,
            picture_forensic_marking: settings.picture_forensic_marking,
            audio_forensic_marking: settings.audio_forensic_marking,
            creation_facility: &settings.creation_facility,
            issue_date,
        }
    }
}

fn database_error(error: rusqlite::Error) -> String {
    format!("KDM distribution database: {error}")
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("cannot encode for the database: {e}"))
}

fn from_json<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("the database holds unreadable JSON: {e}"))
}

fn standard_text(standard: Option<ContentStandard>) -> Option<&'static str> {
    standard.map(|standard| match standard {
        ContentStandard::Smpte => SMPTE_STANDARD,
        ContentStandard::Interop => INTEROP_STANDARD,
    })
}

fn parse_standard(text: Option<String>) -> Option<ContentStandard> {
    match text.as_deref() {
        Some(SMPTE_STANDARD) => Some(ContentStandard::Smpte),
        Some(INTEROP_STANDARD) => Some(ContentStandard::Interop),
        _ => None,
    }
}

fn parse_local_time(text: &str) -> Result<NaiveDateTime, String> {
    NaiveDateTime::parse_from_str(text, LOCAL_TIME_FORMAT)
        .map_err(|e| format!("the database holds an unreadable local time '{text}': {e}"))
}

fn parse_formulation(text: Option<String>) -> Result<Option<KdmFormulation>, String> {
    text.map(|text| text.parse()).transpose()
}

// one row per leaf thumbprint, the chain PEM updated when it is seen again
fn save_certificate(transaction: &Transaction<'_>, chain_pem: &str) -> Result<i64, String> {
    let info = cert_info_from_pem(chain_pem)?;
    chain_from_pem(chain_pem)?;
    transaction
        .execute(
            "INSERT INTO certificates (thumbprint, subject, serial, not_before, not_after, chain_pem)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (thumbprint) DO UPDATE SET chain_pem = excluded.chain_pem",
            params![
                info.thumbprint,
                info.subject_cn,
                info.serial,
                info.not_before,
                info.not_after,
                chain_pem
            ],
        )
        .map_err(database_error)?;
    transaction
        .query_row(
            "SELECT id FROM certificates WHERE thumbprint = ?1",
            params![info.thumbprint],
            |row| row.get(0),
        )
        .map_err(database_error)
}

fn certificate_pem(connection: &Connection, id: i64) -> Result<String, String> {
    connection
        .query_row(
            "SELECT chain_pem FROM certificates WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(database_error)
}

impl DistributionDatabase {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        Self::migrate(Connection::open(path).map_err(database_error)?)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        Self::migrate(Connection::open_in_memory().map_err(database_error)?)
    }

    fn migrate(mut connection: Connection) -> Result<Self, String> {
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);",
            )
            .map_err(database_error)?;
        let transaction = connection.transaction().map_err(database_error)?;
        let version: usize = transaction
            .query_row("SELECT version FROM schema_version", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(database_error)?
            .map_or(0, |version| version as usize);
        if version > MIGRATIONS.len() {
            return Err(format!(
                "the KDM distribution database is at schema version {version}, newer than this \
                 build knows ({})",
                MIGRATIONS.len()
            ));
        }
        for migration in &MIGRATIONS[version..] {
            transaction
                .execute_batch(migration)
                .map_err(database_error)?;
        }
        transaction
            .execute("DELETE FROM schema_version", [])
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![MIGRATIONS.len() as i64],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(Self { connection })
    }

    pub fn schema_version(&self) -> Result<i64, String> {
        self.connection
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .map_err(database_error)
    }

    // matched by name, and screens by name within it, so bookings keep their screens
    pub fn save_cinema(&mut self, cinema: &Cinema) -> Result<CinemaId, String> {
        let transaction = self.connection.transaction().map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO cinemas (name, facility_id, time_zone, emails, notes, contacts, address)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (name) DO UPDATE SET facility_id = excluded.facility_id,
                     time_zone = excluded.time_zone, emails = excluded.emails,
                     notes = excluded.notes, contacts = excluded.contacts,
                     address = excluded.address",
                params![
                    cinema.name,
                    cinema.facility_id,
                    cinema.time_zone,
                    to_json(&cinema.emails)?,
                    cinema.notes,
                    to_json(&cinema.contacts)?,
                    cinema.address.as_ref().map(to_json).transpose()?,
                ],
            )
            .map_err(database_error)?;
        let cinema_id: CinemaId = transaction
            .query_row(
                "SELECT id FROM cinemas WHERE name = ?1",
                params![cinema.name],
                |row| row.get(0),
            )
            .map_err(database_error)?;

        let mut kept = Vec::new();
        for screen in &cinema.screens {
            let recipient = save_certificate(&transaction, &screen.cert.pem()?)?;
            transaction
                .execute(
                    "INSERT INTO screens (cinema_id, name, device_serial, recipient_certificate_id)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (cinema_id, name) DO UPDATE SET
                         device_serial = excluded.device_serial,
                         recipient_certificate_id = excluded.recipient_certificate_id",
                    params![cinema_id, screen.name, screen.device_serial, recipient],
                )
                .map_err(database_error)?;
            let screen_id: ScreenId = transaction
                .query_row(
                    "SELECT id FROM screens WHERE cinema_id = ?1 AND name = ?2",
                    params![cinema_id, screen.name],
                    |row| row.get(0),
                )
                .map_err(database_error)?;
            transaction
                .execute(
                    "DELETE FROM authorized_devices WHERE screen_id = ?1",
                    params![screen_id],
                )
                .map_err(database_error)?;
            for (position, device) in screen.authorized_devices.iter().enumerate() {
                let certificate = save_certificate(&transaction, &device.certificate)?;
                transaction
                    .execute(
                        "INSERT INTO authorized_devices
                             (screen_id, position, device_type, serial, certificate_id)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            screen_id,
                            position as i64,
                            device.device_type,
                            device.serial,
                            certificate
                        ],
                    )
                    .map_err(database_error)?;
            }
            kept.push(screen_id);
        }
        let existing: Vec<ScreenId> = {
            let mut statement = transaction
                .prepare("SELECT id FROM screens WHERE cinema_id = ?1")
                .map_err(database_error)?;
            statement
                .query_map(params![cinema_id], |row| row.get(0))
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        for screen_id in existing.into_iter().filter(|id| !kept.contains(id)) {
            transaction
                .execute("DELETE FROM screens WHERE id = ?1", params![screen_id])
                .map_err(|e| {
                    format!(
                        "a screen gone from cinema '{}' is still booked: {e}",
                        cinema.name
                    )
                })?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(cinema_id)
    }

    pub fn cinema(&self, id: CinemaId) -> Result<StoredCinema, String> {
        let row = self
            .connection
            .query_row(
                "SELECT name, facility_id, time_zone, emails, notes, contacts, address
                 FROM cinemas WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .map_err(database_error)?;
        let (name, facility_id, time_zone, emails, notes, contacts, address) = row;
        let mut cinema = Cinema {
            name,
            emails: from_json(&emails)?,
            notes,
            screens: Vec::new(),
            facility_id,
            time_zone,
            contacts: from_json(&contacts)?,
            address: address.as_deref().map(from_json).transpose()?,
        };
        let screen_rows: Vec<(ScreenId, String, Option<String>, i64)> = {
            let mut statement = self
                .connection
                .prepare(
                    "SELECT id, name, device_serial, recipient_certificate_id
                     FROM screens WHERE cinema_id = ?1 ORDER BY id",
                )
                .map_err(database_error)?;
            statement
                .query_map(params![id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        let mut screen_ids = Vec::new();
        for (screen_id, name, device_serial, recipient) in screen_rows {
            let mut screen = Screen::new(
                &name,
                CertSource::Inline(certificate_pem(&self.connection, recipient)?),
            )?;
            screen.device_serial = device_serial;
            screen.authorized_devices = self.authorized_devices(screen_id)?;
            cinema.screens.push(screen);
            screen_ids.push(screen_id);
        }
        Ok(StoredCinema {
            id,
            cinema,
            screen_ids,
        })
    }

    fn authorized_devices(&self, screen_id: ScreenId) -> Result<Vec<AuthorizedDevice>, String> {
        let rows: Vec<(String, Option<String>, i64)> = {
            let mut statement = self
                .connection
                .prepare(
                    "SELECT device_type, serial, certificate_id FROM authorized_devices
                     WHERE screen_id = ?1 ORDER BY position",
                )
                .map_err(database_error)?;
            statement
                .query_map(params![screen_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        rows.into_iter()
            .map(|(device_type, serial, certificate)| {
                Ok(AuthorizedDevice {
                    device_type,
                    serial,
                    certificate: certificate_pem(&self.connection, certificate)?,
                })
            })
            .collect()
    }

    pub fn cinemas(&self) -> Result<Vec<StoredCinema>, String> {
        let ids: Vec<CinemaId> = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM cinemas ORDER BY name")
                .map_err(database_error)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        ids.into_iter().map(|id| self.cinema(id)).collect()
    }

    pub fn remove_cinema(&mut self, id: CinemaId) -> Result<(), String> {
        let removed = self
            .connection
            .execute("DELETE FROM cinemas WHERE id = ?1", params![id])
            .map_err(|e| format!("cinema {id} cannot be removed while booked: {e}"))?;
        if removed == 0 {
            return Err(format!("cinema {id} not found"));
        }
        Ok(())
    }

    fn screen_cinema(&self, screen_id: ScreenId) -> Result<CinemaId, String> {
        self.connection
            .query_row(
                "SELECT cinema_id FROM screens WHERE id = ?1",
                params![screen_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?
            .ok_or_else(|| format!("screen {screen_id} not found"))
    }

    // the title, CPL and DKDM window come from the DKDM itself
    pub fn add_title_from_dkdm(&mut self, dkdm_xml: &str) -> Result<TitleId, String> {
        let metadata = parse_kdm(dkdm_xml)?;
        let standard = name_fields_from_content_title(&metadata.content_title).standard;
        self.connection
            .execute(
                "INSERT INTO titles (cpl_id, content_title, content_standard, dkdm_xml,
                     dkdm_not_valid_before, dkdm_not_valid_after)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (cpl_id) DO UPDATE SET content_title = excluded.content_title,
                     dkdm_xml = excluded.dkdm_xml,
                     dkdm_not_valid_before = excluded.dkdm_not_valid_before,
                     dkdm_not_valid_after = excluded.dkdm_not_valid_after",
                params![
                    metadata.cpl_id.to_string(),
                    metadata.content_title,
                    standard_text(standard),
                    dkdm_xml,
                    metadata.not_valid_before,
                    metadata.not_valid_after
                ],
            )
            .map_err(database_error)?;
        self.connection
            .query_row(
                "SELECT id FROM titles WHERE cpl_id = ?1",
                params![metadata.cpl_id.to_string()],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    pub fn set_title_standard(
        &mut self,
        id: TitleId,
        standard: Option<ContentStandard>,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE titles SET content_standard = ?1 WHERE id = ?2",
                params![standard_text(standard), id],
            )
            .map_err(database_error)?;
        Ok(())
    }

    pub fn title(&self, id: TitleId) -> Result<Title, String> {
        self.connection
            .query_row(
                "SELECT cpl_id, content_title, content_standard, dkdm_xml,
                     dkdm_not_valid_before, dkdm_not_valid_after
                 FROM titles WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Title {
                        id,
                        cpl_id: row.get(0)?,
                        content_title: row.get(1)?,
                        standard: parse_standard(row.get(2)?),
                        dkdm_xml: row.get(3)?,
                        dkdm_not_valid_before: row.get(4)?,
                        dkdm_not_valid_after: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(database_error)?
            .ok_or_else(|| format!("title {id} not found"))
    }

    pub fn titles(&self) -> Result<Vec<Title>, String> {
        let ids: Vec<TitleId> = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM titles ORDER BY content_title")
                .map_err(database_error)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        ids.into_iter().map(|id| self.title(id)).collect()
    }

    pub fn add_booking(
        &mut self,
        title_id: TitleId,
        screen_ids: &[ScreenId],
        window: LocalWindow,
        formulation: Option<KdmFormulation>,
        created_at: DateTime<Utc>,
    ) -> Result<BookingId, String> {
        if screen_ids.is_empty() {
            return Err("a booking needs at least one screen".to_string());
        }
        if window.end <= window.start {
            return Err(format!(
                "the booking ends at {} before it starts at {}",
                window.end, window.start
            ));
        }
        self.title(title_id)?;
        for screen_id in screen_ids {
            self.screen_cinema(*screen_id)?;
        }
        let transaction = self.connection.transaction().map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO bookings (title_id, local_start, local_end, formulation, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    title_id,
                    window.start.format(LOCAL_TIME_FORMAT).to_string(),
                    window.end.format(LOCAL_TIME_FORMAT).to_string(),
                    formulation.map(KdmFormulation::as_str),
                    created_at.to_rfc3339()
                ],
            )
            .map_err(database_error)?;
        let booking_id = transaction.last_insert_rowid();
        for screen_id in screen_ids {
            transaction
                .execute(
                    "INSERT INTO booking_screens (booking_id, screen_id) VALUES (?1, ?2)",
                    params![booking_id, screen_id],
                )
                .map_err(database_error)?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(booking_id)
    }

    pub fn booking(&self, id: BookingId) -> Result<Booking, String> {
        let (title_id, start, end, formulation): (TitleId, String, String, Option<String>) = self
            .connection
            .query_row(
                "SELECT title_id, local_start, local_end, formulation FROM bookings WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(database_error)?
            .ok_or_else(|| format!("booking {id} not found"))?;
        let screen_ids: Vec<ScreenId> = {
            let mut statement = self
                .connection
                .prepare(
                    "SELECT screen_id FROM booking_screens WHERE booking_id = ?1 ORDER BY screen_id",
                )
                .map_err(database_error)?;
            statement
                .query_map(params![id], |row| row.get(0))
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        Ok(Booking {
            id,
            title_id,
            screen_ids,
            window: LocalWindow {
                start: parse_local_time(&start)?,
                end: parse_local_time(&end)?,
            },
            formulation: parse_formulation(formulation)?,
        })
    }

    pub fn bookings(&self) -> Result<Vec<Booking>, String> {
        let ids: Vec<BookingId> = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM bookings ORDER BY local_start, id")
                .map_err(database_error)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(database_error)?
                .collect::<Result<_, _>>()
                .map_err(database_error)?
        };
        ids.into_iter().map(|id| self.booking(id)).collect()
    }

    pub fn record_issue(&mut self, record: &IssueRecord) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO issues (issued_at, cpl_id, content_title, booking_id, cinema, screen,
                     recipient_subject, recipient_serial, recipient_thumbprint, formulation,
                     valid_from, valid_to, file_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    record.issued_at,
                    record.cpl_id,
                    record.content_title,
                    record.booking_id,
                    record.cinema,
                    record.screen,
                    record.recipient_subject,
                    record.recipient_serial,
                    record.recipient_thumbprint,
                    record.formulation.map(KdmFormulation::as_str),
                    record.valid_from,
                    record.valid_to,
                    record.file_name
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    pub fn issues(&self) -> Result<Vec<IssueRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT issued_at, cpl_id, content_title, booking_id, cinema, screen,
                     recipient_subject, recipient_serial, recipient_thumbprint, formulation,
                     valid_from, valid_to, file_name
                 FROM issues ORDER BY id",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    IssueRecord {
                        issued_at: row.get(0)?,
                        cpl_id: row.get(1)?,
                        content_title: row.get(2)?,
                        booking_id: row.get(3)?,
                        cinema: row.get(4)?,
                        screen: row.get(5)?,
                        recipient_subject: row.get(6)?,
                        recipient_serial: row.get(7)?,
                        recipient_thumbprint: row.get(8)?,
                        formulation: None,
                        valid_from: row.get(10)?,
                        valid_to: row.get(11)?,
                        file_name: row.get(12)?,
                    },
                    row.get::<_, Option<String>>(9)?,
                ))
            })
            .map_err(database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        rows.into_iter()
            .map(|(mut record, formulation)| {
                record.formulation = parse_formulation(formulation)?;
                Ok(record)
            })
            .collect()
    }

    pub fn record_delivery(&mut self, record: &DeliveryRecord) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO deliveries (delivered_at, booking_id, cinema, zip_name, zip_path,
                     recipients, result)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    record.delivered_at,
                    record.booking_id,
                    record.cinema,
                    record.zip_name,
                    record.zip_path,
                    to_json(&record.recipients)?,
                    to_json(&record.result)?
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    pub fn deliveries(&self) -> Result<Vec<DeliveryRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT delivered_at, booking_id, cinema, zip_name, zip_path, recipients, result
                 FROM deliveries ORDER BY id",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    DeliveryRecord {
                        delivered_at: row.get(0)?,
                        booking_id: row.get(1)?,
                        cinema: row.get(2)?,
                        zip_name: row.get(3)?,
                        zip_path: row.get(4)?,
                        recipients: Vec::new(),
                        result: DeliveryResult::Written,
                    },
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        rows.into_iter()
            .map(|(mut record, recipients, result)| {
                record.recipients = from_json(&recipients)?;
                record.result = from_json(&result)?;
                Ok(record)
            })
            .collect()
    }

    // one email per cinema ZIP when smtp is given, each attempt recorded with its result
    pub fn deliver_bundles(
        &mut self,
        outcome: &DkdmIssueOutcome,
        booking_id: Option<BookingId>,
        smtp: Option<&SmtpConfig>,
        delivered_at: DateTime<Utc>,
    ) -> Result<Vec<DeliveryRecord>, String> {
        let mut records = Vec::new();
        for bundle in &outcome.bundles {
            let result = match smtp {
                None => DeliveryResult::Written,
                Some(config) => {
                    match send_bundle(config, bundle, &outcome.content_title, &bundle.emails) {
                        Ok(()) => DeliveryResult::Sent,
                        Err(e) => DeliveryResult::Failed(e),
                    }
                }
            };
            let record = DeliveryRecord {
                delivered_at: delivered_at.to_rfc3339(),
                booking_id,
                cinema: bundle.cinema.clone(),
                zip_name: bundle.zip_name.clone(),
                zip_path: bundle.zip_path.display().to_string(),
                recipients: if smtp.is_some() {
                    bundle.emails.clone()
                } else {
                    Vec::new()
                },
                result,
            };
            self.record_delivery(&record)?;
            records.push(record);
        }
        Ok(records)
    }

    // a screen whose certificate cannot be read is skipped and named in the report
    pub fn import_cinema_database(&mut self, database: &CinemaDb) -> Result<ImportReport, String> {
        let mut report = ImportReport::default();
        for cinema in &database.cinemas {
            let mut importable = cinema.clone();
            importable.screens.clear();
            for screen in &cinema.screens {
                match screen.cert.pem().and_then(|pem| {
                    cert_info_from_pem(&pem)?;
                    Ok(pem)
                }) {
                    Ok(pem) => {
                        let mut stored = screen.clone();
                        stored.cert = CertSource::Inline(pem);
                        importable.screens.push(stored);
                    }
                    Err(e) => report
                        .skipped
                        .push(format!("{} / {}: {e}", cinema.name, screen.name)),
                }
            }
            report.screens += importable.screens.len();
            self.save_cinema(&importable)?;
            report.cinemas += 1;
        }
        Ok(report)
    }

    pub fn import_history(&mut self, records: &[history::Record]) -> Result<usize, String> {
        for record in records {
            self.record_issue(&IssueRecord {
                issued_at: record.timestamp.clone(),
                cpl_id: record.cpl_id.clone(),
                content_title: record.content_title.clone(),
                booking_id: None,
                cinema: None,
                screen: None,
                recipient_subject: record.recipient_subject.clone(),
                recipient_serial: record.recipient_serial.clone(),
                recipient_thumbprint: None,
                formulation: None,
                valid_from: record.valid_from.clone(),
                valid_to: record.valid_to.clone(),
                file_name: record.output_path.clone(),
            })?;
        }
        Ok(records.len())
    }

    fn booking_parts(&self, booking_id: BookingId) -> Result<BookingParts, String> {
        let booking = self.booking(booking_id)?;
        let title = self.title(booking.title_id)?;
        let mut cinemas: Vec<StoredCinema> = Vec::new();
        let mut target_indexes = Vec::new();
        for screen_id in &booking.screen_ids {
            let cinema_id = self.screen_cinema(*screen_id)?;
            let cinema_index = match cinemas.iter().position(|stored| stored.id == cinema_id) {
                Some(index) => index,
                None => {
                    cinemas.push(self.cinema(cinema_id)?);
                    cinemas.len() - 1
                }
            };
            let screen_index = cinemas[cinema_index]
                .screen_ids
                .iter()
                .position(|id| id == screen_id)
                .ok_or_else(|| format!("screen {screen_id} not found"))?;
            target_indexes.push((cinema_index, screen_index));
        }
        Ok(BookingParts {
            booking,
            title,
            cinemas,
            target_indexes,
        })
    }

    // the formulation, window and check report each booked screen would get, nothing written
    pub fn plan_booking(
        &self,
        booking_id: BookingId,
        settings: &IssueSettings,
        issue_date: DateTime<Utc>,
    ) -> Result<IssuePlan, String> {
        let parts = self.booking_parts(booking_id)?;
        plan_issue(&parts.issue(settings, issue_date), &parts.targets())
    }

    // every screen in the booking from the title's DKDM, each KDM recorded in the history
    pub fn issue_booking(
        &mut self,
        booking_id: BookingId,
        settings: &IssueSettings,
        issue_date: DateTime<Utc>,
    ) -> Result<DkdmIssueOutcome, String> {
        let parts = self.booking_parts(booking_id)?;
        let title = parts.title.clone();
        let outcome = issue_from_dkdm(
            &parts.issue(settings, issue_date),
            &parts.targets(),
            &settings.output_dir,
        )?;
        let issued: Vec<&IssuedKdm> = outcome
            .bundles
            .iter()
            .flat_map(|bundle| &bundle.kdms)
            .collect();
        for kdm in issued {
            self.record_issue(&IssueRecord {
                issued_at: kdm.issue_date.to_rfc3339(),
                cpl_id: title.cpl_id.clone(),
                content_title: title.content_title.clone(),
                booking_id: Some(booking_id),
                cinema: Some(kdm.cinema.clone()),
                screen: Some(kdm.screen.clone()),
                recipient_subject: kdm.recipient_subject.clone(),
                recipient_serial: kdm.recipient_serial.clone(),
                recipient_thumbprint: Some(kdm.recipient_thumbprint.clone()),
                formulation: Some(kdm.formulation),
                valid_from: kdm.not_valid_before.clone(),
                valid_to: kdm.not_valid_after.clone(),
                file_name: kdm.file_name.clone(),
            })?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::flm::Contact;
    use crate::kdm_distribution::test_support::{
        DCNC_TITLE, cinemas, dkdm, fixtures, local_window, read,
    };
    use std::io::Read;

    fn settings(output_dir: &Path) -> IssueSettings {
        let f = fixtures();
        IssueSettings {
            signer: f.signer(),
            dkdm_recipient_key: f.distributor_signer_key.clone(),
            creation_facility: "DIS".to_string(),
            output_dir: output_dir.to_path_buf(),
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        }
    }

    fn table_names(database: &DistributionDatabase) -> Vec<String> {
        let mut statement = database
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn an_empty_file_is_migrated_to_the_current_schema_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("kdm.sqlite");
        let database = DistributionDatabase::open(&path).unwrap();
        assert_eq!(database.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(
            table_names(&database),
            vec![
                "authorized_devices",
                "booking_screens",
                "bookings",
                "certificates",
                "cinemas",
                "deliveries",
                "issues",
                "schema_version",
                "screens",
                "titles"
            ]
        );
        drop(database);
        let mut reopened = DistributionDatabase::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), MIGRATIONS.len() as i64);
        reopened.save_cinema(&cinemas().1).unwrap();

        let newer = rusqlite::Connection::open(&path).unwrap();
        newer
            .execute("UPDATE schema_version SET version = 99", [])
            .unwrap();
        drop(newer);
        let error = DistributionDatabase::open(&path).err().unwrap();
        assert!(error.contains("schema version 99"), "{error}");
    }

    #[test]
    fn a_first_version_database_is_upgraded_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kdm.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version (version) VALUES (1);
                 {}
                 INSERT INTO cinemas (name, emails, notes, contacts)
                 VALUES ('Rex', '[]', '', '[]');",
                MIGRATIONS[0]
            ))
            .unwrap();
        drop(connection);
        let database = DistributionDatabase::open(&path).unwrap();
        assert_eq!(database.schema_version().unwrap(), 2);
        assert!(table_names(&database).contains(&"deliveries".to_string()));
        assert_eq!(database.cinemas().unwrap()[0].cinema.name, "Rex");
        assert!(database.deliveries().unwrap().is_empty());
    }

    #[test]
    fn a_cinema_reads_back_as_it_was_saved_and_a_resave_keeps_its_screen_ids() {
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let (mut rex, _) = cinemas();
        rex.facility_id = Some("urn:x-facilityID:example.com:Rex".into());
        rex.notes = "Booth on the left".into();
        rex.contacts = vec![Contact {
            name: "Projection Booth".into(),
            email: Some("booth@rex.test".into()),
            ..Default::default()
        }];
        let id = database.save_cinema(&rex).unwrap();
        let stored = database.cinema(id).unwrap();
        assert_eq!(stored.cinema, rex);
        assert_eq!(stored.screen_ids.len(), 2);

        rex.screens.remove(1);
        rex.time_zone = Some("Europe/Dublin".into());
        assert_eq!(database.save_cinema(&rex).unwrap(), id);
        let resaved = database.cinema(id).unwrap();
        assert_eq!(resaved.cinema, rex);
        assert_eq!(resaved.screen_ids, vec![stored.screen_ids[0]]);
        assert_eq!(database.cinemas().unwrap().len(), 1);
    }

    #[test]
    fn a_booked_screen_cannot_disappear() {
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let (mut rex, _) = cinemas();
        let cinema_id = database.save_cinema(&rex).unwrap();
        let screen_ids = database.cinema(cinema_id).unwrap().screen_ids;
        let title = database.add_title_from_dkdm(&dkdm(DCNC_TITLE, 30)).unwrap();
        database
            .add_booking(title, &screen_ids, local_window(), None, Utc::now())
            .unwrap();
        rex.screens.remove(1);
        let error = database.save_cinema(&rex).unwrap_err();
        assert!(error.contains("still booked"), "{error}");
        assert_eq!(database.cinema(cinema_id).unwrap().screen_ids, screen_ids);
    }

    #[test]
    fn a_dkdm_becomes_a_title_with_its_cpl_window_and_standard() {
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let xml = dkdm(DCNC_TITLE, 30);
        let id = database.add_title_from_dkdm(&xml).unwrap();
        let title = database.title(id).unwrap();
        let metadata = parse_kdm(&xml).unwrap();
        assert_eq!(title.cpl_id, metadata.cpl_id.to_string());
        assert_eq!(title.content_title, DCNC_TITLE);
        assert_eq!(title.standard, Some(ContentStandard::Smpte));
        assert_eq!(title.dkdm_not_valid_after, metadata.not_valid_after);
        assert_eq!(database.add_title_from_dkdm(&xml).unwrap(), id);
        assert_eq!(database.titles().unwrap().len(), 1);
    }

    #[test]
    fn the_json_cinema_database_imports_and_names_what_it_skipped() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("screen.pem");
        std::fs::copy(&f.security_managers[0].certificate, &on_disk).unwrap();
        let mut json = CinemaDb::default();
        json.add_cinema("Odeon", vec!["ops@odeon.test".into()], "notes".into())
            .unwrap();
        json.add_screen("Odeon", "1", CertSource::Path(on_disk.clone()))
            .unwrap();
        json.add_screen(
            "Odeon",
            "2",
            CertSource::Inline(read(&f.security_managers[1].certificate)),
        )
        .unwrap();
        let gone = dir.path().join("gone.pem");
        std::fs::copy(&f.security_managers[2].certificate, &gone).unwrap();
        json.add_screen("Odeon", "3", CertSource::Path(gone.clone()))
            .unwrap();
        std::fs::remove_file(&gone).unwrap();
        let json_path = dir.path().join("cinemas.json");
        json.save(&json_path).unwrap();

        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let report = database
            .import_cinema_database(&CinemaDb::load(&json_path).unwrap())
            .unwrap();
        assert_eq!((report.cinemas, report.screens), (1, 2));
        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].starts_with("Odeon / 3: "),
            "{:?}",
            report.skipped
        );

        let stored = &database.cinemas().unwrap()[0].cinema;
        assert_eq!(stored.emails, vec!["ops@odeon.test"]);
        assert_eq!(stored.notes, "notes");
        assert_eq!(stored.screens[0].cert, CertSource::Inline(read(&on_disk)));
        assert_eq!(
            stored.screens[1].cert_serial,
            json.cinemas[0].screens[1].cert_serial
        );
    }

    #[test]
    fn the_jsonl_history_imports_as_issues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kdm-history.jsonl");
        let record = history::Record::now(
            "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9",
            "Feature",
            "SM.Vendor.IMB.1001",
            "4ca4",
            "2026-11-01T18:00:00+00:00",
            "2026-11-08T23:00:00+00:00",
            "/out/001_screen.kdm.xml",
        );
        history::append(&path, &record).unwrap();
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        assert_eq!(
            database
                .import_history(&history::read_all(&path).unwrap())
                .unwrap(),
            1
        );
        let issues = database.issues().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issued_at, record.timestamp);
        assert_eq!(issues[0].recipient_serial, "4ca4");
        assert_eq!(issues[0].file_name, "/out/001_screen.kdm.xml");
        assert_eq!(issues[0].booking_id, None);
    }

    #[test]
    fn issuing_a_booking_writes_a_zip_per_cinema_and_a_history_row_per_kdm() {
        let dir = tempfile::tempdir().unwrap();
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let (rex, odeon) = cinemas();
        let rex_id = database.save_cinema(&rex).unwrap();
        let odeon_id = database.save_cinema(&odeon).unwrap();
        let mut screens = database.cinema(rex_id).unwrap().screen_ids;
        screens.extend(database.cinema(odeon_id).unwrap().screen_ids);
        let title = database.add_title_from_dkdm(&dkdm(DCNC_TITLE, 30)).unwrap();
        let window = local_window();
        let booking = database
            .add_booking(title, &screens, window, None, Utc::now())
            .unwrap();
        assert_eq!(database.booking(booking).unwrap().window, window);

        let issue_date = Utc::now() + chrono::Duration::minutes(2);
        let outcome = database
            .issue_booking(booking, &settings(dir.path()), issue_date)
            .unwrap();
        assert!(outcome.refused.is_empty(), "{:#?}", outcome.refused);
        assert_eq!(outcome.bundles.len(), 2);

        let issues = database.issues().unwrap();
        assert_eq!(issues.len(), 3);
        let mut zipped = Vec::new();
        for bundle in &outcome.bundles {
            let mut archive =
                zip::ZipArchive::new(std::fs::File::open(&bundle.zip_path).unwrap()).unwrap();
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).unwrap();
                let mut xml = String::new();
                entry.read_to_string(&mut xml).unwrap();
                assert!(parse_kdm(&xml).is_ok());
                zipped.push(entry.name().to_string());
            }
        }
        let recorded: Vec<String> = issues.iter().map(|issue| issue.file_name.clone()).collect();
        assert_eq!(recorded, zipped);
        for issue in &issues {
            assert_eq!(issue.booking_id, Some(booking));
            assert_eq!(issue.issued_at, issue_date.to_rfc3339());
            assert_eq!(issue.content_title, DCNC_TITLE);
            assert!(issue.recipient_thumbprint.is_some());
        }
        assert_eq!(issues[0].cinema.as_deref(), Some("Rex"));
        assert_eq!(issues[0].screen.as_deref(), Some("1"));
        assert_eq!(
            issues[0].formulation,
            Some(KdmFormulation::MultipleModifiedTransitional1)
        );
        assert_eq!(issues[2].cinema.as_deref(), Some("Odeon"));
    }

    fn booked(database: &mut DistributionDatabase) -> BookingId {
        let (rex, odeon) = cinemas();
        let mut screens = Vec::new();
        for cinema in [&rex, &odeon] {
            let id = database.save_cinema(cinema).unwrap();
            screens.extend(database.cinema(id).unwrap().screen_ids);
        }
        let title = database.add_title_from_dkdm(&dkdm(DCNC_TITLE, 30)).unwrap();
        database
            .add_booking(title, &screens, local_window(), None, Utc::now())
            .unwrap()
    }

    #[test]
    fn a_plan_names_each_screen_formulation_and_refusal_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let booking = booked(&mut database);
        let mut odeon = database.cinemas().unwrap()[0].cinema.clone();
        assert_eq!(odeon.name, "Odeon");
        odeon.time_zone = None;
        database.save_cinema(&odeon).unwrap();

        let plan = database
            .plan_booking(booking, &settings(dir.path()), Utc::now())
            .unwrap();
        let formulations: Vec<Option<KdmFormulation>> = plan
            .screens
            .iter()
            .map(|screen| screen.formulation)
            .collect();
        assert_eq!(
            formulations,
            vec![
                Some(KdmFormulation::MultipleModifiedTransitional1),
                Some(KdmFormulation::ModifiedTransitional1),
                None
            ]
        );
        assert!(plan.screens[0].refusals.is_empty());
        assert!(plan.screens[0].not_valid_before.is_some());
        assert!(plan.screens[2].refusals[0].contains("has no time zone"));
        assert!(plan.not_checked.iter().any(|note| note.contains("rule 12")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(database.issues().unwrap().is_empty());
    }

    #[test]
    fn delivering_records_a_written_zip_and_the_smtp_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let booking = booked(&mut database);
        let outcome = database
            .issue_booking(booking, &settings(dir.path()), Utc::now())
            .unwrap();
        let written = database
            .deliver_bundles(&outcome, Some(booking), None, Utc::now())
            .unwrap();
        assert!(
            written
                .iter()
                .all(|record| record.result == DeliveryResult::Written)
        );

        let single = DkdmIssueOutcome {
            bundles: vec![outcome.bundles[0].clone()],
            ..outcome.clone()
        };
        let (smtp, transcript) = crate::kdm_distribution::test_support::fake_server();
        let sent = database
            .deliver_bundles(&single, Some(booking), Some(&smtp), Utc::now())
            .unwrap();
        assert_eq!(sent[0].result, DeliveryResult::Sent);
        assert_eq!(sent[0].recipients, vec!["kdm@rex.test"]);
        assert!(
            transcript
                .lock()
                .unwrap()
                .body
                .contains(&format!("Subject: {}", outcome.bundles[0].zip_name))
        );

        let mut refused = smtp.clone();
        refused.port = 1;
        let failed = database
            .deliver_bundles(&single, Some(booking), Some(&refused), Utc::now())
            .unwrap();
        assert!(
            matches!(&failed[0].result, DeliveryResult::Failed(reason) if reason.contains("smtp send"))
        );

        let recorded = database.deliveries().unwrap();
        assert_eq!(recorded.len(), outcome.bundles.len() + 2);
        assert_eq!(recorded.last().unwrap(), &failed[0]);
    }
}
