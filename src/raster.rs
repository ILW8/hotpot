use std::error::Error;
use std::fmt::Display;
use std::str::FromStr;

use anyhow::{Result, anyhow};
use geo_types::Coord;
use rayon::prelude::*;
use rusqlite::{ToSql, params};
use serde::{Deserialize, Deserializer};

use crate::db::{ActivityFilter, Config, Database, decode_line};
use crate::tile::{Tile, TileActivityMask, TileBounds, WebMercatorViewport};

pub static PINKISH: LinearGradient = LinearGradient::from_stops(&[
    (1, [0xff, 0xb1, 0xff, 0x7f]),
    (10, [0xff, 0xb1, 0xff, 0xff]),
    (50, [0xff, 0xff, 0xff, 0xff]),
]);

pub static BLUE_RED: LinearGradient = LinearGradient::from_stops(&[
    (1, [0x3f, 0x5e, 0xfb, 0xff]),
    (10, [0xfc, 0x46, 0x6b, 0xff]),
    (50, [0xff, 0xff, 0xff, 0xff]),
]);

pub static RED: LinearGradient = LinearGradient::from_stops(&[
    (1, [0xb2, 0x0a, 0x2c, 0xff]),
    (10, [0xff, 0xfb, 0xd5, 0xff]),
    (50, [0xff, 0xff, 0xff, 0xff]),
]);

pub static ORANGE: LinearGradient = LinearGradient::from_stops(&[
    (1, [0xfc, 0x4a, 0x1a, 0xff]),
    (10, [0xf7, 0xb7, 0x33, 0xff]),
    (50, [0xfd, 0xed, 0xce, 0xff]),
]);

pub struct TileRaster {
    bounds: TileBounds,
    /// Width of the target tile in source tile coordinates.
    source_width: u64,
    width: u32,
    tile_extent: u32,
    pixels: Vec<u8>,
}

impl TileRaster {
    /// `width` can be any size, so views can be rendered at fractional zoom levels.
    fn new(tile: Tile, source: TileBounds, width: u32, tile_extent: u32) -> Self {
        debug_assert!(source.z >= tile.z, "source zoom must be >= target zoom");

        let zoom_steps = (source.z - tile.z) as u32;

        Self {
            width,
            tile_extent,
            pixels: vec![0; (width * width) as usize],
            bounds: source,
            source_width: (tile_extent as u64) << zoom_steps,
        }
    }

    fn add_activity(
        &mut self,
        source_tile: &Tile,
        coords: impl IntoIterator<Item = Coord<u32>>,
        mask: &TileActivityMask,
    ) {
        debug_assert_eq!(source_tile.z, self.bounds.z);

        // Origin of source tile within target tile
        let x_offset = self.tile_extent * (source_tile.x - self.bounds.xmin);
        let y_offset = self.tile_extent * (source_tile.y - self.bounds.ymin);

        let tile_bbox = crate::tile::BBox::square(self.width as f64 - 1.0);

        let mut prev = None;
        for Coord { x, y } in coords {
            // Translate (x,y) to location in target tile.
            // [0..(width * STORED_TILE_WIDTH)]
            let x = x + x_offset;
            let y = (self.tile_extent - y) + y_offset;

            // Scale the coordinates to [0..width]. Same as a right shift when
            // width is a power of two.
            let x = (x as u64 * self.width as u64 / self.source_width) as u32;
            let y = (y as u64 * self.width as u64 / self.source_width) as u32;

            // Apply mask in tile pixel space
            if mask.is_hidden(x as i32, y as i32) {
                // Break the line
                prev = None;
                continue;
            }

            let Some(Coord { x: px, y: py }) = prev else {
                prev = Some(Coord { x, y });
                continue;
            };

            if x == px && y == py {
                continue;
            }

            // Pre-clamp the coordinates to the target tile bounds so we can
            // avoid a bounds check in the loop
            let Some((start, end)) = tile_bbox.clip_line(
                &geo::Point::new(px as f64, py as f64).into(),
                &geo::Point::new(x as f64, y as f64).into(),
            ) else {
                continue;
            };

            let line_iter = line_drawing::Bresenham::<i32>::new(
                (start.0.x() as i32, start.0.y() as i32),
                (end.0.x() as i32, end.0.y() as i32),
            );

            for (ix, iy) in line_iter {
                let idx = (iy as u32 * self.width + ix as u32) as usize;
                self.pixels[idx] = self.pixels[idx].saturating_add(1);
            }
            prev = Some(Coord { x, y });
        }
    }

