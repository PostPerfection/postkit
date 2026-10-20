use std::collections::HashMap;
use std::f64::consts::{FRAC_PI_2, PI};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Cartesian {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Loudspeaker {
    pub label: &'static str,
    // ITU-R BS.2127 11.2 allocentric position
    pub position: Cartesian,
    // ITU-R BS.2051 nominal azimuth, every loudspeaker here sits at elevation 0
    pub azimuth: f64,
    pub is_lfe: bool,
}

impl Loudspeaker {
    fn nominal_position(&self) -> Cartesian {
        let azimuth = self.azimuth.to_radians();
        Cartesian {
            x: -azimuth.sin(),
            y: azimuth.cos(),
            z: 0.0,
        }
    }
}

const fn loudspeaker(label: &'static str, x: f64, y: f64, azimuth: f64) -> Loudspeaker {
    Loudspeaker {
        label,
        position: Cartesian { x, y, z: 0.0 },
        azimuth,
        is_lfe: false,
    }
}

const LFE1: Loudspeaker = Loudspeaker {
    label: "LFE1",
    position: Cartesian {
        x: -1.0,
        y: 1.0,
        z: -1.0,
    },
    azimuth: 45.0,
    is_lfe: true,
};

// BS.2127 Tables 5, 6 and 13, in the output channel order
pub(super) const STEREO_LOUDSPEAKERS: &[Loudspeaker] = &[
    loudspeaker("M+030", -1.0, 1.0, 30.0),
    loudspeaker("M-030", 1.0, 1.0, -30.0),
];
pub(super) const FIVE_POINT_ONE_LOUDSPEAKERS: &[Loudspeaker] = &[
    loudspeaker("M+030", -1.0, 1.0, 30.0),
    loudspeaker("M-030", 1.0, 1.0, -30.0),
    loudspeaker("M+000", 0.0, 1.0, 0.0),
    LFE1,
    loudspeaker("M+110", -1.0, -1.0, 110.0),
    loudspeaker("M-110", 1.0, -1.0, -110.0),
];
pub(super) const SEVEN_POINT_ONE_LOUDSPEAKERS: &[Loudspeaker] = &[
    loudspeaker("M+030", -1.0, 1.0, 30.0),
    loudspeaker("M-030", 1.0, 1.0, -30.0),
    loudspeaker("M+000", 0.0, 1.0, 0.0),
    LFE1,
    loudspeaker("M+090", -1.0, 0.0, 90.0),
    loudspeaker("M-090", 1.0, 0.0, -90.0),
    loudspeaker("M+135", -1.0, -1.0, 135.0),
    loudspeaker("M-135", 1.0, -1.0, -135.0),
];

// ST 2098-2 Table 24 as boxes (min X, max X, min Y, max Y, min Z, max Z) on nominal positions, walls split at the BS.2127 10.1 room corners of azimuth 30 and 110
const ZONE_EDGE: f64 = 0.01;
const COSINE_30_DEGREES: f64 = 0.866_025_403_784_438_6;
const COSINE_110_DEGREES: f64 = -0.342_020_143_325_668_7;
pub(super) const ZONE_BOXES: [[f64; 6]; 9] = [
    [-1.0, -ZONE_EDGE, COSINE_30_DEGREES, 1.0, -1.0, 0.0],
    [0.0, 0.0, COSINE_30_DEGREES, 1.0, -1.0, 0.0],
    [ZONE_EDGE, 1.0, COSINE_30_DEGREES, 1.0, -1.0, 0.0],
    [
        -1.0,
        -ZONE_EDGE,
        COSINE_110_DEGREES + ZONE_EDGE,
        COSINE_30_DEGREES - ZONE_EDGE,
        -1.0,
        0.0,
    ],
    [
        ZONE_EDGE,
        1.0,
        COSINE_110_DEGREES + ZONE_EDGE,
        COSINE_30_DEGREES - ZONE_EDGE,
        -1.0,
        0.0,
    ],
    [-1.0, -ZONE_EDGE, -1.0, COSINE_110_DEGREES, -1.0, 0.0],
    [ZONE_EDGE, 1.0, -1.0, COSINE_110_DEGREES, -1.0, 0.0],
    [-1.0, -ZONE_EDGE, -1.0, 1.0, ZONE_EDGE, 1.0],
    [ZONE_EDGE, 1.0, -1.0, 1.0, ZONE_EDGE, 1.0],
];

pub(super) fn in_excluded_zone(loudspeaker: &Loudspeaker, excluded_zones: &[bool; 9]) -> bool {
    ZONE_BOXES
        .iter()
        .zip(excluded_zones)
        .any(|(zone, excluded)| *excluded && zone_contains(zone, loudspeaker.nominal_position()))
}

// BS.2127 7.3.12.1, on the nominal position at radius 1
fn zone_contains(zone: &[f64; 6], position: Cartesian) -> bool {
    const MARGIN: f64 = 1e-6;
    let inside = |value: f64, minimum: f64, maximum: f64| {
        minimum - MARGIN < value && value < maximum + MARGIN
    };
    inside(position.x, zone[0], zone[1])
        && inside(position.y, zone[2], zone[3])
        && inside(position.z, zone[4], zone[5])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ObjectParameters {
    pub position: Cartesian,
    // ADM width, height and depth, which a one dimensional IAB spread sets alike
    pub extent: f64,
    pub excluded_zones: [bool; 9],
    pub channel_lock: bool,
    pub diffuse: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct DirectDiffuseGains {
    pub direct: Vec<f64>,
    pub diffuse: Vec<f64>,
}

// the loudspeakers left after zone exclusion, with what the extent panner needs of them
struct ReducedLayout {
    // indexes into the output channels
    channels: Vec<usize>,
    positions: Vec<Cartesian>,
    azimuths: Vec<f64>,
    extent_tables: ExtentTables,
}

pub(super) struct ObjectPanner {
    loudspeakers: &'static [Loudspeaker],
    // keyed by the excluded mask over the output channels
    reduced_layouts: HashMap<u32, ReducedLayout>,
}

impl ObjectPanner {
    pub fn new(loudspeakers: &'static [Loudspeaker]) -> Self {
        ObjectPanner {
            loudspeakers,
            reduced_layouts: HashMap::new(),
        }
    }

    // BS.2127 7.3.1 for bf.cartesian, without screen handling, divergence or the 7.3.2 clip, as valid codes stay in the room
    pub fn gains(&mut self, parameters: &ObjectParameters) -> DirectDiffuseGains {
        let mut position = parameters.position;
        let excluded = self.excluded_mask(&parameters.excluded_zones);
        let loudspeakers = self.loudspeakers;
        let reduced = self
            .reduced_layouts
            .entry(excluded)
            .or_insert_with(|| ReducedLayout::new(loudspeakers, excluded));
        if parameters.channel_lock {
            position = reduced.channel_lock(position);
        }
        let reduced_gains = if parameters.extent == 0.0 {
            point_gains(&reduced.positions, position)
        } else {
            reduced.extent_gains(position, parameters.extent)
        };
        let mut gains = vec![0.0; self.loudspeakers.len()];
        for (channel, gain) in reduced.channels.iter().zip(reduced_gains) {
            gains[*channel] = gain;
        }
        let silent = vec![0.0; gains.len()];
        if parameters.diffuse {
            DirectDiffuseGains {
                direct: silent,
                diffuse: gains,
            }
        } else {
            DirectDiffuseGains {
                direct: gains,
                diffuse: silent,
            }
        }
    }

    // BS.2127 7.3.5
    fn excluded_mask(&self, excluded_zones: &[bool; 9]) -> u32 {
        let panned = |channel: usize| !self.loudspeakers[channel].is_lfe;
        let mut excluded = 0u32;
        for (channel, loudspeaker) in self.loudspeakers.iter().enumerate() {
            if panned(channel) && in_excluded_zone(loudspeaker, excluded_zones) {
                excluded |= 1 << channel;
            }
        }
        // follows ear's core.allocentric.get_excluded
        let mut row_removed = 0u32;
        for (channel, loudspeaker) in self.loudspeakers.iter().enumerate() {
            let position = loudspeaker.position;
            let on_side_wall = position.x.abs() == 1.0 && position.y.abs() != 1.0;
            if excluded & (1 << channel) == 0 || !on_side_wall {
                continue;
            }
            for (other, other_loudspeaker) in self.loudspeakers.iter().enumerate() {
                let other_position = other_loudspeaker.position;
                if panned(other) && other_position.y == position.y && other_position.z == position.z
                {
                    row_removed |= 1 << other;
                }
            }
        }
        excluded |= row_removed;
        let all_panned = (0..self.loudspeakers.len())
            .filter(|channel| panned(*channel))
            .fold(0u32, |mask, channel| mask | 1 << channel);
        if excluded == all_panned {
            return 0;
        }
        excluded
    }
}

impl ReducedLayout {
    fn new(loudspeakers: &[Loudspeaker], excluded: u32) -> Self {
        let channels: Vec<usize> = (0..loudspeakers.len())
            .filter(|channel| !loudspeakers[*channel].is_lfe && excluded & (1 << channel) == 0)
            .collect();
        let positions: Vec<Cartesian> = channels
            .iter()
            .map(|channel| loudspeakers[*channel].position)
            .collect();
        let azimuths = channels
            .iter()
            .map(|channel| loudspeakers[*channel].azimuth)
            .collect();
        let extent_tables = ExtentTables::new(&positions);
        ReducedLayout {
            channels,
            positions,
            azimuths,
            extent_tables,
        }
    }

    // BS.2127 7.3.6 in the allocentric configuration, without maxDistance
    fn channel_lock(&self, position: Cartesian) -> Cartesian {
        const TOLERANCE: f64 = 1e-10;
        let weighted_distance = |loudspeaker: Cartesian| {
            (((position.x - loudspeaker.x).powi(2)) / 16.0
                + 4.0 * (position.y - loudspeaker.y).powi(2)
                + 32.0 * (position.z - loudspeaker.z).powi(2))
            .sqrt()
        };
        let distances: Vec<f64> = self
            .positions
            .iter()
            .map(|loudspeaker| weighted_distance(*loudspeaker))
            .collect();
        let closest = distances.iter().copied().fold(f64::INFINITY, f64::min);
        // ties go to the lowest (|elevation|, elevation, |azimuth|, azimuth), elevation is 0 here
        let chosen = (0..self.positions.len())
            .filter(|index| distances[*index] - closest < TOLERANCE)
            .min_by(|first, second| {
                let key = |index: usize| (self.azimuths[index].abs(), self.azimuths[index]);
                key(*first)
                    .partial_cmp(&key(*second))
                    .expect("azimuths are finite")
            })
            .expect("a reduced layout keeps at least one loudspeaker");
        self.positions[chosen]
    }

    // follows ear's core.objectbased.allo_extent.get_gains
    fn extent_gains(&self, position: Cartesian, extent: f64) -> Vec<f64> {
        let tables = &self.extent_tables;
        let point = point_gains(&self.positions, position);
        let centre_z = position.z.max(0.0);
        let minimum_size = |points: usize| 2.0 / (points as f64 - 1.0);
        let scaled = prescaled_extent(extent);
        let size_x = scaled.max(minimum_size(HORIZONTAL_GRID_POINTS));
        let size_y = scaled.max(minimum_size(HORIZONTAL_GRID_POINTS));
        let size_z = scaled.max(minimum_size(VERTICAL_GRID_POINTS));
        let effective = if tables.single_row {
            size_x
        } else {
            0.75 * size_x.max(size_y) + 0.25 * size_x.min(size_y)
        };
        let exponent = if effective <= 0.5 {
            6.0
        } else {
            6.0 - 4.0 * (effective - 0.5) / (MAXIMUM_EXTENT - 0.5)
        };
        // (gain * weight)^p, with each weight raised once and zero gains skipped
        let raised_weights = |grid: &mut dyn Iterator<Item = f64>, weight: &dyn Fn(f64) -> f64| {
            grid.map(|source| weight(source).powf(exponent))
                .collect::<Vec<f64>>()
        };
        let x_weights = raised_weights(&mut horizontal_grid(), &|source| {
            horizontal_weight(source, position.x, size_x)
        });
        let y_weights = raised_weights(&mut horizontal_grid(), &|source| {
            horizontal_weight(source, position.y, size_y)
        });
        let z_weights = raised_weights(&mut vertical_grid(), &|source| {
            vertical_weight(source, centre_z, size_z)
        });
        let term = |gain: f64, raised_weight: f64| {
            if gain == 0.0 {
                0.0
            } else {
                gain.powf(exponent) * raised_weight
            }
        };
        let sum = |gains: &[f64], raised_weights: &[f64]| {
            let sum: f64 = gains
                .iter()
                .zip(raised_weights)
                .map(|(gain, raised_weight)| term(*gain, *raised_weight))
                .sum();
            if sum < UNDERFLOW_LIMIT { 0.0 } else { sum }
        };
        let speaker_count = self.positions.len();
        let mut inside = vec![0.0; speaker_count];
        let mut bound = vec![0.0; speaker_count];
        for speaker in 0..speaker_count {
            let (x_gains, y_gains, z_gains) =
                (&tables.x[speaker], &tables.y[speaker], &tables.z[speaker]);
            let (x_sum, y_sum, z_sum) = (
                sum(x_gains, &x_weights),
                sum(y_gains, &y_weights),
                sum(z_gains, &z_weights),
            );
            let last_x = HORIZONTAL_GRID_POINTS - 1;
            let last_z = VERTICAL_GRID_POINTS - 1;
            let left = term(x_gains[0], x_weights[0]);
            let right = term(x_gains[last_x], x_weights[last_x]);
            let back = term(y_gains[0], y_weights[0]);
            let front = term(y_gains[last_x], y_weights[last_x]);
            let floor = term(z_gains[0], z_weights[0]);
            let ceiling = term(z_gains[last_z], z_weights[last_z]);
            inside[speaker] = x_sum * y_sum * z_sum;
            bound[speaker] = (left + right) * y_sum * z_sum
                + x_sum * (front + back) * z_sum
                + x_sum * y_sum * (floor + ceiling);
        }
        normalise(&mut inside);
        let distance_x = (position.x + 1.0).min(1.0 - position.x);
        let distance_y = (position.y + 1.0).min(1.0 - position.y);
        let distance_z = (centre_z + 1.0).min(1.0 - centre_z);
        let distance_to_boundary = match tables.boundary_axes {
            BoundaryAxes::X => distance_x,
            BoundaryAxes::XAndY => distance_x.min(distance_y),
            BoundaryAxes::All => distance_x.min(distance_y).min(distance_z),
        };
        let fade = |size: f64| fade_out(distance_to_boundary, size);
        let inside_share = match tables.boundary_axes {
            BoundaryAxes::X => fade(size_x).powi(3),
            BoundaryAxes::XAndY => (fade(size_x) * fade(size_y)).powf(1.5),
            BoundaryAxes::All => fade(size_x) * fade(size_y) * fade(size_z),
        };
        let mut extent_gains: Vec<f64> = bound
            .iter()
            .zip(&inside)
            .map(|(bound, inside)| (bound + inside_share * inside).powf(1.0 / exponent))
            .collect();
        normalise(&mut extent_gains);
        let (point_share, extent_share) = if effective < FADE_EXTENT {
            let angle = effective / FADE_EXTENT * FRAC_PI_2;
            (angle.cos(), angle.sin())
        } else {
            (0.0, 1.0)
        };
        let mut total: Vec<f64> = point
            .iter()
            .zip(&extent_gains)
            .map(|(point, extent)| point_share * point + extent_share * extent)
            .collect();
        normalise(&mut total);
        total
    }
}

const HORIZONTAL_GRID_POINTS: usize = 40;
// no layout here has a loudspeaker below the main plane (BS.2127 7.3.11.2 note 1)
const VERTICAL_GRID_POINTS: usize = 20;
const MAXIMUM_EXTENT: f64 = 2.8;
const FADE_EXTENT: f64 = 0.2;
const NORMALISE_TOLERANCE: f64 = 1e-5;
// 10^-6.5
const UNDERFLOW_LIMIT: f64 = 3.162_277_660_168_379_5e-7;

fn horizontal_grid() -> impl Iterator<Item = f64> {
    let last = HORIZONTAL_GRID_POINTS as f64 - 1.0;
    (0..HORIZONTAL_GRID_POINTS).map(move |index| -1.0 + 2.0 * index as f64 / last)
}

fn vertical_grid() -> impl Iterator<Item = f64> {
    let last = VERTICAL_GRID_POINTS as f64 - 1.0;
    (0..VERTICAL_GRID_POINTS).map(move |index| index as f64 / last)
}

// BS.2127 Fig. 14
fn prescaled_extent(extent: f64) -> f64 {
    const POINTS: [(f64, f64); 5] = [(0.0, 0.0), (0.2, 0.3), (0.5, 1.0), (0.75, 1.8), (1.0, 2.8)];
    let extent = extent.clamp(0.0, 1.0);
    for pair in POINTS.windows(2) {
        let ((start_extent, start_value), (end_extent, end_value)) = (pair[0], pair[1]);
        if extent <= end_extent {
            return start_value
                + (extent - start_extent) * (end_value - start_value)
                    / (end_extent - start_extent);
        }
    }
    MAXIMUM_EXTENT
}

fn horizontal_weight(source: f64, centre: f64, size: f64) -> f64 {
    let scaled = 1.5 * (source - centre) / (2.0 * size);
    10f64.powf(-scaled.powi(4).min(6.5))
}

fn vertical_weight(source: f64, centre: f64, size: f64) -> f64 {
    let scaled = 1.5 * (source - centre) / size;
    10f64.powf(-scaled.powi(4).min(6.5)) * (source * 3.0 * PI / 7.0).cos()
}

// BS.2127 7.3.11.5 h(c, s), with the 2s bound the reference code uses
fn fade_out(distance_to_boundary: f64, size: f64) -> f64 {
    if distance_to_boundary >= 2.0 * size && distance_to_boundary >= 0.4 {
        ((2.0 * size).max(0.4).powi(3) / (0.16 * 2.0 * size)).cbrt()
    } else {
        (distance_to_boundary / 2.0 * (distance_to_boundary / 0.4).powi(2)).cbrt()
    }
}

fn normalise(gains: &mut [f64]) {
    let norm = gains.iter().map(|gain| gain * gain).sum::<f64>().sqrt();
    for gain in gains.iter_mut() {
        *gain = if norm > NORMALISE_TOLERANCE {
            *gain / norm
        } else {
            0.0
        };
    }
}

// ear's _dim counts the axes the loudspeakers vary along, and 0 or 3 of them take every axis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryAxes {
    X,
    XAndY,
    All,
}

// per loudspeaker and axis, the point gain of a virtual source on each grid point (BS.2127 7.3.11.3)
struct ExtentTables {
    single_row: bool,
    boundary_axes: BoundaryAxes,
    x: Vec<[f64; HORIZONTAL_GRID_POINTS]>,
    y: Vec<[f64; HORIZONTAL_GRID_POINTS]>,
    z: Vec<[f64; VERTICAL_GRID_POINTS]>,
}

impl ExtentTables {
    fn new(positions: &[Cartesian]) -> Self {
        let varies = |coordinate: fn(&Cartesian) -> f64| {
            positions
                .iter()
                .any(|position| coordinate(position) != coordinate(&positions[0]))
        };
        let (varies_x, varies_y, varies_z) = (
            varies(|position| position.x),
            varies(|position| position.y),
            varies(|position| position.z),
        );
        let single_row = !varies_y && !varies_z;
        let boundary_axes = match [varies_x, varies_y, varies_z]
            .iter()
            .filter(|varying| **varying)
            .count()
        {
            1 => BoundaryAxes::X,
            2 => BoundaryAxes::XAndY,
            _ => BoundaryAxes::All,
        };
        let along = |speaker: usize, source: Cartesian| axis_gains(positions, speaker, source);
        let origin = Cartesian {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let x = (0..positions.len())
            .map(|speaker| {
                let mut gains = [0.0; HORIZONTAL_GRID_POINTS];
                for (gain, x) in gains.iter_mut().zip(horizontal_grid()) {
                    *gain = along(speaker, Cartesian { x, ..origin }).x;
                }
                gains
            })
            .collect();
        let y = (0..positions.len())
            .map(|speaker| {
                let mut gains = [0.0; HORIZONTAL_GRID_POINTS];
                for (gain, y) in gains.iter_mut().zip(horizontal_grid()) {
                    *gain = along(speaker, Cartesian { y, ..origin }).y;
                }
                gains
            })
            .collect();
        let z = (0..positions.len())
            .map(|speaker| {
                let mut gains = [0.0; VERTICAL_GRID_POINTS];
                for (gain, z) in gains.iter_mut().zip(vertical_grid()) {
                    *gain = along(speaker, Cartesian { z, ..origin }).z;
                }
                gains
            })
            .collect();
        ExtentTables {
            single_row,
            boundary_axes,
            x,
            y,
            z,
        }
    }
}

struct AxisGains {
    x: f64,
    y: f64,
    z: f64,
}

// BS.2127 7.3.10 for one loudspeaker, kept per axis
fn axis_gains(positions: &[Cartesian], speaker: usize, object: Cartesian) -> AxisGains {
    const SAME_PLANE: f64 = 0.001;
    let relative = |position: &Cartesian| Cartesian {
        x: position.x - object.x,
        y: position.y - object.y,
        z: position.z - object.z,
    };
    let this = relative(&positions[speaker]);
    let all = positions.iter().map(relative);
    let plane = all
        .clone()
        .filter(|other| (other.z - this.z).abs() < SAME_PLANE);
    let row = plane
        .clone()
        .filter(|other| (other.y - this.y).abs() < SAME_PLANE);
    AxisGains {
        z: axis_gain(this.z, all.map(|other| other.z)),
        y: axis_gain(this.y, plane.map(|other| other.y)),
        x: axis_gain(this.x, row.map(|other| other.x)),
    }
}

fn axis_gain(this: f64, others: impl Iterator<Item = f64>) -> f64 {
    let other = if this >= 0.0 {
        others.filter(|other| *other < this).reduce(f64::max)
    } else {
        others.filter(|other| *other > this).reduce(f64::min)
    };
    match other {
        None => 1.0,
        Some(other) if sign(other) == sign(this) => 0.0,
        Some(other) => (this / (other - this) * FRAC_PI_2).cos(),
    }
}

fn sign(value: f64) -> i8 {
    if value > 0.0 {
        1
    } else if value < 0.0 {
        -1
    } else {
        0
    }
}

pub(super) fn point_gains(positions: &[Cartesian], object: Cartesian) -> Vec<f64> {
    (0..positions.len())
        .map(|speaker| {
            let gains = axis_gains(positions, speaker, object);
            gains.x * gains.y * gains.z
        })
        .collect()
}
