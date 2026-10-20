//! Find the activities whose tracks pass near a point.
//!
//! Candidates are the stored tiles at the highest zoom level that overlap the
//! circle's bounding box. Each candidate line is then measured exactly against
//! the point. Masked vertices are dropped first, as the renderer does, so the
//! results match what the map draws.

use std::collections::HashMap;
use std::f64::consts::PI;

use anyhow::{Result, anyhow};
use geo::EuclideanDistance;
use geo_types::{Coord, Line, Point};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::db::{ActivityFilter, Config, Database, decode_line};
use crate::tile::{LngLat, Tile};

/// Activities returned per page, newest first.
const PAGE_SIZE: usize = 100;

#[derive(Debug, Serialize)]
pub struct Nearby {
    /// Number of matching activities, which can exceed `activities.len()`.
    total: usize,
    activities: Vec<NearbyActivity>,
}

#[derive(Debug, Serialize)]
pub struct NearbyActivity {
    id: i64,
    file: String,
    name: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    start_time: Option<OffsetDateTime>,
    activity_type: Option<String>,
    /// Kilometers.
    total_distance: Option<f64>,
    /// Closest approach to the point, in meters.
    distance: f64,
    // ponytail: every row carries its properties (~2 KB for a Strava ride),
    // so ~200 KB a page. Fetch them per activity if that gets slow.
    properties: Value,
}

/// One pixel plane for all stored tiles of the highest zoom level, with y
/// growing southward, so lines from neighboring tiles can be compared.
struct Grid {
    z: u8,
    extent: f64,
    /// Web Mercator meters per pixel.
    merc_per_px: f64,
    /// Activity masks as (center, radius) in pixels.
    masks: Vec<(Coord, f64)>,
}

impl Grid {
    fn new(config: &Config) -> Self {
        let z = *config.zoom_levels.iter().max().expect("no zoom levels");
        let bounds = Tile::new(0, 0, z).xy_bounds();
        let merc_per_px = (bounds.right - bounds.left) / config.tile_extent as f64;

        let masks = config
            .activity_mask
            .iter()
            .filter_map(|m| {
                let center = LngLat::new(m.lng, m.lat).xy()?;
                let radius = m.radius * center.meter_scale() / merc_per_px;
                Some((center.to_mercator_pixel(z, config.tile_extent), radius))
            })
            .collect();

        Self {
            z,
            extent: config.tile_extent as f64,
            merc_per_px,
            masks,
        }
    }

    /// The point in pixels, and the number of pixels per ground meter there.
    fn project(&self, pt: LngLat) -> Option<(Coord, f64)> {
        let merc = pt.xy()?;
        let px = merc.to_mercator_pixel(self.z, self.extent as u32);
        Some((px, merc.meter_scale() / self.merc_per_px))
    }

    fn lng_lat(&self, p: Coord) -> [f64; 2] {
        let world = (1u64 << self.z) as f64 * self.extent;
        let lng = p.x / world * 360.0 - 180.0;
        let lat = (PI * (1.0 - 2.0 * p.y / world)).sinh().atan().to_degrees();
        // About 10 cm, keeps the JSON small.
        [lng, lat].map(|v| (v * 1e6).round() / 1e6)
    }

    fn decode(&self, tile: &Tile, coords: &[u8]) -> Vec<Coord> {
        decode_line(coords)
            .map(|c| Coord {
                x: tile.x as f64 * self.extent + c.x as f64,
                // Stored y counts up from the bottom of the tile.
                y: (tile.y + 1) as f64 * self.extent - c.y as f64,
            })
            .collect()
    }

    /// The parts of a line that the renderer draws: masked vertices are
    /// dropped and break the line.
    fn visible<'a>(&'a self, line: &'a [Coord]) -> impl Iterator<Item = &'a [Coord]> {
        line.split(|p| {
            self.masks
                .iter()
                .any(|(c, r)| (p.x - c.x).powi(2) + (p.y - c.y).powi(2) <= r * r)
        })
        .filter(|run| run.len() > 1)
    }
}

