//! A query that names two streets with a word between them ("5th Ave S &
//! 9th St S, Naples") asks for where they cross. The Pack stores no
//! junctions, so the crossing is found at query time from the lines of the
//! two street records the existing search finds, each in the stated area.
//! It only adds an answer: a query it cannot cross is answered as before.

use super::{
    AddressGeocodeOptions, BoundingBox, Depth, TextSearchHit, TextSearchOptions, TextSearcher,
    effective_limit,
};
use crate::{
    api::{PartMatch, QueryMatch},
    error::Result,
    labels,
    pack::{Point, PointPrecision},
    record::Layer,
    search::road,
    util::geo::{METRES_PER_DEGREE, haversine_m, line_parts},
};

/// Words that join two street names.
const CONNECTORS: [&str; 4] = ["&", "and", "at", "@"];

/// Records of each street the search is asked for: a street of one name in
/// one town may be several records, and the right one is not always first.
const SIDE_HITS: usize = 5;

/// Most readings of one query tried; each costs two searches.
const MAX_SPLITS: usize = 6;

/// Most words after the second street that may be its stated area, for a
/// query with no comma ("... Naples FL").
const MAX_AREA_WORDS: usize = 3;

/// How far apart two lines may lie and still cross. Two streets that meet
/// are mapped to end at one another, but a way may stop at the other's
/// median, and a street's pieces join across gaps of up to 50 m; 25 m
/// covers both and stays under the width of a block, so streets that only
/// run alongside do not cross.
const NEAR_MISS_M: f64 = 25.0;

/// Below this, two lines touch: stored coordinates are 1e-7 degrees, about
/// a centimetre, so a shared vertex differs by less.
const TOUCH_M: f64 = 0.05;

/// A query read as two streets and, perhaps, their stated area.
#[derive(Debug, PartialEq, Eq)]
struct Split {
    first: String,
    second: String,
    area: Option<String>,
}

/// The ways `written` reads as two streets, one for each connector word
/// that has a word on each side, left to right. After a comma the rest is
/// the stated area; without one, the last words of the second street may
/// be it, fewest first.
fn splits(written: &str) -> Vec<Split> {
    let (head, area) = match written.split_once(',') {
        Some((head, tail)) => (head, Some(tail.trim()).filter(|tail| !tail.is_empty())),
        None => (written, None),
    };
    let words = head.split_whitespace().collect::<Vec<_>>();
    let mut found = Vec::new();
    for (at, word) in words.iter().enumerate().skip(1) {
        if at + 1 >= words.len() || !CONNECTORS.iter().any(|c| word.eq_ignore_ascii_case(c)) {
            continue;
        }
        let first = words[..at].join(" ");
        let later = &words[at + 1..];
        let peeled = if area.is_some() {
            0
        } else {
            MAX_AREA_WORDS.min(later.len() - 1)
        };
        for peel in 0..=peeled {
            let (second, rest) = later.split_at(later.len() - peel);
            found.push(Split {
                first: first.clone(),
                second: second.join(" "),
                area: area
                    .map(str::to_string)
                    .or_else(|| (!rest.is_empty()).then(|| rest.join(" "))),
            });
        }
    }
    found
}

/// A street the search found for one side of the query.
struct Side {
    hit: TextSearchHit,
    parts: Vec<Vec<[f64; 2]>>,
}

/// Where two lines cross.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Meet {
    at: [f64; 2],
    /// 0 for lines that touch, else how far apart they pass.
    gap_m: f64,
}

fn street_layer(layer: Option<&str>) -> bool {
    layer.is_none_or(|layer| layer == Layer::Street.as_str())
}

impl TextSearcher {
    /// The crossing of the two streets `options.query` names, when it
    /// names two that cross in its stated area.
    pub(super) fn crossing_in_text(
        &self,
        options: &TextSearchOptions,
    ) -> Result<Option<TextSearchHit>> {
        if self.places
            || !street_layer(options.layer.as_deref())
            || effective_limit(options.limit) == 0
        {
            return Ok(None);
        }
        self.first_crossing(
            &splits(&options.query),
            options.focus,
            options.bbox,
            |street, area| {
                let query =
                    area.map_or_else(|| street.to_string(), |area| format!("{street}, {area}"));
                self.search_at(
                    &TextSearchOptions {
                        query,
                        limit: SIDE_HITS,
                        layer: Some(Layer::Street.as_str().to_string()),
                        ..options.clone()
                    },
                    Depth::Query,
                )
            },
        )
    }

