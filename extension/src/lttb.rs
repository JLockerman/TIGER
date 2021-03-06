
use pgx::*;
use std::borrow::Cow;

use crate::{
    aggregate_utils::in_aggregate_context, flatten, palloc::Internal,
};

use time_series::{TSPoint, TimeSeries as InternalTimeSeries};

use crate::time_series::{TimeSeriesData, SeriesType};

// hack to allow us to qualify names with "timescale_analytics_experimental"
// so that pgx generates the correct SQL
mod timescale_analytics_experimental {
}

pub struct LttbTrans {
    series: InternalTimeSeries,
    resolution: usize,
}

#[pg_extern(schema = "timescale_analytics_experimental")]
pub fn lttb_trans(
    state: Option<Internal<LttbTrans>>,
    time: pg_sys::TimestampTz,
    val: Option<f64>,
    resolution: i32,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<LttbTrans>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let val = match val {
                None => return state,
                Some(val) => val,
            };
            let mut state = match state {
                Some(state) => state,
                None => {
                    if resolution <= 2 {
                        error!("resolution must be greater than 2")
                    }
                    LttbTrans {
                        series: InternalTimeSeries::new_explicit_series(),
                        resolution: resolution as usize,
                    }.into()
                },
            };

            state.series.add_point(TSPoint {
                ts: time,
                val: val,
            });
            Some(state)
        })
    }
}

#[pg_extern(schema = "timescale_analytics_experimental")]
pub fn lttb_final(
    state: Option<Internal<LttbTrans>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<crate::time_series::timescale_analytics_experimental::TimeSeries<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let mut state = match state {
                None => return None,
                Some(state) => state,
            };
            state.series.sort();
            let series = Cow::from(&state.series);
            let downsampled = lttb(&*series, state.resolution);
            flatten!(
                TimeSeries {
                    series: SeriesType::SortedSeries {
                        num_points: &(downsampled.len() as u64),
                        points: &*downsampled,
                    }
                }
            ).into()
        })
    }
}

extension_sql!(r#"
CREATE AGGREGATE timescale_analytics_experimental.lttb(ts TIMESTAMPTZ, value DOUBLE PRECISION, resolution INT) (
    sfunc = timescale_analytics_experimental.lttb_trans,
    stype = internal,
    finalfunc = timescale_analytics_experimental.lttb_final
);
"#);


// based on https://github.com/jeromefroe/lttb-rs version 0.2.0
pub fn lttb(data: &[TSPoint], threshold: usize) -> Cow<'_, [TSPoint]> {
    if threshold >= data.len() || threshold == 0 {
        // Nothing to do.
        return Cow::Borrowed(data)
    }

    let mut sampled = Vec::with_capacity(threshold);

    // Bucket size. Leave room for start and end data points.
    let every = ((data.len() - 2) as f64) / ((threshold - 2) as f64);

    // Initially a is the first point in the triangle.
    let mut a = 0;

    // Always add the first point.
    sampled.push(data[a]);

    for i in 0..threshold - 2 {
        // Calculate point average for next bucket (containing c).
        let mut avg_x = 0i64;
        let mut avg_y = 0f64;

        let avg_range_start = (((i + 1) as f64) * every) as usize + 1;

        let mut end = (((i + 2) as f64) * every) as usize + 1;
        if end >= data.len() {
            end = data.len();
        }
        let avg_range_end = end;

        let avg_range_length = (avg_range_end - avg_range_start) as f64;

        for i in 0..(avg_range_end - avg_range_start) {
            let idx = (avg_range_start + i) as usize;
            avg_x += data[idx].ts;
            avg_y += data[idx].val;
        }
        avg_x /= avg_range_length as i64;
        avg_y /= avg_range_length;

        // Get the range for this bucket.
        let range_offs = ((i as f64) * every) as usize + 1;
        let range_to = (((i + 1) as f64) * every) as usize + 1;

        // Point a.
        let point_a_x = data[a].ts;
        let point_a_y = data[a].val;

        let mut max_area = -1f64;
        let mut next_a = range_offs;
        for i in 0..(range_to - range_offs) {
            let idx = (range_offs + i) as usize;

            // Calculate triangle area over three buckets.
            let area = ((point_a_x - avg_x) as f64 * (data[idx].val - point_a_y)
                - (point_a_x - data[idx].ts) as f64 * (avg_y - point_a_y))
                .abs()
                * 0.5;
            if area > max_area {
                max_area = area;
                next_a = idx; // Next a is this b.
            }
        }

        sampled.push(data[next_a]); // Pick this point from the bucket.
        a = next_a; // This a is the next a (chosen b).
    }

    // Always add the last point.
    sampled.push(data[data.len() - 1]);

    Cow::Owned(sampled)
}

// currently only have doctests, see docs/lttb