/// Smallest distance from `pt` to any segment of the runs.
fn closest<'a>(pt: Coord, runs: impl Iterator<Item = &'a [Coord]>) -> Option<f64> {
    runs.flat_map(|run| run.windows(2))
        .map(|w| Point::from(pt).euclidean_distance(&Line::new(w[0], w[1])))
        .min_by(f64::total_cmp)
}

/// Activities matching `filter` that pass within `radius` meters of `pt`,
/// skipping the newest `offset`.
pub fn find(
    db: &Database,
    config: &Config,
    pt: LngLat,
    radius: f64,
    filter: &ActivityFilter,
    offset: usize,
) -> Result<Nearby> {
    let nearest = scan(db, config, pt, radius, filter)?;

    let ids = serde_json::to_string(&nearest.keys().collect::<Vec<_>>())?;
    let conn = db.connection()?;
    let mut stmt = conn.prepare(
        "\
        SELECT \
            id, \
            file, \
            CAST(COALESCE(title, properties ->> 'activity_name') AS TEXT), \
            start_time, \
            CAST(properties ->> 'activity_type' AS TEXT), \
            CAST(properties ->> 'total_distance' AS REAL), \
            JSON(properties) \
        FROM activities \
        WHERE id IN (SELECT value FROM json_each(?)) \
        ORDER BY start_time DESC, id DESC \
        LIMIT ? OFFSET ?",
    )?;

    let activities = stmt
        .query_map(params![ids, PAGE_SIZE, offset], |row| {
            let id = row.get(0)?;
            Ok(NearbyActivity {
                id,
                file: row.get(1)?,
                name: row.get(2)?,
                start_time: row.get(3)?,
                activity_type: row.get(4)?,
                total_distance: row.get(5)?,
                distance: nearest[&id],
                properties: serde_json::from_str(&row.get::<_, String>(6)?)
                    .expect("activity `properties` should contain valid JSON"),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(Nearby {
        total: nearest.len(),
        activities,
    })
}

/// Ids of the activities passing within `radius` meters of `pt`, as a JSON
/// array, for the filter language's `near()`.
pub fn ids_near(db: &Database, pt: LngLat, radius: f64) -> Result<String> {
    // Every map tile asks again, so keep the answer until activities change.
    let (count, last): (i64, i64) = db.connection()?.query_row(
        "SELECT count(*), COALESCE(max(id), 0) FROM activities",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let key = format!("{},{},{radius},{count},{last}", pt.0.x(), pt.0.y());

    // Held while scanning, so a screenful of tiles waits for one scan instead
    // of each running its own.
    let mut cache = db.near_ids.lock().unwrap();
    if let Some(ids) = cache.get(&key) {
        return Ok(ids.clone());
    }
    // ponytail: forgets everything when full, LRU if many circles are in use.
    if cache.len() >= 32 {
        cache.clear();
    }

    let config = db.load_config()?;
    let nearest = scan(db, &config, pt, radius, &ActivityFilter::default())?;
    let ids = serde_json::to_string(&nearest.keys().collect::<Vec<_>>())?;
    cache.insert(key, ids.clone());
    Ok(ids)
}

/// Closest approach in meters of each activity matching `filter` that passes
/// within `radius` meters of `pt`.
fn scan(
    db: &Database,
    config: &Config,
    pt: LngLat,
    radius: f64,
    filter: &ActivityFilter,
) -> Result<HashMap<i64, f64>> {
    filter.resolve(db)?;
    let grid = Grid::new(config);
    let (center, px_per_meter) = grid
        .project(pt)
        .ok_or_else(|| anyhow!("point out of bounds"))?;
    let radius_px = radius * px_per_meter;

    // ponytail: no wrap-around, so circles crossing the antimeridian miss the
    // far side.
    let max_tile = (1u32 << grid.z) - 1;
    let tile = |v: f64| ((v / grid.extent).floor().max(0.0) as u32).min(max_tile);
    let (xmin, xmax) = (tile(center.x - radius_px), tile(center.x + radius_px));
    let (ymin, ymax) = (tile(center.y - radius_px), tile(center.y + radius_px));

    let mut params = params![grid.z, xmin, xmax, ymin, ymax].to_vec();
    let (expr, join) = if filter.is_empty() {
        (String::from("true"), "")
    } else {
        (
            filter.to_query(&mut params),
            "JOIN activities ON activities.id = activity_tiles.activity_id ",
        )
    };

    let conn = db.connection()?;
    let mut stmt = conn.prepare(&format!(
        "\
        SELECT activity_id, x, y, coords \
        FROM activity_tiles \
        {join}\
        WHERE z = ? \
            AND (x BETWEEN ? AND ?) \
            AND (y BETWEEN ? AND ?) \
            AND {expr};",
    ))?;

    let mut nearest: HashMap<i64, f64> = HashMap::new();
    let mut rows = stmt.query(params.as_slice())?;
    while let Some(row) = rows.next()? {
        let tile = Tile::new(row.get_unwrap(1), row.get_unwrap(2), grid.z);
        let line = grid.decode(&tile, row.get_ref(3)?.as_blob()?);

        if let Some(dist) = closest(center, grid.visible(&line))
            && dist <= radius_px
        {
            let best = nearest.entry(row.get_unwrap(0)).or_insert(dist);
            *best = best.min(dist);
        }
    }

    nearest.values_mut().for_each(|px| *px /= px_per_meter);
    Ok(nearest)
}

/// An activity's track as a GeoJSON MultiLineString, without masked parts.
pub fn track(db: &Database, config: &Config, id: i64) -> Result<Value> {
    let grid = Grid::new(config);
    let conn = db.connection()?;
    let mut stmt =
        conn.prepare("SELECT x, y, coords FROM activity_tiles WHERE activity_id = ? AND z = ?")?;

    let mut lines = vec![];
    let mut rows = stmt.query(params![id, grid.z])?;
    while let Some(row) = rows.next()? {
        let tile = Tile::new(row.get_unwrap(0), row.get_unwrap(1), grid.z);
        let line = grid.decode(&tile, row.get_ref(2)?.as_blob()?);
        lines.extend(
            grid.visible(&line)
                .map(|run| run.iter().map(|p| grid.lng_lat(*p)).collect::<Vec<_>>()),
        );
    }

    Ok(json!({ "type": "MultiLineString", "coordinates": lines }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use geo_types::{LineString, MultiLineString};

    use super::*;
    use crate::activity::{RawActivity, upsert};
    use crate::db::ActivityMask;

    /// Ground meters per degree of latitude on the sphere hotpot projects onto.
    const METERS_PER_DEG: f64 = 6_378_137.0 * PI / 180.0;
    const LAT: f64 = 51.5;

    fn north(meters: f64) -> f64 {
        LAT + meters / METERS_PER_DEG
    }

    /// A ~700 m ride along `LAT` (spanning several z16 tiles) and a ~70 m run
    /// 200 m north of it. The run is newer. The ride has a point every ~35 m
    /// like real GPS data, since the tile clipper skips the middle tiles of a
    /// segment spanning more than two.
    fn fixture(masks: Vec<ActivityMask>) -> (Database, Config) {
        let db = Database::memory().unwrap();
        let config = Config {
            activity_mask: masks,
            ..Config::default()
        };

        let tracks = [
            (
                "ride",
                "Ride",
                1_600_000_000,
                (0..=20).map(|i| (-0.13 + i as f64 * 0.0005, LAT)).collect(),
            ),
            (
                "run",
                "Run",
                1_700_000_000,
                vec![(-0.1255, north(200.0)), (-0.1245, north(200.0))],
            ),
        ];

        for (name, kind, ts, line) in tracks {
            let activity = RawActivity {
                title: Some(name.to_string()),
                start_time: Some(OffsetDateTime::from_unix_timestamp(ts).unwrap()),
                properties: HashMap::from([("activity_type".to_string(), kind.into())]),
                tracks: MultiLineString::from(LineString::from(line)),
            };
            upsert(&mut db.connection().unwrap(), name, activity, &config).unwrap();
        }

        (db, config)
    }

    fn names(
        db: &Database,
        config: &Config,
        meters_north: f64,
        radius: f64,
        filter: &str,
    ) -> Vec<String> {
        let props = (!filter.is_empty()).then(|| filter.parse().unwrap());
        let filter = ActivityFilter::new(None, None, props);
        let pt = LngLat::new(-0.125, north(meters_north));

        find(db, config, pt, radius, &filter, 0)
            .unwrap()
            .activities
            .into_iter()
            .map(|a| a.name.unwrap())
            .collect()
    }

    #[test]
    fn test_distance_is_ground_meters() {
        let (db, config) = fixture(vec![]);
        let pt = LngLat::new(-0.125, north(30.0));
        let found = find(&db, &config, pt, 50.0, &ActivityFilter::default(), 0).unwrap();

        assert_eq!(found.total, 1);
        let dist = found.activities[0].distance;
        assert!((dist - 30.0).abs() < 0.5, "expected ~30 m, got {dist}");
        assert_eq!(found.activities[0].properties["activity_type"], "Ride");
    }

    #[test]
    fn test_radius_bounds_matches() {
        let (db, config) = fixture(vec![]);

        assert!(names(&db, &config, 30.0, 25.0, "").is_empty());
        assert_eq!(names(&db, &config, 30.0, 35.0, ""), ["ride"]);
        // Newest first.
        assert_eq!(names(&db, &config, 100.0, 150.0, ""), ["run", "ride"]);
    }

    #[test]
    fn test_offset_skips_newest() {
        let (db, config) = fixture(vec![]);
        let pt = LngLat::new(-0.125, north(100.0));
        let found = find(&db, &config, pt, 150.0, &ActivityFilter::default(), 1).unwrap();

        assert_eq!(found.total, 2);
        assert_eq!(found.activities.len(), 1);
        assert_eq!(found.activities[0].name.as_deref(), Some("ride"));
    }

    #[test]
    fn test_respects_filter() {
        let (db, config) = fixture(vec![]);
        assert_eq!(
            names(&db, &config, 100.0, 150.0, "activity_type = Ride"),
            ["ride"]
        );
    }

    #[test]
    fn test_near_filter() {
        let (db, config) = fixture(vec![]);
        let run = format!("near({}, -0.125, 10)", north(200.0));

        assert_eq!(names(&db, &config, 100.0, 150.0, &run), ["run"]);
        assert_eq!(
            names(&db, &config, 100.0, 150.0, &format!("!({run})")),
            ["ride"]
        );

        let filter = ActivityFilter::new(None, None, Some(run.parse().unwrap()));
        assert_eq!(db.activity_count(&filter).unwrap(), 1);
        assert_eq!(db.activity_info(&filter).unwrap().len(), 1);
    }

    #[test]
    fn test_ignores_masked_parts() {
        let mask = ActivityMask {
            name: "run".to_string(),
            lat: north(200.0),
            lng: -0.125,
            radius: 60.0,
        };
        let (db, config) = fixture(vec![mask]);

        assert_eq!(names(&db, &config, 100.0, 150.0, ""), ["ride"]);
    }

    #[test]
    fn test_track_round_trips_coordinates() {
        let (db, config) = fixture(vec![]);
        let geojson = track(&db, &config, 1).unwrap();
        let lines: Vec<Vec<[f64; 2]>> =
            serde_json::from_value(geojson["coordinates"].clone()).unwrap();

        let points: Vec<_> = lines.concat();
        let (lng_min, lng_max) = points.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| {
            (lo.min(p[0]), hi.max(p[0]))
        });

        assert!((lng_min + 0.13).abs() < 1e-5, "{lng_min}");
        assert!((lng_max + 0.12).abs() < 1e-5, "{lng_max}");
        assert!(points.iter().all(|p| (p[1] - LAT).abs() < 1e-5));
    }
}