    /// [`Self::crossing_in_text`] for an address whose area is in fields,
    /// so no area follows the streets.
    pub(super) fn crossing_in_fields(
        &self,
        options: &AddressGeocodeOptions,
        focus: Option<[f64; 2]>,
        bbox: Option<BoundingBox>,
    ) -> Result<Option<TextSearchHit>> {
        if !street_layer(options.layer.as_deref()) || effective_limit(options.limit) == 0 {
            return Ok(None);
        }
        let splits = splits(&options.address)
            .into_iter()
            .filter(|split| split.area.is_none())
            .collect::<Vec<_>>();
        self.first_crossing(&splits, focus, bbox, |street, _| {
            let (_, hits) = self.geocode_address_candidates(
                &AddressGeocodeOptions {
                    address: street.to_string(),
                    limit: SIDE_HITS,
                    layer: Some(Layer::Street.as_str().to_string()),
                    ..options.clone()
                },
                focus,
                bbox,
                None,
            )?;
            Ok(hits)
        })
    }

    /// The crossing of the first reading in `splits` whose two sides are
    /// streets that cross. `find` searches streets for one side's words
    /// and the area they are in.
    fn first_crossing(
        &self,
        splits: &[Split],
        focus: Option<[f64; 2]>,
        bbox: Option<BoundingBox>,
        find: impl Fn(&str, Option<&str>) -> Result<Vec<TextSearchHit>>,
    ) -> Result<Option<TextSearchHit>> {
        for split in splits.iter().take(MAX_SPLITS) {
            let area = split.area.as_deref();
            let first = self.streets_named(find(&split.first, area)?, &split.first)?;
            if first.is_empty() {
                continue;
            }
            let second = self.streets_named(find(&split.second, area)?, &split.second)?;
            // Nearest in rank first: the second street of the best first
            // street comes before the best second street of the next.
            let mut pairs = (0..first.len())
                .flat_map(|at| (0..second.len()).map(move |with| (at, with)))
                .collect::<Vec<_>>();
            pairs.sort_by_key(|&(at, with)| (at + with, at));
            for (at, with) in pairs {
                let (one, other) = (&first[at], &second[with]);
                if one.hit.record_id == other.hit.record_id
                    || road::same_road(&one.hit.record.name, &other.hit.record.name)
                {
                    continue;
                }
                let inside =
                    |point: [f64; 2]| bbox.is_none_or(|bbox| bbox.contains(point[0], point[1]));
                if let Some(meet) = meeting(&one.parts, &other.parts, focus, inside) {
                    return Ok(Some(crossing_hit(one, other, meet.at)));
                }
            }
        }
        Ok(None)
    }

    /// The street records among `hits` that `words` name, with their lines.
    /// The search ranks a street that only shares a word with the query,
    /// which would cross anything near it.
    fn streets_named(&self, hits: Vec<TextSearchHit>, words: &str) -> Result<Vec<Side>> {
        let mut sides = Vec::new();
        for hit in hits {
            if hit.fallback || hit.record.layer != Layer::Street.as_str() {
                continue;
            }
            let Some(street) = self.pack.street(hit.record_id)? else {
                continue;
            };
            let named = std::iter::once(&street.name)
                .chain(&street.alt_names)
                .any(|name| road::same_road(words, name));
            let Some(lines) = line_parts(&street.geometry) else {
                continue;
            };
            if named {
                let parts = lines
                    .into_iter()
                    .map(|line| {
                        line.iter()
                            .filter_map(|position| match position.as_slice() {
                                [lon, lat, ..] => Some([*lon, *lat]),
                                _ => None,
                            })
                            .collect()
                    })
                    .collect();
                sides.push(Side { hit, parts });
            }
        }
        Ok(sides)
    }
}