    fn enumerate_pixels(&self) -> EnumerateRasterPixels<'_> {
        EnumerateRasterPixels {
            width: self.width as usize,
            idx: 0,
            pixels: self.pixels.as_ref(),
        }
    }

    pub fn encode_png(&self, gradient: &LinearGradient) -> Vec<u8> {
        encode_indexed_png(&self.pixels, self.width, self.width, gradient)
    }
}

fn encode_indexed_png(
    pixels: &[u8],
    width: u32,
    height: u32,
    gradient: &LinearGradient,
) -> Vec<u8> {
    debug_assert_eq!(pixels.len(), (width * height) as usize);

    let (palette, trns) = gradient.as_png_palette();

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(palette.as_slice());
        encoder.set_trns(trns.as_slice());
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::NoFilter);

        let mut writer = encoder.write_header().expect("write png header");
        writer.write_image_data(pixels).expect("write png data");
    }

    bytes
}

/// Encode an image of the given size with no activity data, i.e. every pixel at
/// palette index 0.
pub fn encode_empty_png(width: u32, height: u32, gradient: &LinearGradient) -> Vec<u8> {
    encode_indexed_png(&vec![0; (width * height) as usize], width, height, gradient)
}

/// Linearly interpolate between two colors
const fn lerp(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    [
        (a[0] as f32 * (1.0 - t) + b[0] as f32 * t) as u8,
        (a[1] as f32 * (1.0 - t) + b[1] as f32 * t) as u8,
        (a[2] as f32 * (1.0 - t) + b[2] as f32 * t) as u8,
        (a[3] as f32 * (1.0 - t) + b[3] as f32 * t) as u8,
    ]
}

struct EnumerateRasterPixels<'a> {
    width: usize,
    idx: usize,
    pixels: &'a [u8],
}

impl Iterator for EnumerateRasterPixels<'_> {
    type Item = (usize, usize, u8);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= (self.width * self.width) {
            None
        } else {
            let pixel = self.pixels[self.idx];
            let x = self.idx % self.width;
            let y = self.idx / self.width;
            self.idx += 1;
            Some((x, y, pixel))
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinearGradient {
    rgb: [u8; 768],
    alpha: [u8; 256],
}

impl LinearGradient {
    pub const fn from_stops(stops: &[(u8, [u8; 4])]) -> Self {
        let mut gradient = LinearGradient {
            rgb: [0; 768],
            alpha: [0; 256],
        };

        let mut w = 0;
        while w + 1 < stops.len() {
            let (start_idx, start_color) = stops[w];
            let (end_idx, end_color) = stops[w + 1];

            if start_idx <= end_idx {
                let mut i = start_idx;
                loop {
                    let color = lerp(
                        start_color,
                        end_color,
                        (i - start_idx) as f32 / (end_idx - start_idx) as f32,
                    );
                    gradient.set(i, color);
                    if i == end_idx {
                        break;
                    }
                    i += 1;
                }
            }
            w += 1;
        }

        // Copy the last color to the end of the palette
        if !stops.is_empty() {
            let (last_idx, color) = stops[stops.len() - 1];
            let mut i = last_idx;
            loop {
                gradient.set(i, color);
                if i == u8::MAX {
                    break;
                }
                i += 1;
            }
        }

        gradient
    }

    #[inline]
    const fn set(&mut self, idx: u8, color: [u8; 4]) {
        let i = idx as usize;
        self.rgb[i * 3] = color[0];
        self.rgb[i * 3 + 1] = color[1];
        self.rgb[i * 3 + 2] = color[2];
        self.alpha[i] = color[3];
    }

    pub fn as_png_palette(&self) -> (&[u8; 768], &[u8; 256]) {
        (&self.rgb, &self.alpha)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct LinearGradientParseError;
impl Display for LinearGradientParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("invalid linear gradient"))
    }
}
impl Error for LinearGradientParseError {}

/*
TODO: support varying stops per-zoom level. Possible format:

   {
       "palette": ["789", "334455", "ffffff33"],
       "stops": [
           [0,  [75, 175, 250]],
           [10, [25, 50, 75]],
           [15, [5, 10, 15]]
       ]
   }
*/
impl FromStr for LinearGradient {
    type Err = LinearGradientParseError;

