//! Poll-based ingestion of location history from an OwnTracks Recorder.
//!
//! Fixes are split into segments wherever the phone went quiet or jumped, and
//! each segment becomes one activity named `owntracks:<device>:<first tst>`.

use std::collections::HashMap;

use anyhow::Result;
use geo::HaversineDistance;
use geo_types::{LineString, Point};
use serde::Deserialize;
use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::activity::{self, RawActivity};
use crate::db::{self, Database};
use crate::external::unwrap_response;
use crate::track_stats::{TrackPoint, TrackStats};

/// Fixes less accurate than this (meters) are dropped.
const MAX_ACCURACY: f64 = 100.0;

/// Consecutive fixes further apart than this (seconds or meters) start a new
/// segment.
const GAP_SECS: i64 = 60;
const GAP_METERS: f64 = 250.0;

/// Segments with fewer fixes than this are dropped.
const MIN_POINTS: usize = 5;

/// Request size when fetching long lookbacks (~35k fixes per week).
const CHUNK_DAYS: i64 = 7;

const TIME_FORMAT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

#[derive(Clone)]
pub struct OwntracksClient {
    http: reqwest::Client,
    url: String,
    user: String,
    /// `None` fetches every device of the user.
    device: Option<String>,
}

impl OwntracksClient {
    pub fn from_env() -> Option<Self> {
        let var = |key| std::env::var(key).ok().filter(|it: &String| !it.is_empty());

        Some(Self {
            http: reqwest::Client::new(),
            url: var("OWNTRACKS_URL")?.trim_end_matches('/').to_string(),
            user: var("OWNTRACKS_USER")?,
            device: var("OWNTRACKS_DEVICE"),
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Fix {
    /// Fix time reported by the phone (not when the Recorder received it).
    tst: i64,
    lat: f64,
    lon: f64,
    acc: Option<f64>,
}

impl Fix {
    fn point(&self) -> Point {
        Point::new(self.lon, self.lat)
    }
}

#[derive(Deserialize)]
struct ListResponse {
    results: Vec<String>,
}

#[derive(Deserialize)]
struct LocationsResponse {
    data: Vec<Value>,
}

impl OwntracksClient {
    async fn devices(&self) -> Result<Vec<String>> {
        let res = self
            .http
            .get(format!("{}/api/0/list", self.url))
            .query(&[("user", &self.user)])
            .send()
            .await?;

        Ok(unwrap_response::<ListResponse>(res).await?.results)
    }

    async fn locations(
        &self,
        device: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<Fix>> {
        let (from, to) = (from.format(TIME_FORMAT)?, to.format(TIME_FORMAT)?);
        let res = self
            .http
            .get(format!("{}/api/0/locations", self.url))
            .query(&[
                ("user", self.user.as_str()),
                ("device", device),
                ("from", &from),
                ("to", &to),
                ("fields", "tst,lat,lon,acc"),
            ])
            .send()
            .await?;

        // Skip over any records missing a position rather than failing the
        // whole fetch.
        let res: LocationsResponse = unwrap_response(res).await?;
        Ok(res
            .data
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Poll the Recorder and ingest any new segments. Returns the number added.
    pub async fn fetch(
        &self,
        db: &Database,
        db_config: &db::Config,
        lookback_days: u32,
    ) -> Result<usize> {
        let now = OffsetDateTime::now_utc();
        let mut window_start = now - Duration::days(lookback_days.into());

        if let Some(cutoff) = db_config.fetch_cutoff {
            window_start = window_start.max(cutoff.midnight().assume_utc());
        }

        let devices = match &self.device {
            Some(device) => vec![device.clone()],
            None => self.devices().await?,
        };

        let known = {
            let conn = db.connection()?;
            activity::known_files(&conn)?
        };

        let mut added = 0;
        for device in devices {
            // ponytail: holds the whole window in memory (~1.7M fixes/year at
            // 40 B each), stream chunk by chunk if backfills get that long.
            let mut fixes = vec![];
            let mut from = window_start;
            while from < now {
                let to = (from + Duration::days(CHUNK_DAYS)).min(now);
                fixes.extend(self.locations(&device, from, to).await?);
                from = to;
            }

            let window = (window_start.unix_timestamp(), now.unix_timestamp());
            for segment in segments(fixes, window) {
                let name = format!("owntracks:{device}:{}", segment[0].tst);
                if known.contains(&name) {
                    continue;
                }

                let mut conn = db.connection()?;
                match activity::upsert(&mut conn, &name, to_activity(&segment), db_config) {
                    Ok(_) => {
                        tracing::info!("imported owntracks segment {name}");
                        added += 1;
                    }
                    Err(e) => tracing::error!("failed to ingest owntracks segment {name}: {e}"),
                }
            }
        }

        Ok(added)
    }
}

fn is_gap(a: &Fix, b: &Fix) -> bool {
    b.tst - a.tst > GAP_SECS || a.point().haversine_distance(&b.point()) > GAP_METERS
}

/// Split fixes into segments, dropping inaccurate fixes and short segments.
///
/// Segments which come within `GAP_SECS` of either end of the `(start, end)`
/// window are also dropped: they may continue outside of it, so they are
/// either still in progress or truncated. Importing those would produce a
/// different (duplicate) segment once a later fetch sees the rest of it.
fn segments(mut fixes: Vec<Fix>, (start, end): (i64, i64)) -> Vec<Vec<Fix>> {
    fixes.retain(|f| f.acc.is_none_or(|acc| acc <= MAX_ACCURACY));
    fixes.sort_by_key(|f| f.tst);

    fixes
        .chunk_by(|a, b| !is_gap(a, b))
        .filter(|s| {
            s.len() >= MIN_POINTS
                && s[0].tst - start > GAP_SECS
                && end - s[s.len() - 1].tst > GAP_SECS
        })
        .map(<[Fix]>::to_vec)
        .collect()
}

fn to_activity(segment: &[Fix]) -> RawActivity {
    let points: Vec<TrackPoint> = segment
        .iter()
        .map(|f| TrackPoint {
            point: f.point(),
            elevation: None,
            timestamp: Some(f.tst),
        })
        .collect();

    let mut properties = HashMap::from([("activity_type".to_string(), Value::from("OwnTracks"))]);
    TrackStats::from_points(&points).merge_into(&mut properties);

    RawActivity {
        title: None,
        start_time: OffsetDateTime::from_unix_timestamp(segment[0].tst).ok(),
        tracks: points
            .iter()
            .map(|p| p.point)
            .collect::<LineString>()
            .into(),
        properties,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fix `secs` into the window and `north` meters (roughly) north of
    /// the origin.
    fn fix(secs: i64, north: f64, acc: f64) -> Fix {
        Fix {
            tst: 1_000 + secs,
            lat: 52.0 + north / 111_000.0,
            lon: 13.0,
            acc: Some(acc),
        }
    }

    /// `n` accurate fixes every 10s, walking 10m north each time.
    fn walk(start_secs: i64, start_north: f64, n: i64) -> Vec<Fix> {
        (0..n)
            .map(|i| fix(start_secs + i * 10, start_north + i as f64 * 10.0, 5.0))
            .collect()
    }

    const WINDOW: (i64, i64) = (0, 100_000);

    fn starts(segments: &[Vec<Fix>]) -> Vec<(i64, usize)> {
        segments
            .iter()
            .map(|s| (s[0].tst - 1_000, s.len()))
            .collect()
    }

    #[test]
    fn splits_on_time_and_distance_gaps() {
        let mut fixes = walk(0, 0.0, 5);
        // 61s pause
        fixes.extend(walk(101, 40.0, 5));
        // 300m jump
        fixes.extend(walk(151, 380.0, 6));
        // too short to keep
        fixes.extend(walk(1_000, 0.0, 4));

        assert_eq!(
            starts(&segments(fixes, WINDOW)),
            vec![(0, 5), (101, 5), (151, 6)]
        );
    }

    #[test]
    fn sorts_by_fix_time() {
        let mut fixes = walk(0, 0.0, 6);
        fixes.reverse();

        assert_eq!(starts(&segments(fixes, WINDOW)), vec![(0, 6)]);
    }

    #[test]
    fn drops_inaccurate_fixes() {
        let mut fixes = walk(0, 0.0, 5);
        // A wild fix 1km off would otherwise split this into two short
        // segments.
        fixes.insert(2, fix(15, 1_000.0, 500.0));

        assert_eq!(starts(&segments(fixes.clone(), WINDOW)), vec![(0, 5)]);

        fixes[2].acc = Some(MAX_ACCURACY);
        assert!(segments(fixes, WINDOW).is_empty());
    }

    #[test]
    fn skips_segments_touching_window_edges() {
        let mut fixes = walk(0, 0.0, 5);
        fixes.extend(walk(500, 0.0, 5));
        fixes.extend(walk(1_000, 0.0, 5));
        let last_tst = fixes.last().unwrap().tst;

        // First starts within GAP_SECS of the window start, so it may have
        // been cut off.
        let start = 1_000 - GAP_SECS;
        // Last ends within GAP_SECS of now, so it may still be in progress.
        let end = last_tst + GAP_SECS;

        assert_eq!(
            starts(&segments(fixes.clone(), (start, end))),
            vec![(500, 5)]
        );
        assert_eq!(
            starts(&segments(fixes, (start - 1, end + 1))),
            vec![(0, 5), (500, 5), (1_000, 5)]
        );
    }

    #[test]
    fn segment_to_activity() {
        let activity = to_activity(&walk(0, 0.0, 5));

        assert_eq!(activity.start_time.unwrap().unix_timestamp(), 1_000);
        assert_eq!(activity.tracks.0.len(), 1);
        assert_eq!(activity.tracks.0[0].0.len(), 5);
        assert_eq!(
            activity.properties["activity_type"],
            Value::from("OwnTracks")
        );
        assert!(activity.properties.contains_key("average_speed"));
    }
}