/// `one` as the answer for the crossing of its street and `other`'s at
/// `at`. A point on both streets is a point on a street, so it keeps the
/// street layer and accuracy.
fn crossing_hit(one: &Side, other: &Side, at: [f64; 2]) -> TextSearchHit {
    let mut hit = one.hit.clone();
    let record = &mut hit.record;
    let name = format!("{} & {}", record.name, other.hit.record.name);
    record.label = match one.hit.record.label.strip_prefix(&one.hit.record.name) {
        Some(area) => format!("{name}{area}"),
        None => name.clone(),
    };
    record.name = name;
    // Ordered, so the same two streets give one id whichever is named first.
    record.id = labels::crossing_id(&one.hit.record.id, &other.hit.record.id);
    record.address.street = None;
    record.road_class = None;
    record.bbox = None;
    record.point = Some(Point {
        lon: at[0],
        lat: at[1],
        precision: PointPrecision::RepresentativePoint,
    });
    hit.confidence = None;
    hit.highlights.clear();
    hit.matched = worse(one.hit.matched, other.hit.matched);
    hit
}

/// Each part of `left` and `right`, as the weaker of the two judged it.
fn worse(left: QueryMatch, right: QueryMatch) -> QueryMatch {
    let part = |left: Option<PartMatch>, right: Option<PartMatch>| match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    QueryMatch {
        housenumber: part(left.housenumber, right.housenumber),
        street: part(left.street, right.street),
        unit: part(left.unit, right.unit),
        locality: part(left.locality, right.locality),
        region: part(left.region, right.region),
        postcode: part(left.postcode, right.postcode),
        country: part(left.country, right.country),
    }
}

/// Where the lines `left` and `right` cross: lines that touch, else the
/// closest approach within [`NEAR_MISS_M`]. Of several, the one that
/// touches, then the closest approach, then the nearest `focus`, then the
/// lowest longitude and latitude.
fn meeting(
    left: &[Vec<[f64; 2]>],
    right: &[Vec<[f64; 2]>],
    focus: Option<[f64; 2]>,
    inside: impl Fn([f64; 2]) -> bool,
) -> Option<Meet> {
    let origin = *left.iter().flatten().next()?;
    let east = origin[1].to_radians().cos().max(1e-9) * METRES_PER_DEGREE;
    let plane = |point: [f64; 2]| {
        [
            (point[0] - origin[0]) * east,
            (point[1] - origin[1]) * METRES_PER_DEGREE,
        ]
    };
    let lines = |parts: &[Vec<[f64; 2]>]| {
        parts
            .iter()
            .flat_map(|part| part.windows(2).map(|pair| [plane(pair[0]), plane(pair[1])]))
            .collect::<Vec<_>>()
    };
    let (left, right) = (lines(left), lines(right));
    let mut best: Option<Meet> = None;
    for one in &left {
        for other in &right {
            let Some((gap_m, [x, y])) = nearest_points(*one, *other) else {
                continue;
            };
            let at = [origin[0] + x / east, origin[1] + y / METRES_PER_DEGREE];
            if !inside(at) {
                continue;
            }
            let meet = Meet {
                at,
                gap_m: if gap_m < TOUCH_M { 0.0 } else { gap_m },
            };
            if best.is_none_or(|best| closer(meet, best, focus).is_lt()) {
                best = Some(meet);
            }
        }
    }
    best
}

fn closer(left: Meet, right: Meet, focus: Option<[f64; 2]>) -> std::cmp::Ordering {
    let from_focus = |meet: Meet| {
        focus.map_or(0.0, |focus| {
            haversine_m(focus[0], focus[1], meet.at[0], meet.at[1])
        })
    };
    let cell = |meet: Meet| {
        [
            (meet.at[0] * 1e7).round() as i64,
            (meet.at[1] * 1e7).round() as i64,
        ]
    };
    left.gap_m
        .total_cmp(&right.gap_m)
        .then_with(|| from_focus(left).total_cmp(&from_focus(right)))
        .then_with(|| cell(left).cmp(&cell(right)))
}