    /// Parse a string containing a list of stop points and colors, separated by
    /// a `;`.
    ///
    /// Colors may be written as `RGB`, `RRGGBB`, or `RRGGBBAA`
    ///
    /// For example: `0:001122;25:789;50:334455;75:ffffff33`
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let stops: Vec<(u8, [u8; 4])> = s
            .split(';')
            .map(|part| {
                let (threshold, color) = part.split_once(':').ok_or(LinearGradientParseError)?;
                let threshold = threshold
                    .parse::<u8>()
                    .map_err(|_| LinearGradientParseError)?;
                let color = {
                    let rgba = match color.len() {
                        3 => {
                            let rgb: String = color.chars().flat_map(|ch| [ch, ch]).collect();
                            format!("{}FF", rgb)
                        }
                        6 => format!("{color}FF"),
                        8 => color.to_string(),
                        _ => return Err(LinearGradientParseError),
                    };

                    u32::from_str_radix(&rgba, 16).map_err(|_| LinearGradientParseError)?
                };

                Ok((threshold, color.to_be_bytes()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(LinearGradient::from_stops(&stops))
    }
}

impl<'de> Deserialize<'de> for LinearGradient {
    fn deserialize<D>(deserializer: D) -> Result<LinearGradient, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        LinearGradient::from_str(&s).map_err(|_| serde::de::Error::custom("invalid gradient"))
    }
}

pub fn render_view(
    viewport: WebMercatorViewport,
    gradient: &LinearGradient,
    width: u32,
    height: u32,
    filter: &ActivityFilter,
    db: &Database,
    config: &Config,
) -> Result<Vec<u8>> {
    let base = 256;
    let min_zoom = *config.zoom_levels.iter().min().unwrap() as i32;
    let max_zoom = *config.zoom_levels.iter().max().unwrap() as i32;

    // Fractional zoom at which the viewport just fits the image.
    let sw = viewport.sw.to_mercator_pixel(0, base);
    let ne = viewport.ne.to_mercator_pixel(0, base);
    let scale = f64::max((ne.x - sw.x) / width as f64, (sw.y - ne.y) / height as f64);
    if !(scale.is_finite() && scale > 0.0) {
        return Err(anyhow!("viewport has no area"));
    }
    let zoom = -scale.log2();

    // Rasterize tiles from the integer zoom level below at whatever pixel
    // size makes up the difference, so lines stay one pixel wide. The epsilon
    // keeps bounds that land exactly on a level from rounding down to the next.
    let z = ((zoom + 1e-6).floor() as i32).clamp(min_zoom, max_zoom) as u8;
    let tile_size = (base as f64 * (zoom - z as f64).exp2()).round().max(1.0) as u32;

    // Image origin in zoom `z` pixels at `tile_size`, centred on the viewport.
    let px_per_base = tile_size as f64 * (1u64 << z) as f64 / base as f64;
    let left = ((sw.x + ne.x) / 2.0 * px_per_base - width as f64 / 2.0).round() as i64;
    let top = ((sw.y + ne.y) / 2.0 * px_per_base - height as f64 / 2.0).round() as i64;

    let ts = tile_size as i64;
    let tiles = 0..(1i64 << z);
    let tile_positions: Vec<_> = (top.div_euclid(ts)..=(top + height as i64 - 1).div_euclid(ts))
        .flat_map(|ty| {
            (left.div_euclid(ts)..=(left + width as i64 - 1).div_euclid(ts)).map(move |tx| (tx, ty))
        })
        .filter(|(tx, ty)| tiles.contains(tx) && tiles.contains(ty))
        .collect();

    tracing::debug!(
        num_tiles = tile_positions.len(),
        zoom = z,
        tile_size,
        "rendering subtiles"
    );

    // Render tiles in parallel
    let tile_results: Vec<_> = tile_positions
        .par_iter()
        .map(|&(tx, ty)| {
            let tile = Tile::new(tx as u32, ty as u32, z);
            rasterize_tile(tile, tile_size, filter, db, config)
                .map(|img| img.map(|img| (tx * ts - left, ty * ts - top, img)))
        })
        .collect();

    let mut mosaic = vec![0u8; (width * height) as usize];
    for result in tile_results {
        if let Some((origin_x, origin_y, raster)) = result? {
            for (x, y, pixel) in raster.enumerate_pixels() {
                let (mx, my) = (origin_x + x as i64, origin_y + y as i64);
                if (0..width as i64).contains(&mx) && (0..height as i64).contains(&my) {
                    mosaic[(my as u32 * width + mx as u32) as usize] = pixel;
                }
            }
        }
    }

    Ok(encode_indexed_png(&mosaic, width, height, gradient))
}

pub fn rasterize_tile(
    tile: Tile,
    width: u32,
    filter: &ActivityFilter,
    db: &Database,
    config: &Config,
) -> Result<Option<TileRaster>> {
    let masks = &config.activity_mask;
    let zoom_level = config
        .source_level(tile.z)
        .ok_or_else(|| anyhow!("no source level for tile: {:?}", tile))?;

    let bounds = TileBounds::from(zoom_level, &tile);
    let mut raster = TileRaster::new(tile, bounds, width, config.tile_extent);

    let mask = tile.build_mask(masks, width as i32);

    let mut have_activity = false;

    let conn = db.connection()?;
    let (mut stmt, params) = prepare_activities_query(&conn, filter, &bounds)?;
    let mut rows = stmt.query(params.as_slice())?;
    while let Some(row) = rows.next()? {
        let source_tile = Tile::new(row.get_unwrap(0), row.get_unwrap(1), row.get_unwrap(2));

        let bytes = row
            .get_ref(3)?
            .as_bytes()
            .map_err(|_| anyhow!("expected blob for tile coordinates"))?;
        let coords = decode_line(bytes);

        raster.add_activity(&source_tile, coords, &mask);

        have_activity = true;
    }

    if !have_activity {
        return Ok(None);
    }

    Ok(Some(raster))
}

fn prepare_activities_query<'a>(
    conn: &'a rusqlite::Connection,
    filter: &'a ActivityFilter,
    bounds: &'a TileBounds,
) -> Result<(rusqlite::Statement<'a>, Vec<&'a dyn ToSql>)> {
    let mut params = params![bounds.z, bounds.xmin, bounds.xmax, bounds.ymin, bounds.ymax].to_vec();

    let (expr, join) = if filter.is_empty() {
        (String::from("true"), "")
    } else {
        (
            filter.to_query(&mut params),
            "JOIN activities ON activities.id = activity_tiles.activity_id ",
        )
    };

    let stmt = conn.prepare(&format!(
        "\
        SELECT x, y, z, coords \
        FROM activity_tiles \
        {join}\
        WHERE z = ? \
            AND (x >= ? AND x < ?) \
            AND (y >= ? AND y < ?) \
            AND {expr};",
    ))?;

    Ok((stmt, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_color(gradient: &LinearGradient, idx: u8) -> [u8; 4] {
        let (palette, trns) = gradient.as_png_palette();
        let i = idx as usize;
        [
            palette[i * 3],
            palette[i * 3 + 1],
            palette[i * 3 + 2],
            trns[i],
        ]
    }

    #[test]
    fn test_linear_gradient_parse() {
        let gradient = "1:001122;10:789;100:334455;200:ffffff33"
            .parse::<LinearGradient>()
            .unwrap();
        assert_eq!(palette_color(&gradient, 0), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(palette_color(&gradient, 1), [0x00, 0x11, 0x22, 0xff]);
        assert_eq!(palette_color(&gradient, 10), [0x77, 0x88, 0x99, 0xff]);
        assert_eq!(palette_color(&gradient, 100), [0x33, 0x44, 0x55, 0xff]);
        // Last value should be copied to end
        assert_eq!(palette_color(&gradient, 255), [0xff, 0xff, 0xff, 0x33]);
    }

    #[test]
    fn test_linear_gradient_parse_out_of_order_stops() {
        let gradient = "50:ff0000;10:00ff00".parse::<LinearGradient>().unwrap();
        assert_eq!(palette_color(&gradient, 0), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(palette_color(&gradient, 9), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(palette_color(&gradient, 10), [0x00, 0xff, 0x00, 0xff]);
        assert_eq!(palette_color(&gradient, 50), [0x00, 0xff, 0x00, 0xff]);
        assert_eq!(palette_color(&gradient, 255), [0x00, 0xff, 0x00, 0xff]);
    }

    #[test]
    fn test_indexed_png_palette_round_trips_indices_and_colors() {
        let gradient = "1:001122;10:789;100:334455;200:ffffff33"
            .parse::<LinearGradient>()
            .unwrap();

        let pixels: Vec<u8> = (0..=255u8).collect();
        let encoded = encode_indexed_png(&pixels, 256, 1, &gradient);

        assert_eq!(
            &encoded[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );

        let decoder = png::Decoder::new(std::io::Cursor::new(&encoded));
        let mut reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!(info.color_type, png::ColorType::Indexed);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);

        let decoded_palette = info.palette.clone().unwrap().into_owned();
        let decoded_trns = info.trns.clone().unwrap().into_owned();

        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        let indices = &buf[..frame.buffer_size()];

        assert_eq!(indices, pixels.as_slice());

        for (i, &idx) in indices.iter().enumerate() {
            let idx = idx as usize;
            assert_eq!(
                [
                    decoded_palette[idx * 3],
                    decoded_palette[idx * 3 + 1],
                    decoded_palette[idx * 3 + 2],
                    decoded_trns[idx]
                ],
                palette_color(&gradient, i as u8)
            );
        }
    }

    #[test]
    fn test_unfiltered_query_matches_filtered_query_raster() {
        let db = Database::memory().unwrap();
        let config = db.load_config().unwrap();

        let tile = Tile::new(511, 340, 10);
        let source_zoom = config.source_level(tile.z).unwrap();
        let bounds = TileBounds::from(source_zoom, &tile);

        let points: [(u16, u16); 5] = [(10, 10), (900, 400), (1500, 1900), (300, 1700), (60, 25)];
        let mut coords = Vec::with_capacity(points.len() * 4);
        for (x, y) in points {
            coords.extend_from_slice(&x.to_le_bytes());
            coords.extend_from_slice(&y.to_le_bytes());
        }

        {
            let conn = db.connection().unwrap();
            conn.execute(
                "INSERT INTO activities (id, file, title, start_time, properties) \
                 VALUES (1, 'test.gpx', 'test', '2020-06-01T12:00:00Z', jsonb('{}'))",
                [],
            )
            .unwrap();

            conn.execute(
                "INSERT INTO activity_tiles (activity_id, z, x, y, coords) VALUES (1, ?, ?, ?, ?)",
                params![bounds.z, bounds.xmin, bounds.ymin, &coords],
            )
            .unwrap();
        }

        let unfiltered = ActivityFilter::default();
        assert!(unfiltered.is_empty());

        let matches_everything = ActivityFilter::new(
            None,
            time::Date::from_calendar_date(2000, time::Month::January, 1).ok(),
            None,
        );
        assert!(!matches_everything.is_empty());

        let without_join = rasterize_tile(tile, 256, &unfiltered, &db, &config)
            .unwrap()
            .expect("expected activity data");
        let with_join = rasterize_tile(tile, 256, &matches_everything, &db, &config)
            .unwrap()
            .expect("expected activity data");

        assert!(without_join.pixels.iter().any(|&p| p > 0));
        assert_eq!(without_join.pixels, with_join.pixels);
    }

    #[test]
    fn test_non_power_of_two_widths() {
        let db = Database::memory().unwrap();
        let config = db.load_config().unwrap();

        // A horizontal line across the middle of a tile at the finest stored level.
        let tile = Tile::new(32740, 21790, 16);
        let mut coords = Vec::new();
        for (x, y) in [(0u16, 1024u16), (2047, 1024)] {
            coords.extend_from_slice(&x.to_le_bytes());
            coords.extend_from_slice(&y.to_le_bytes());
        }
        {
            let conn = db.connection().unwrap();
            conn.execute(
                "INSERT INTO activities (id, file, title, start_time, properties) \
                 VALUES (1, 'line.gpx', 'line', '2020-06-01T12:00:00Z', jsonb('{}'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO activity_tiles (activity_id, z, x, y, coords) VALUES (1, ?, ?, ?, ?)",
                params![tile.z, tile.x, tile.y, &coords],
            )
            .unwrap();
        }

        let filter = ActivityFilter::default();
        for width in [256u32, 300, 600] {
            let raster = rasterize_tile(tile, width, &filter, &db, &config)
                .unwrap()
                .expect("expected activity data");
            let lit: Vec<_> = raster.enumerate_pixels().filter(|(_, _, p)| *p > 0).collect();
            assert_eq!(lit.len(), width as usize, "width {width}");
            assert!(lit.iter().all(|&(_, y, _)| y == width as usize / 2), "width {width}");
        }

        // Rendering exactly that tile's bounds at 300 px (zoom 16.23) gives the same pixels.
        let b = tile.xy_bounds();
        let viewport = WebMercatorViewport {
            sw: crate::tile::WebMercator(geo_types::Point::new(b.left, b.bot)),
            ne: crate::tile::WebMercator(geo_types::Point::new(b.right, b.top)),
        };
        let png = render_view(viewport, &PINKISH, 300, 300, &filter, &db, &config).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(&png)).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        let raster = rasterize_tile(tile, 300, &filter, &db, &config).unwrap().unwrap();
        assert_eq!(&buf[..frame.buffer_size()], raster.pixels.as_slice());
    }
}
