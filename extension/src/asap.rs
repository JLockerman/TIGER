
use pgx::*;
use asap::*;
use serde::{Deserialize, Serialize};

use crate::{
    aggregate_utils::in_aggregate_context, palloc::Internal,
};

use time_series::{TSPoint, TimeSeries as InternalTimeSeries, ExplicitTimeSeries, NormalTimeSeries, GapfillMethod, TimeSeriesError};

use crate::time_series::TimeSeries;

// This is included for debug purposes and probably should not leave experimental
#[pg_extern(schema = "timescale_analytics_experimental")]
pub fn asap_smooth_raw(
    data: Vec<f64>,
    resolution: i32,
) -> Vec<f64> {
    asap_smooth(&data, resolution as u32)
}

// hack to allow us to qualify names with "timescale_analytics_experimental"
// so that pgx generates the correct SQL
mod timescale_analytics_experimental {
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ASAPTransState {
    ts: InternalTimeSeries,
    resolution: i32,
}

#[pg_extern(schema = "timescale_analytics_experimental")]
pub fn asap_trans(
    state: Option<Internal<ASAPTransState>>,
    ts: Option<pg_sys::TimestampTz>,
    val: Option<f64>,
    resolution: i32,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<ASAPTransState>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let p = match (ts, val) {
                (_, None) => return state,
                (None, _) => return state,
                (Some(ts), Some(val)) => TSPoint { ts, val },
            };

            match state {
                None => {
                    Some(ASAPTransState {
                            ts: InternalTimeSeries::Explicit(
                                ExplicitTimeSeries {
                                    ordered: true,
                                    points: vec![p],
                                },
                            ),
                            resolution
                        }.into()
                    )
                }
                Some(mut s) => {
                    s.ts.add_point(p);
                    Some(s)
                }
            }
        })
    }
}

fn find_downsample_interval(series: &ExplicitTimeSeries, resolution: i64) -> i64 {
    assert!(series.ordered);

    // First candidate is simply the total range divided into even size buckets
    let candidate = (series.points.last().unwrap().ts - series.points.first().unwrap().ts) / resolution;

    // Problem with this approach is ASAP appears to deliver much rougher graphs if buckets
    // don't contain an equal number of points.  We try to adjust for this by truncating the
    // downsample_interval to a multiple of the average delta, unfortunately this is very
    // susceptible to gaps in the data.  So instead of the average delta, we use the median.
    let mut diffs = vec!(0; (series.points.len() - 1) as usize);
    for i in 1..series.points.len() as usize {
        diffs[i-1] = series.points[i].ts - series.points[i-1].ts;
    }
    diffs.sort();
    let median = diffs[diffs.len() / 2];
    candidate / median * median  // Truncate candidate to a multiple of median
}

#[pg_extern(schema = "timescale_analytics_experimental")]
fn asap_final(
    state: Option<Internal<ASAPTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<crate::time_series::timescale_analytics_experimental::TimeSeries<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let state = match state {
                None => return None,
                Some(state) => state.clone(),
            };

            if let InternalTimeSeries::Explicit(mut series) = state.ts {
                series.sort();

                // In following the ASAP reference implementation, we only downsample if the number
                // of points is at least twice the resolution.  Otherwise we keep the number of
                // points, but still normalize them to equal sized buckets.
                let normal = if series.points.len() >= 2 * state.resolution as usize {
                    let downsample_interval = find_downsample_interval(&series, state.resolution as i64);
                    series.downsample_and_gapfill_to_normal_form(downsample_interval, GapfillMethod::Linear)
                } else {
                    series.downsample_and_gapfill_to_normal_form((series.points.last().unwrap().ts - series.points.first().unwrap().ts) / series.points.len() as i64, GapfillMethod::Linear)
                };
                let mut normal = match normal {
                    Ok(series) => series,
                    Err(TimeSeriesError::InsufficientDataToExtrapolate) => panic!("Not enough data to generate a smoothed representation"),
                    Err(_) => unreachable!()
                };

                // Drop the last value to match the reference implementation
                normal.values.pop();

                let mut result = NormalTimeSeries {start_ts: normal.start_ts,
                    step_interval: 0,
                    values: asap_smooth(&normal.values, state.resolution as u32)
                };

                // Set the step interval for the asap result so that it covers the same interval
                // as the passed in data
                result.step_interval = normal.step_interval * normal.values.len() as i64 / result.values.len() as i64;
                TimeSeries::from_internal_time_series(&InternalTimeSeries::Normal(result)).into()
            } else {
                panic!("Unexpected timeseries format encountered");
            }
        })
    }
}