/// How far apart the segments `one` and `other` (in metres on a plane)
/// pass, and the point between them: where they cross, or the middle of
/// their closest approach. `None` when they pass farther than
/// [`NEAR_MISS_M`].
fn nearest_points(one: [[f64; 2]; 2], other: [[f64; 2]; 2]) -> Option<(f64, [f64; 2])> {
    let span = |segment: [[f64; 2]; 2], axis: usize| {
        (
            segment[0][axis].min(segment[1][axis]),
            segment[0][axis].max(segment[1][axis]),
        )
    };
    let apart = |axis: usize| {
        let (one, other) = (span(one, axis), span(other, axis));
        one.0 - other.1 > NEAR_MISS_M || other.0 - one.1 > NEAR_MISS_M
    };
    if apart(0) || apart(1) {
        return None;
    }
    let cross = |a: [f64; 2], b: [f64; 2]| a[0] * b[1] - a[1] * b[0];
    let sub = |a: [f64; 2], b: [f64; 2]| [a[0] - b[0], a[1] - b[1]];
    let (r, s) = (sub(one[1], one[0]), sub(other[1], other[0]));
    let denominator = cross(r, s);
    if denominator.abs() > f64::EPSILON {
        let gap = sub(other[0], one[0]);
        let (t, u) = (cross(gap, s) / denominator, cross(gap, r) / denominator);
        // A little past the ends: a shared vertex lands there by rounding.
        if (-1e-9..=1.0 + 1e-9).contains(&t) && (-1e-9..=1.0 + 1e-9).contains(&u) {
            return Some((0.0, [one[0][0] + t * r[0], one[0][1] + t * r[1]]));
        }
    }
    let mut closest: Option<(f64, [f64; 2])> = None;
    for (point, segment) in [
        (one[0], other),
        (one[1], other),
        (other[0], one),
        (other[1], one),
    ] {
        let length = {
            let along = sub(segment[1], segment[0]);
            along[0] * along[0] + along[1] * along[1]
        };
        let t = if length <= f64::EPSILON {
            0.0
        } else {
            let from = sub(point, segment[0]);
            let along = sub(segment[1], segment[0]);
            ((from[0] * along[0] + from[1] * along[1]) / length).clamp(0.0, 1.0)
        };
        let on = [
            segment[0][0] + t * (segment[1][0] - segment[0][0]),
            segment[0][1] + t * (segment[1][1] - segment[0][1]),
        ];
        let distance = ((point[0] - on[0]).powi(2) + (point[1] - on[1]).powi(2)).sqrt();
        if closest.is_none_or(|(best, _)| distance < best) {
            closest = Some((
                distance,
                [(point[0] + on[0]) / 2.0, (point[1] + on[1]) / 2.0],
            ));
        }
    }
    closest.filter(|(distance, _)| *distance <= NEAR_MISS_M)
}

