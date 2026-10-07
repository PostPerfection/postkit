use super::cinema::Cinema;
use super::database::{Booking, BookingId, CinemaId, DistributionDatabase};
use super::window::kdm_window_in_time_zone;
use crate::certificate::{ChainCertificate, certificate_label, chain_from_files, chain_from_pem};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use x509_parser::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExpiringItem {
    Recipient,
    AuthorizedDevice,
    Dkdm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookingExpiry {
    pub booking_id: BookingId,
    pub content_title: String,
    pub cinema: String,
    // None for the DKDM, which every booked screen at the cinema needs
    pub screen: Option<String>,
    pub item: ExpiringItem,
    // the certificate's distinguished name, or the CPL id for a DKDM
    pub subject: String,
    pub expires_at: DateTime<Utc>,
    pub booking_ends_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerCertificateExpiry {
    pub subject: String,
    pub expires_at: DateTime<Utc>,
    // open bookings whose window ends after this certificate does
    pub bookings_ending_after: Vec<BookingId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpiryReport {
    pub bookings: Vec<BookingExpiry>,
    pub signer_chain: Vec<SignerCertificateExpiry>,
    // a booking at a cinema whose window end cannot be placed in UTC, with the reason
    pub not_checked: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookingEnding {
    pub booking_id: BookingId,
    pub cinema_id: CinemaId,
    pub cinema: String,
    pub ends_at: DateTime<Utc>,
}

fn certificate_expiry(certificate: &ChainCertificate) -> Result<(String, DateTime<Utc>), String> {
    let (_, parsed) = X509Certificate::from_der(&certificate.der)
        .map_err(|e| format!("{}: cannot be read: {e}", certificate.label))?;
    let not_after = parsed.validity().not_after.timestamp();
    let expires_at = DateTime::from_timestamp(not_after, 0)
        .ok_or_else(|| format!("{}: not after is out of range", certificate.label))?;
    Ok((certificate_label(&certificate.der, 0), expires_at))
}

fn leaf_expiry(chain_pem: &str) -> Result<(String, DateTime<Utc>), String> {
    let chain = chain_from_pem(chain_pem)?;
    let leaf = chain
        .first()
        .ok_or_else(|| "the certificate PEM holds no certificate".to_string())?;
    certificate_expiry(leaf)
}

fn booking_end(booking: &Booking, cinema: &Cinema) -> Result<DateTime<Utc>, String> {
    let zone = cinema
        .time_zone
        .as_deref()
        .ok_or_else(|| format!("cinema '{}' has no time zone", cinema.name))?;
    Ok(kdm_window_in_time_zone(&booking.window, zone)?.end)
}

pub fn bookings_ending_within(
    database: &DistributionDatabase,
    now: DateTime<Utc>,
    within: chrono::Duration,
) -> Result<Vec<BookingEnding>, String> {
    let cinemas = database.cinemas()?;
    let mut endings = Vec::new();
    for booking in database.bookings()? {
        for stored in &cinemas {
            let booked = stored
                .screen_ids
                .iter()
                .any(|id| booking.screen_ids.contains(id));
            if !booked {
                continue;
            }
            // expiry_report lists a cinema with no usable time zone as not checked
            let Ok(ends_at) = booking_end(&booking, &stored.cinema) else {
                continue;
            };
            if now < ends_at && ends_at <= now + within {
                endings.push(BookingEnding {
                    booking_id: booking.id,
                    cinema_id: stored.id,
                    cinema: stored.cinema.name.clone(),
                    ends_at,
                });
            }
        }
    }
    Ok(endings)
}

// signer_chain is the signer certificate followed by the CA certificates above it
pub fn expiry_report(
    database: &DistributionDatabase,
    signer_chain: &[PathBuf],
    now: DateTime<Utc>,
) -> Result<ExpiryReport, String> {
    let cinemas = database.cinemas()?;
    let mut report = ExpiryReport::default();
    let mut open_booking_ends: Vec<(BookingId, DateTime<Utc>)> = Vec::new();
    for booking in database.bookings()? {
        let title = database.title(booking.title_id)?;
        let dkdm_expires_at = DateTime::parse_from_rfc3339(&title.dkdm_not_valid_after)
            .map_err(|e| {
                format!(
                    "the DKDM of '{}' ends at an unreadable time '{}': {e}",
                    title.content_title, title.dkdm_not_valid_after
                )
            })?
            .with_timezone(&Utc);
        for stored in &cinemas {
            let cinema = &stored.cinema;
            let booked: Vec<_> = stored
                .screen_ids
                .iter()
                .zip(&cinema.screens)
                .filter(|(id, _)| booking.screen_ids.contains(id))
                .map(|(_, screen)| screen)
                .collect();
            if booked.is_empty() {
                continue;
            }
            let booking_ends_at = match booking_end(&booking, cinema) {
                Ok(ends) => ends,
                Err(reason) => {
                    report.not_checked.push(format!(
                        "{} at {}: {reason}",
                        title.content_title, cinema.name
                    ));
                    continue;
                }
            };
            if booking_ends_at <= now {
                continue;
            }
            open_booking_ends.push((booking.id, booking_ends_at));
            let mut add = |screen: Option<&str>, item, subject: String, expires_at| {
                if expires_at < booking_ends_at {
                    report.bookings.push(BookingExpiry {
                        booking_id: booking.id,
                        content_title: title.content_title.clone(),
                        cinema: cinema.name.clone(),
                        screen: screen.map(str::to_string),
                        item,
                        subject,
                        expires_at,
                        booking_ends_at,
                    });
                }
            };
            add(
                None,
                ExpiringItem::Dkdm,
                title.cpl_id.clone(),
                dkdm_expires_at,
            );
            for screen in booked {
                let (subject, expires_at) = leaf_expiry(&screen.cert.pem()?)?;
                add(
                    Some(&screen.name),
                    ExpiringItem::Recipient,
                    subject,
                    expires_at,
                );
                for device in &screen.authorized_devices {
                    let (subject, expires_at) = leaf_expiry(&device.certificate)?;
                    add(
                        Some(&screen.name),
                        ExpiringItem::AuthorizedDevice,
                        subject,
                        expires_at,
                    );
                }
            }
        }
    }
    for certificate in chain_from_files(signer_chain)? {
        let (subject, expires_at) = certificate_expiry(&certificate)?;
        let mut bookings_ending_after: Vec<BookingId> = open_booking_ends
            .iter()
            .filter(|(_, ends)| *ends > expires_at)
            .map(|(booking_id, _)| *booking_id)
            .collect();
        bookings_ending_after.dedup();
        report.signer_chain.push(SignerCertificateExpiry {
            subject,
            expires_at,
            bookings_ending_after,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::database::ScreenId;
    use crate::kdm_distribution::test_support::{
        DCNC_TITLE, chain_screen, cinemas, dkdm, fixtures, local_window, short_lived_leaf,
    };
    use crate::kdm_distribution::window::LocalWindow;

    const SHORT_LIVED_DAYS: u32 = 5;
    const DKDM_DAYS: i64 = 5;

    #[test]
    fn a_certificate_or_dkdm_ending_inside_an_open_booking_is_listed_with_the_screen_it_breaks() {
        let f = fixtures();
        let directory = tempfile::tempdir().unwrap();
        let short_lived =
            short_lived_leaf(directory.path(), "SM.Vendor.IMB.1009", SHORT_LIVED_DAYS);
        let (mut rex, mut odeon) = cinemas();
        rex.screens[1] = chain_screen("2", "1009", &short_lived.certificate, &[]);
        odeon.time_zone = None;
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let mut screens = Vec::new();
        for cinema in [&rex, &odeon] {
            let id = database.save_cinema(cinema).unwrap().cinema_id;
            screens.extend(database.cinema(id).unwrap().screen_ids);
        }
        let title = database
            .add_title_from_dkdm(&dkdm(DCNC_TITLE, DKDM_DAYS))
            .unwrap();
        let window = local_window();
        let open = database
            .add_booking(title, &screens, window, None, Utc::now())
            .unwrap();
        let today = Utc::now().date_naive();
        let finished = LocalWindow {
            start: (today - chrono::Duration::days(10))
                .and_hms_opt(18, 0, 0)
                .unwrap(),
            end: (today - chrono::Duration::days(5))
                .and_hms_opt(23, 0, 0)
                .unwrap(),
        };
        database
            .add_booking(title, &screens[..2], finished, None, Utc::now())
            .unwrap();

        let signer_chain = [
            short_lived.certificate.clone(),
            f.distributor_signer.clone(),
        ];
        let report = expiry_report(&database, &signer_chain, Utc::now()).unwrap();
        let listed: Vec<(BookingId, &str, Option<&str>, ExpiringItem)> = report
            .bookings
            .iter()
            .map(|expiry| {
                (
                    expiry.booking_id,
                    expiry.cinema.as_str(),
                    expiry.screen.as_deref(),
                    expiry.item,
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (open, "Rex", None, ExpiringItem::Dkdm),
                (open, "Rex", Some("2"), ExpiringItem::Recipient),
            ]
        );
        let rex_end = kdm_window_in_time_zone(&window, "Europe/London")
            .unwrap()
            .end;
        for expiry in &report.bookings {
            assert_eq!(expiry.booking_ends_at, rex_end);
            assert!(expiry.expires_at < rex_end);
            assert_eq!(expiry.content_title, DCNC_TITLE);
        }
        assert!(report.bookings[1].subject.contains("SM.Vendor.IMB.1009"));
        assert_eq!(report.not_checked.len(), 1);
        assert!(
            report.not_checked[0].contains("'Odeon' has no time zone"),
            "{:?}",
            report.not_checked
        );

        assert_eq!(report.signer_chain.len(), 2);
        assert_eq!(report.signer_chain[0].bookings_ending_after, vec![open]);
        assert!(report.signer_chain[1].bookings_ending_after.is_empty());
        assert!(report.signer_chain[1].subject.contains("Distributor"));
    }

    fn local_window_ending(end: &str) -> LocalWindow {
        let end = chrono::NaiveDateTime::parse_from_str(end, "%Y-%m-%d %H:%M").unwrap();
        LocalWindow {
            start: end - chrono::Duration::days(7),
            end,
        }
    }

    #[test]
    fn a_booking_is_listed_at_each_cinema_where_its_local_end_falls_inside_the_span() {
        let (rex, odeon) = cinemas();
        let mut database = DistributionDatabase::open_in_memory().unwrap();
        let rex_id = database.save_cinema(&rex).unwrap().cinema_id;
        let odeon_id = database.save_cinema(&odeon).unwrap().cinema_id;
        let rex_screen = database.cinema(rex_id).unwrap().screen_ids[0];
        let odeon_screen = database.cinema(odeon_id).unwrap().screen_ids[0];
        let title = database
            .add_title_from_dkdm(&dkdm(DCNC_TITLE, DKDM_DAYS))
            .unwrap();
        let mut book = |screens: &[ScreenId], end: &str| {
            database
                .add_booking(title, screens, local_window_ending(end), None, Utc::now())
                .unwrap()
        };
        let inside = book(&[rex_screen], "2026-11-07 23:00");
        book(&[rex_screen], "2026-11-20 23:00");
        book(&[rex_screen], "2026-11-01 23:00");
        // 10:00 in London is 10:00 UTC in November, in New York it is 15:00 UTC
        let both_zones = book(&[rex_screen, odeon_screen], "2026-11-08 10:00");

        let at = |time: &str| {
            DateTime::parse_from_rfc3339(time)
                .unwrap()
                .with_timezone(&Utc)
        };
        let now = at("2026-11-05T12:00:00Z");
        let ending = bookings_ending_within(&database, now, chrono::Duration::days(3)).unwrap();
        assert_eq!(
            ending,
            vec![
                BookingEnding {
                    booking_id: inside,
                    cinema_id: rex_id,
                    cinema: "Rex".into(),
                    ends_at: at("2026-11-07T23:00:00Z"),
                },
                BookingEnding {
                    booking_id: both_zones,
                    cinema_id: rex_id,
                    cinema: "Rex".into(),
                    ends_at: at("2026-11-08T10:00:00Z"),
                },
            ]
        );
    }
}