// Aggregate on only values (assumes aggregation over ordered normalized timestamp)
extension_sql!(r#"
CREATE AGGREGATE timescale_analytics_experimental.asap_smooth(ts TIMESTAMPTZ, value DOUBLE PRECISION, resolution INT) (
    sfunc = timescale_analytics_experimental.asap_trans,
    stype = internal,
    finalfunc = timescale_analytics_experimental.asap_final
);
"#);

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use pgx::*;

    #[pg_test]
    fn test_asap() {
        Spi::execute(|client| {
            client.select("CREATE TABLE asap_test (date timestamptz, value DOUBLE PRECISION)", None, None);

            // Create a table with some cyclic data
            client.select("insert into asap_test select '2020-1-1 UTC'::timestamptz + make_interval(days=>foo), 10 + 5 * cos(foo) from generate_series(0,1000) foo", None, None);
            // Gap from [1001,1040] then continue cycle
            client.select("insert into asap_test select '2020-1-1 UTC'::timestamptz + make_interval(days=>foo), 10 + 5 * cos(foo) from generate_series(1041,2000) foo", None, None);
            // Values in [2001,2200] are 2 less than normal
            client.select("insert into asap_test select '2020-1-1 UTC'::timestamptz + make_interval(days=>foo), 8 + 5 * cos(foo) from generate_series(2001,2200) foo", None, None);
            // And fill out to 3000
            client.select("insert into asap_test select '2020-1-1 UTC'::timestamptz + make_interval(days=>foo), 10 + 5 * cos(foo) from generate_series(2201,3000) foo", None, None);

            // Smoothing to resolution 100 gives us 95 points so our hole should be around index 32-33
            // and our decreased values should be around 64-72.  However, since the output is
            // rolling averages, expect these values to impact the results around these ranges as well.

            client.select("create table asap_vals as SELECT * FROM timescale_analytics_experimental.unnest_series((SELECT timescale_analytics_experimental.asap_smooth(date, value, 100) FROM asap_test ))", None, None);

            let sanity = client.select("SELECT COUNT(*) FROM asap_vals", None, None).first()
                .get_one::<i32>().unwrap();
            assert_eq!(sanity, 95);

            // First check that our smoothed values away from our impacted ranges are about 10
            let test_val = client
                .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 5", None, None)
                .first()
                .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() < 0.05);
            let test_val = client
                .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 20", None, None)
                .first()
                .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() < 0.05);
            let test_val = client
                .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 55", None, None)
                .first()
                .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() < 0.05);
            let test_val = client
                .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 85", None, None)
                .first()
                .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() < 0.05);

            // There's not too much we can assume about our gap, since it's only one or two data point at our resolution, and they'll be filled with the linear interpolation of the left and right sides and then taken as part of a moving average with the surrounding points.  We will just check that the values are a bit away from 10 around this range.
            let test_val = client
            .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 29", None, None)
            .first()
            .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() > 0.1);
            let test_val = client
            .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 33", None, None)
            .first()
            .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() > 0.1);

            // Finally check that our points near our decreased range are significantly lower.  We don't expect these to necessarily get down to 8 due to the rolling average, but they should be closer to 8 than 10 in the middle of the range.
            let test_val = client
            .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 68", None, None)
            .first()
            .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() > (8.0 - test_val).abs());
            let test_val = client
            .select("SELECT value FROM asap_vals ORDER BY time LIMIT 1 OFFSET 70", None, None)
            .first()
            .get_one::<f64>().unwrap();
            assert!((10.0 - test_val).abs() > (8.0 - test_val).abs());
        });
    }
}