/// Naples and Bonita Springs as a built Pack, for the tests of every entry
/// point that takes a query.
#[cfg(test)]
pub(crate) mod fixture {
    use std::{
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use crate::builder::osm::{
        BuildOsmOptions, build_osm_pack,
        test_pbf::{TestElement, node, relation, way, write_pbf},
    };

    type Tags<'a> = &'a [(&'static str, &'static str)];

    /// Naples and Bonita Springs, each with a "5th Avenue South" and a
    /// "9th Street South" that cross, and streets that meet in a vertex,
    /// stop short of one another, or never meet.
    fn elements() -> Vec<Vec<TestElement>> {
        let (mut nodes, mut ways, mut relations) = (Vec::new(), Vec::new(), Vec::new());
        let mut next = 1;
        let mut area = |id: i64, [lon0, lat0, lon1, lat1]: [f64; 4], tags: Tags| {
            let first = next;
            for (lat, lon) in [(lat0, lon0), (lat0, lon1), (lat1, lon1), (lat1, lon0)] {
                nodes.push(node(next, lat, lon, &[]));
                next += 1;
            }
            ways.push(way(
                id,
                &[first, first + 1, first + 2, first + 3, first],
                &[],
            ));
            let mut tags = tags.to_vec();
            tags.extend([("boundary", "administrative"), ("type", "boundary")]);
            relations.push(relation(id, &[(id, "outer")], &tags));
        };
        area(
            1,
            [-90.0, 25.0, -80.0, 28.0],
            &[
                ("admin_level", "2"),
                ("name", "United States"),
                ("ISO3166-1:alpha2", "US"),
            ],
        );
        area(
            2,
            [-89.0, 25.5, -81.0, 27.5],
            &[
                ("admin_level", "4"),
                ("name", "Florida"),
                ("ISO3166-2", "US-FL"),
            ],
        );
        area(
            3,
            [-81.85, 26.0, -81.70, 26.2],
            &[("admin_level", "8"), ("name", "Naples")],
        );
        area(
            4,
            [-81.95, 26.3, -81.80, 26.4],
            &[("admin_level", "8"), ("name", "Bonita Springs")],
        );
        let mut street = |id: i64, name: &'static str, points: &[(f64, f64)]| {
            let first = 1000 * id;
            let refs = (first..).take(points.len()).collect::<Vec<_>>();
            for (&at, &(lon, lat)) in refs.iter().zip(points) {
                nodes.push(node(at, lat, lon, &[]));
            }
            ways.push(way(
                id,
                &refs,
                &[("highway", "residential"), ("name", name)],
            ));
        };
        street(
            101,
            "5th Avenue South",
            &[(-81.800, 26.140), (-81.790, 26.140)],
        );
        street(
            102,
            "9th Street South",
            &[(-81.795, 26.130), (-81.795, 26.150)],
        );
        street(
            103,
            "Oak Street",
            &[(-81.780, 26.120), (-81.775, 26.120), (-81.770, 26.120)],
        );
        // Pine Street ends where Oak Street has a vertex, so the two share it.
        street(
            104,
            "Pine Street",
            &[(-81.775, 26.110), (-81.775, 26.120), (-81.775, 26.130)],
        );
        street(105, "Long Lane", &[(-81.770, 26.180), (-81.750, 26.180)]);
        // 11 m short of Long Lane.
        street(106, "Gap Road", &[(-81.760, 26.150), (-81.760, 26.1799)]);
        street(107, "Lake Lane", &[(-81.800, 26.185), (-81.780, 26.185)]);
        // 167 m short of Lake Lane.
        street(108, "Far Road", &[(-81.790, 26.150), (-81.790, 26.1835)]);
        street(109, "Marks Road", &[(-81.720, 26.110), (-81.720, 26.120)]);
        street(110, "Spencer Road", &[(-81.710, 26.110), (-81.710, 26.120)]);
        street(111, "Main Street", &[(-81.840, 26.050), (-81.720, 26.050)]);
        // Crosses Main Street at (-81.79, 26.05) and at (-81.77, 26.05).
        street(
            112,
            "Loop Road",
            &[(-81.800, 26.040), (-81.780, 26.060), (-81.760, 26.040)],
        );
        street(
            201,
            "5th Avenue South",
            &[(-81.880, 26.340), (-81.870, 26.340)],
        );
        street(
            202,
            "9th Street South",
            &[(-81.875, 26.330), (-81.875, 26.350)],
        );
        vec![nodes, ways, relations]
    }

    pub(crate) fn pack() -> &'static Path {
        static PACK: OnceLock<PathBuf> = OnceLock::new();
        PACK.get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("open-geocode-crossing-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("dir");
            let input = dir.join("fixture.osm.pbf");
            write_pbf(&input, &elements());
            build_osm_pack(&BuildOsmOptions {
                inputs: vec![input],
                pack: dir.join("pack"),
                ..BuildOsmOptions::default()
            })
            .expect("build");
            dir.join("pack")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use serde_json::{Value, json};

    use crate::{
        api::forward_collection,
        search::{AddressGeocodeOptions, TextSearchHit, TextSearchOptions, TextSearcher},
    };

    fn searcher() -> &'static TextSearcher {
        static SEARCHER: OnceLock<TextSearcher> = OnceLock::new();
        SEARCHER.get_or_init(|| TextSearcher::open(super::fixture::pack()).expect("searcher"))
    }

    fn options(query: &str) -> TextSearchOptions {
        TextSearchOptions {
            query: query.to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        }
    }

    fn ask(query: &str) -> Vec<TextSearchHit> {
        searcher().search(&options(query)).expect("search")
    }

    /// What the query was answered with before crossings existed.
    fn before(options: &TextSearchOptions) -> Vec<TextSearchHit> {
        searcher().search_readings(options).expect("search")
    }

    fn at(hit: &TextSearchHit) -> [f64; 2] {
        let point = hit.record.point.expect("point");
        [point.lon, point.lat]
    }

    /// One quantum of a stored coordinate is 1e-7 degrees; this is ten.
    fn assert_at(hit: &TextSearchHit, expected: [f64; 2]) {
        let found = at(hit);
        assert!(
            (found[0] - expected[0]).abs() < 1e-6 && (found[1] - expected[1]).abs() < 1e-6,
            "{found:?} is not {expected:?}"
        );
    }

    fn ids(hits: &[TextSearchHit]) -> Vec<String> {
        hits.iter().map(|hit| hit.record.id.clone()).collect()
    }

    const NAPLES_CROSSING: [f64; 2] = [-81.795, 26.140];

    #[test]
    fn two_streets_that_cross_between_vertices_answer_with_the_crossing() {
        let hits = ask("5th Avenue South and 9th Street South, Naples, FL");
        assert_at(&hits[0], NAPLES_CROSSING);
        assert_eq!(hits[0].record.layer, "street");
        assert_eq!(hits[0].record.name, "5th Avenue South & 9th Street South");
    }

    #[test]
    fn the_crossing_is_one_answer_among_the_others_the_query_had() {
        let query = "5th Avenue South and 9th Street South, Naples, FL";
        let main = ids(&before(&options(query)));
        let hits = ask(query);
        assert_eq!(ids(&hits[1..]), main[..hits.len() - 1]);
        let one = TextSearchOptions {
            limit: 1,
            ..options(query)
        };
        let hits = searcher().search(&one).expect("search");
        assert_eq!(hits.len(), 1);
        assert_at(&hits[0], NAPLES_CROSSING);
    }

    #[test]
    fn every_connector_and_abbreviation_reads_the_same() {
        for query in [
            "5th Ave S & 9th St S, Naples",
            "5th Avenue South at 9th Street South, Naples, FL",
            "5th Avenue South @ 9th Street South, Naples",
            "5TH AVENUE SOUTH AND 9TH STREET SOUTH, naples, fl",
            "5th Avenue South and 9th Street South Naples FL",
        ] {
            let hits = ask(query);
            assert_at(&hits[0], NAPLES_CROSSING);
            assert_eq!(hits[0].record.layer, "street", "{query}");
        }
    }

    #[test]
    fn the_crossing_has_one_id_whichever_street_is_named_first() {
        let first = ask("5th Avenue South and 9th Street South, Naples");
        let second = ask("9th Street South and 5th Avenue South, Naples");
        assert_eq!(
            first[0].record.id,
            "derived:crossing:osm:way:101:osm:way:102"
        );
        assert_eq!(second[0].record.id, first[0].record.id);
        assert_eq!(second[0].record.name, "9th Street South & 5th Avenue South");
    }

    #[test]
    fn streets_that_share_a_vertex_cross_there() {
        assert_at(
            &ask("Oak Street and Pine Street, Naples")[0],
            [-81.775, 26.120],
        );
    }

    #[test]
    fn a_street_that_stops_a_few_metres_short_still_meets_the_other() {
        assert_at(&ask("Gap Road & Long Lane, Naples")[0], [-81.760, 26.17995]);
    }

    #[test]
    fn the_stated_area_picks_the_crossing_among_towns() {
        assert_at(
            &ask("5th Avenue South and 9th Street South, Bonita Springs")[0],
            [-81.875, 26.340],
        );
    }

    #[test]
    fn of_two_crossings_the_one_nearer_the_focus_answers() {
        let query = "Main Street and Loop Road, Naples";
        assert_at(&ask(query)[0], [-81.79, 26.05]);
        for (focus, expected) in [
            ([-81.76, 26.05], [-81.77, 26.05]),
            ([-81.80, 26.05], [-81.79, 26.05]),
        ] {
            let hits = searcher()
                .search(&TextSearchOptions {
                    focus: Some(focus),
                    ..options(query)
                })
                .expect("search");
            assert_at(&hits[0], expected);
        }
    }

    #[test]
    fn a_query_that_is_no_crossing_is_answered_as_before() {
        for query in [
            "Bed and Breakfast, Naples",
            "Marks and Spencer, Naples",
            "Johnson & Johnson",
            "Far Road and Lake Lane, Naples",
            "Marks Road and Spencer Road, Naples",
            "Pine Street and Pine Street, Naples",
            "5th Avenue South and Nowhere Street, Naples",
            "Nowhere Street at 9th Street South, Naples",
            "and",
            "5th Avenue South &",
            "& 9th Street South, Naples",
        ] {
            let found = options(query);
            let hits = searcher().search(&found).expect("search");
            let main = before(&found);
            assert_eq!(ids(&hits), ids(&main), "{query}");
            assert_eq!(
                hits.iter().map(at).collect::<Vec<_>>(),
                main.iter().map(at).collect::<Vec<_>>(),
                "{query}"
            );
        }
    }

    #[test]
    fn a_layer_other_than_street_keeps_the_answer_as_before() {
        let found = TextSearchOptions {
            layer: Some("address".to_string()),
            ..options("5th Avenue South and 9th Street South, Naples, FL")
        };
        assert_eq!(
            ids(&searcher().search(&found).expect("search")),
            ids(&before(&found))
        );
    }

    #[test]
    fn a_crossing_outside_the_box_is_not_given() {
        let found = TextSearchOptions {
            bbox: Some(crate::search::BoundingBox {
                min_lon: -81.80,
                min_lat: 26.141,
                max_lon: -81.79,
                max_lat: 26.15,
            }),
            ..options("5th Avenue South and 9th Street South, Naples, FL")
        };
        let hits = searcher().search(&found).expect("search");
        assert!(hits.iter().all(|hit| at(hit) != NAPLES_CROSSING));
    }

    fn feature(query: &str) -> Value {
        let collection = forward_collection(&searcher().pack, query.to_string(), ask(query))
            .expect("collection");
        serde_json::to_value(collection).expect("json")["features"][0].clone()
    }

    #[test]
    fn the_crossing_is_a_street_result_that_names_both_streets_and_the_area() {
        let feature = feature("5th Ave S & 9th St S, Naples, FL");
        let properties = &feature["properties"];
        assert_eq!(properties["layer"], "street");
        assert_eq!(properties["accuracy"], "street");
        assert_eq!(properties["name"], "5th Avenue South & 9th Street South");
        assert_eq!(
            properties["label"],
            "5th Avenue South & 9th Street South, Naples"
        );
        assert_eq!(
            properties["match"],
            json!({"street": "matched", "locality": "matched", "region": "matched"})
        );
        assert_eq!(properties["id"], "derived:crossing:osm:way:101:osm:way:102");
        assert_eq!(properties["address"], Value::Null);
    }

    fn fields(address: &str) -> AddressGeocodeOptions {
        AddressGeocodeOptions {
            countries: Vec::new(),
            address: address.to_string(),
            locality: Some("Naples".to_string()),
            region: Some("FL".to_string()),
            postcode: None,
            limit: 5,
            layer: None,
        }
    }

    #[test]
    fn address_fields_take_the_crossing_too() {
        let found = searcher()
            .geocode_address(&fields("5th Avenue South & 9th Street South"))
            .expect("geocode")
            .expect("an answer");
        assert_at(&found.hit, NAPLES_CROSSING);
        assert_eq!(found.hit.record.name, "5th Avenue South & 9th Street South");
    }

    #[test]
    fn address_fields_that_name_no_crossing_answer_as_before() {
        let fields = fields("Bed and Breakfast");
        let now = searcher().geocode_address(&fields).expect("geocode");
        let main = searcher()
            .geocode_address_readings(&fields, None, None)
            .expect("geocode");
        assert_eq!(
            now.map(|found| found.hit.record.id),
            main.map(|found| found.hit.record.id)
        );
    }
}
