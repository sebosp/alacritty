//! Exports the TimeSeries class
//! The TimeSeries is a circular buffer that contains an entry per epoch
//! at different granularities. It is maintained as a Vec<(u64, f64)>
//! Metrics will overwrite the contents of the array partially, the start of
//! the metrics and the number of the items shifts, this allows the vector
//! and rotate without relocation of memory or shifting of the vector.

// DONE:
// -- mock the prometheus server and response
// -- When disconnected from a server, it is not easy to know which one or why.
// -- Add space for drawing the charts, right now we are decreasing 2 lines, but it breaks tmux
// -- The dashboards should be toggable, some key combination
// IN PROGRESS:
// -- Group labels into separate colors (find something that does color spacing in rust)
// -- Create a main offset and from there space and calculate the location of the charts
// TODO:
// -- When activated on toggle it could blur a portion of the screen
// -- Create a TimeSeries inside the Term itself so that increments can be done synchronously but
//    send/fetch the updates to the background every half a second or so?

#![warn(rust_2018_idioms)]
#[macro_use]
extern crate serde_derive;

pub mod async_utils;
pub mod config;
pub mod decorations;
pub mod prometheus;

pub use futures;
pub use hyper;
pub use hyper_tls;
pub use percent_encoding;
pub use tokio;
pub use tokio_core;

use decorations::*;
use log::*;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use serde_yaml;
use std::fmt;
use std::str::FromStr;
use std::time::UNIX_EPOCH;
use tracing::{event, span, Level};

/// `MissingValuesPolicy` provides several ways to deal with missing values
/// when drawing the Metric
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum MissingValuesPolicy {
    Zero,
    One,
    First,
    Last,
    Fixed(f64),
    Avg,
    Max,
    Min,
}

impl Default for MissingValuesPolicy {
    fn default() -> MissingValuesPolicy {
        MissingValuesPolicy::Zero
    }
}

/// `ValueCollisionPolicy` handles collisions when several values are collected
/// for the same time unit, allowing for overwriting, incrementing, etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ValueCollisionPolicy {
    Overwrite,
    Increment,
    Decrement,
    Ignore,
}

impl Default for ValueCollisionPolicy {
    fn default() -> ValueCollisionPolicy {
        ValueCollisionPolicy::Increment
    }
}

/// `TimeSeriesStats` contains statistics about the current TimeSeries
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Copy)]
pub struct TimeSeriesStats {
    max: f64,
    min: f64,
    avg: f64, // Calculation may lead to overflow
    first: f64,
    last: f64,
    count: usize,
    sum: f64, // May overflow
    last_epoch: u64,
    is_dirty: bool,
}

impl Default for TimeSeriesStats {
    fn default() -> TimeSeriesStats {
        TimeSeriesStats {
            max: 0f64,
            min: 0f64,
            avg: 0f64,
            first: 0f64,
            last: 0f64,
            count: 0usize,
            sum: 0f64,
            last_epoch: 0u64,
            is_dirty: false,
        }
    }
}

/// `TimeSeries` contains a vector of tuple (epoch, Option<value>)
/// The vector behaves as a circular buffer to avoid shifting values.
/// The circular buffer may be invalidated partially, for example when too much
/// time has passed without metrics, the vecotr is allowed to shrink without
/// memory rellocation, this is achieved by using two indexes for the first
/// and last item.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TimeSeries {
    /// Capture events through time
    /// Contains one entry per time unit
    pub metrics: Vec<(u64, Option<f64>)>,

    /// Number of items request to the metric store
    pub metrics_capacity: usize,

    /// Stats for the TimeSeries
    pub stats: TimeSeriesStats,

    /// Useful for records that do not increment but rather are a fixed
    /// or absolute value recorded at a given time
    pub collision_policy: ValueCollisionPolicy,

    /// Missing values returns a value for a specific time there is no data
    /// recorded.
    pub missing_values_policy: MissingValuesPolicy,

    /// The first item in the circular buffer
    pub first_idx: usize,

    /// How many items are active in our circular buffer
    pub active_items: usize,
}

/// `IterTimeSeries` provides the Iterator Trait for TimeSeries metrics.
/// The state for the iteration is held en "pos" field. The "current_item" is
/// used to determine if further iterations on the circular buffer is needed.
pub struct IterTimeSeries<'a> {
    /// The reference to the TimeSeries struct to iterate over.
    inner: &'a TimeSeries,
    /// The current position state
    pos: usize,
    /// The current item number, to be compared with the active_items
    current_item: usize,
}

/// `Rgb` is a copy of alacritty_terminal/src/term/color.rs
/// Because we also need to deserialize from yaml
#[derive(Debug, Eq, PartialEq, Copy, Clone, Default, Serialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Transform from a hex string, copy from alacritty_terminal/src/term/colors.rs
impl FromStr for Rgb {
    type Err = ();

    fn from_str(s: &str) -> ::std::result::Result<Rgb, ()> {
        let mut chars = s.chars();
        let mut rgb = Rgb::default();

        macro_rules! component {
            ($($c:ident),*) => {
                $(
                    match chars.next().and_then(|c| c.to_digit(16)) {
                        Some(val) => rgb.$c = (val as u8) << 4,
                        None => return Err(())
                    }

                    match chars.next().and_then(|c| c.to_digit(16)) {
                        Some(val) => rgb.$c |= val as u8,
                        None => return Err(())
                    }
                )*
            }
        }

        match chars.next() {
            Some('0') => {
                if chars.next() != Some('x') {
                    return Err(());
                }
            }
            Some('#') => (),
            _ => return Err(()),
        }

        component!(r, g, b);

        Ok(rgb)
    }
}

/// Deserialize an Rgb from a hex string, copy from alacritty_terminal/src/term/colors.rs
impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RgbVisitor;

        // Used for deserializing reftests
        #[derive(Deserialize)]
        struct RgbDerivedDeser {
            r: u8,
            g: u8,
            b: u8,
        }

        impl<'a> Visitor<'a> for RgbVisitor {
            type Value = Rgb;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("hex color like 0xff00ff")
            }

            fn visit_str<E>(self, value: &str) -> ::std::result::Result<Rgb, E>
            where
                E: ::serde::de::Error,
            {
                Rgb::from_str(&value[..])
                    .map_err(|_| E::custom("failed to parse rgb; expected hex color like 0xff00ff"))
            }
        }

        // Return an error if the syntax is incorrect
        let value = serde_yaml::Value::deserialize(deserializer)?;

        // Attempt to deserialize from struct form
        if let Ok(RgbDerivedDeser { r, g, b }) = RgbDerivedDeser::deserialize(value.clone()) {
            return Ok(Rgb { r, g, b });
        }

        // Deserialize from hex notation (either 0xff00ff or #ff00ff)
        match value.deserialize_str(RgbVisitor) {
            Ok(rgb) => Ok(rgb),
            Err(err) => {
                error!(
                    "Rgb::deserialize: Problem with config: {}; using color #000000",
                    err
                );
                Ok(Rgb::default())
            }
        }
    }
}
/// `ManualTimeSeries` is a basic time series that we feed ourselves, used for internal counters
/// for example keyboard input, output newlines, loaded items count.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ManualTimeSeries {
    /// The name of the ManualTimeSeries
    pub name: String,

    /// The TimeSeries that contains the data
    #[serde(default)]
    pub series: TimeSeries,

    /// The granularity to store
    #[serde(default)]
    pub granularity: u64,

    /// The color of the TimeSeries
    #[serde(default)]
    pub color: Rgb,

    /// The transparency of the TimeSeries
    #[serde(default)]
    pub alpha: f32,
}

impl Default for ManualTimeSeries {
    fn default() -> ManualTimeSeries {
        ManualTimeSeries {
            name: String::from("unkown"),
            series: TimeSeries::default(),
            granularity: 1, // 1 second
            color: Rgb::default(),
            alpha: 1.0,
        }
    }
}

/// `TimeSeriesSource` contains several types of time series that can be extended
/// with drawable data
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type")]
pub enum TimeSeriesSource {
    #[serde(rename = "prometheus")]
    PrometheusTimeSeries(prometheus::PrometheusTimeSeries),
    #[serde(rename = "alacritty_input")]
    AlacrittyInput(ManualTimeSeries),
    #[serde(rename = "alacritty_output")]
    AlacrittyOutput(ManualTimeSeries),
    #[serde(rename = "async_items_loaded")]
    AsyncLoadedItems(ManualTimeSeries),
}

impl Default for TimeSeriesSource {
    fn default() -> TimeSeriesSource {
        TimeSeriesSource::AlacrittyInput(ManualTimeSeries::default())
    }
}

impl TimeSeriesSource {
    /// `init` calls functions that are inside our TimeSeries sources to
    /// setup specific flags that should be turned on
    pub fn init(&mut self) {
        if let TimeSeriesSource::PrometheusTimeSeries(x) = self {
            x.init();
        }
    }

    /// `series` returns an immutable series copy of the content.
    pub fn series(&self) -> TimeSeries {
        match self {
            TimeSeriesSource::PrometheusTimeSeries(x) => x.series.clone(),
            TimeSeriesSource::AlacrittyInput(x) => x.series.clone(),
            TimeSeriesSource::AlacrittyOutput(x) => x.series.clone(),
            TimeSeriesSource::AsyncLoadedItems(x) => x.series.clone(),
        }
    }

    /// `series_mut` returns a mutable reference to the underlying series
    pub fn series_mut(&mut self) -> &mut TimeSeries {
        match self {
            TimeSeriesSource::PrometheusTimeSeries(x) => &mut x.series,
            TimeSeriesSource::AlacrittyInput(x) => &mut x.series,
            TimeSeriesSource::AlacrittyOutput(x) => &mut x.series,
            TimeSeriesSource::AsyncLoadedItems(x) => &mut x.series,
        }
    }

    pub fn name(&self) -> String {
        match self {
            TimeSeriesSource::PrometheusTimeSeries(x) => x.name.clone(),
            TimeSeriesSource::AlacrittyInput(x) => x.name.clone(),
            TimeSeriesSource::AlacrittyOutput(x) => x.name.clone(),
            TimeSeriesSource::AsyncLoadedItems(x) => x.name.clone(),
        }
    }

    // XXX: SEB: This is really ugly, we should have maybe Trait for Drawable and have a color
    // easily available or have like a .prop("color").
    pub fn color(&self) -> Rgb {
        match self {
            TimeSeriesSource::PrometheusTimeSeries(x) => x.color,
            TimeSeriesSource::AlacrittyInput(x) => x.color,
            TimeSeriesSource::AlacrittyOutput(x) => x.color,
            TimeSeriesSource::AsyncLoadedItems(x) => x.color,
        }
    }

    pub fn alpha(&self) -> f32 {
        match self {
            TimeSeriesSource::PrometheusTimeSeries(x) => x.alpha,
            TimeSeriesSource::AlacrittyInput(x) => x.alpha,
            TimeSeriesSource::AlacrittyOutput(x) => x.alpha,
            TimeSeriesSource::AsyncLoadedItems(x) => x.alpha,
        }
    }
}

/// `Value2D` provides X,Y values for several uses, such as offset, padding
#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct Value2D {
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
}

/// `SizeInfo` is a copy of the Alacritty SizeInfo, XXX: remove on merge.
#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct SizeInfo {
    pub width: f32,
    pub height: f32,
    pub chart_width: f32,
    pub chart_height: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    pub padding_x: f32,
    pub padding_y: f32,
}

impl SizeInfo {
    /// `scale_x` Scales the value from the current display boundary to
    /// a cartesian plane from [-1.0, 1.0], where -1.0 is 0px (left-most) and
    /// 1.0 is the `display_width` parameter (right-most), i.e. 1024px.
    pub fn scale_x(&self, input_value: f32) -> f32 {
        let center_x = self.width / 2.;
        let x = self.padding_x + input_value;
        (x - center_x) / center_x
    }

    /// `scale_y_to_size` Scales the value from the current display boundary to
    /// a cartesian plane from [-1.0, 1.0], where 1.0 is 0px (top) and -1.0 is
    /// the `display_height` parameter (bottom), i.e. 768px.
    pub fn scale_y(&self, max_value: f64, input_value: f64) -> f32 {
        let center_y = self.height / 2.;
        // From the bottom of the chart, what is the position of the input_value:
        // max_value    = input_value
        // chart_height   x
        let y_metric_value = (input_value as f32 * self.chart_height) / max_value as f32;
        let y = self.height - 2. * self.padding_y - y_metric_value;
        -(y - center_y) / center_y
    }
}

/// `ChartsConfig` contains a vector of charts and basic position of the charts,
/// allowing to use a global position instead of individually setting up the chart position
#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ChartsConfig {
    /// The x,y coordinates in which the time series will start
    pub position: Option<Value2D>,
    /// The default dimensions of the chart
    pub default_dimensions: Option<Value2D>,
    /// The default spacing between the charts
    pub spacing: f32,
    // An array of charts to draw
    pub charts: Vec<TimeSeriesChart>,
}

impl ChartsConfig {
    /// Goes through the charts inside the ChartConfig and if position is not set it calculates it.
    pub fn setup_chart_spacing(&mut self) {
        let mut current_position = self.position;
        for chart in &mut self.charts {
            if chart.position.is_none() {
                current_position = if let (Some(position), Some(dimensions)) =
                    (current_position, self.default_dimensions)
                {
                    chart.position = current_position;
                    Some(Value2D {
                        x: position.x + dimensions.x + self.spacing,
                        y: 0.,
                    })
                } else {
                    event!(
                        Level::ERROR,
                        "setup_chart_spacing: default dimensions and position were not given for charts and dimensions and positions are missing for chart: {}",
                        chart.name
                    );
                    self.position
                }
            }
            if chart.dimensions.is_none() {
                chart.dimensions = self.default_dimensions;
            }
        }
    }
}

/// `TimeSeriesChart` has an array of TimeSeries to display, it contains the
/// X, Y position and has methods to draw in opengl.
#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct TimeSeriesChart {
    /// The name of the Chart
    pub name: String,

    /// The different sources of the TimeSeries to draw
    #[serde(rename = "series")]
    pub sources: Vec<TimeSeriesSource>,

    /// Decorations such as color, transparency, etc
    #[serde(default)]
    pub decorations: Vec<Decoration>,

    /// The merged stats of the TimeSeries
    #[serde(default)]
    pub stats: TimeSeriesStats,

    /// The x,y position in which the time series should be drawn
    /// If unspecified, a position will be reserved from the ChartsConfig offset values.
    #[serde(default)]
    pub position: Option<Value2D>,

    /// The dimensions of the chart.
    /// If unspecified the default_dimensions are used from ChartsConfig
    #[serde(default)]
    pub dimensions: Option<Value2D>,

    /// The opengl representation of the each series.
    #[serde(default)]
    pub opengl_vecs: Vec<Vec<f32>>,

    /// Last updated epoch
    #[serde(default)]
    pub last_updated: u64,
}

impl TimeSeriesChart {
    /// `update_series_opengl_vecs` Represents the activity levels values in a
    /// drawable vector for opengl, for a specific index in the series array
    pub fn update_series_opengl_vecs(&mut self, series_idx: usize, display_size: SizeInfo) {
        let span = span!(
            Level::TRACE,
            "update_series_opengl_vecs",
            name = self.name.clone().as_str(),
            series_idx = series_idx,
        );
        let _enter = span.enter();
        if series_idx >= self.sources.len() {
            event!(
                Level::ERROR,
                "update_series_opengl_vecs: Request for out of bound series index: {}",
                series_idx
            );
            return;
        }
        while self.opengl_vecs.capacity() <= self.sources.capacity() {
            self.opengl_vecs.push(vec![]);
        }
        let mut display_size = display_size;
        if let Some(dimensions) = self.dimensions {
            display_size.chart_height = dimensions.y;
            display_size.chart_width = dimensions.x;
        } else {
            // TODO: When the charts are first read, they should compose the dimensions.
            // If we hit this, then we should recalculate from the global ChartsConfig default
            // dimensions somehow
        }
        // Get the opengl representation of the vector
        let opengl_vecs_capacity = self.sources[series_idx].series().active_items;
        if opengl_vecs_capacity > self.opengl_vecs[series_idx].capacity() {
            let missing_capacity = opengl_vecs_capacity - self.opengl_vecs[series_idx].capacity();
            self.opengl_vecs[series_idx].reserve(missing_capacity);
        }
        event!(
            Level::DEBUG,
            "update_series_opengl_vecs: Needed OpenGL capacity: {}, Display Size: {:?}, offset \
             {:?}",
            opengl_vecs_capacity,
            display_size,
            self.position,
        );
        for source in &mut self.sources {
            if source.series().stats.is_dirty {
                event!(
                    Level::DEBUG,
                    "update_series_opengl_vecs: '{}' stats are dirty, needs recalculating",
                    source.name()
                );
                source.series_mut().calculate_stats();
            }
        }
        self.calculate_stats();
        let mut decorations_space = 0f32;
        for decoration in &self.decorations {
            event!(
                Level::DEBUG,
                "update_series_opengl_vecs: Adding width of decoration: {}",
                decoration.width()
            );
            decorations_space += decoration.width();
        }
        event!(
            Level::DEBUG,
            "update_series_opengl_vecs: width: {}, decorations_space: {}",
            display_size.chart_width,
            decorations_space
        );
        let missing_values_fill = self.sources[series_idx].series().get_missing_values_fill();
        event!(
            Level::DEBUG,
            "update_series_opengl_vecs: Using {} to fill missing values. Metrics[{}]: {:?}",
            missing_values_fill,
            self.sources[series_idx].series().metrics_capacity,
            self.sources[series_idx].series()
        );
        let tick_spacing = (display_size.chart_width - decorations_space)
            / self.sources[series_idx].series().metrics_capacity as f32;
        event!(
            Level::DEBUG,
            "update_series_opengl_vecs: Using tick_spacing {}",
            tick_spacing
        );
        // The decorations width request is on both left and right sides.
        let decoration_offset = decorations_space / 2f32;
        for (idx, metric) in self.sources[series_idx].series().iter().enumerate() {
            let x_value = idx as f32 * tick_spacing + decoration_offset;
            // If there is a Marker Line, it takes 10% of the initial horizontal space
            let y_value = match metric.1 {
                Some(x) => x,
                None => missing_values_fill,
            };
            let scaled_x = display_size.scale_x(x_value + self.position.unwrap_or_default().x);
            let scaled_y = display_size.scale_y(self.stats.max, y_value);
            // Adding twice to a vec, could this be made into one operation? Is this slow?
            // need to transform activity line values from varying levels into scaled [-1, 1]
            // XXX: Move to Circular Buffer? Problem is Circular buffer is only meant for epochs
            if (idx + 1) * 2 > self.opengl_vecs[series_idx].len() {
                self.opengl_vecs[series_idx].push(scaled_x);
                self.opengl_vecs[series_idx].push(scaled_y);
            } else {
                self.opengl_vecs[series_idx][idx * 2] = scaled_x;
                self.opengl_vecs[series_idx][idx * 2 + 1] = scaled_y;
            }
        }
        for decoration in &mut self.decorations {
            event!(
                Level::DEBUG,
                "update_series_opengl_vecs: Updating decoration {:?} vertices",
                decoration
            );
            decoration.update_opengl_vecs(
                display_size,
                self.position.unwrap_or_default(),
                &self.stats,
                &self.sources,
            );
        }
        self.last_updated = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// `update_all_series_opengl_vecs` Represents the activity levels values in a
    /// drawable vector for opengl for all the available series in the current chart
    pub fn update_all_series_opengl_vecs(&mut self, display_size: SizeInfo) {
        let span = span!(
            Level::TRACE,
            "update_all_series_opengl_vecs",
            name = self.name.clone().as_str()
        );
        let _enter = span.enter();
        event!(Level::DEBUG, "update_all_series_opengl_vecs: Starting");
        for idx in 0..self.sources.len() {
            self.update_series_opengl_vecs(idx, display_size);
        }
        event!(Level::DEBUG, "update_all_series_opengl_vecs: Finished");
    }

    /// `calculate_stats` Iterates over the time series stats and merges them.
    /// This will also go through the decorations and account for the requested
    /// draw space for them.
    pub fn calculate_stats(&mut self) {
        let span = span!(
            Level::TRACE,
            "calculate_stats",
            name = self.name.clone().as_str()
        );
        let _enter = span.enter();
        event!(Level::TRACE, "TimeSeriesChart::calculate_stats start");
        let mut max_activity_value = std::f64::MIN;
        let mut min_activity_value = std::f64::MAX;
        let mut sum_activity_values = 0f64;
        let mut total_count = 0usize;
        let mut max_epoch = 0u64;
        for source in &mut self.sources {
            if source.series_mut().stats.is_dirty {
                source.series_mut().calculate_stats();
            }
        }
        for source in &self.sources {
            if source.series().stats.max > max_activity_value {
                max_activity_value = source.series().stats.max;
            }
            if source.series().stats.last_epoch > max_epoch {
                max_epoch = source.series().stats.last_epoch;
            }
            if source.series().stats.min < min_activity_value {
                min_activity_value = source.series().stats.min;
            }
            sum_activity_values += source.series().stats.sum;
            total_count += source.series().stats.count;
        }
        // Account for the decoration requested height
        for decoration in &self.decorations {
            let top_value = decoration.top_value();
            let bottom_value = decoration.bottom_value();
            if top_value > max_activity_value {
                max_activity_value = top_value
            }
            if bottom_value < min_activity_value {
                min_activity_value = bottom_value;
            }
        }
        self.stats.count = total_count;
        self.stats.max = max_activity_value;
        self.stats.min = min_activity_value;
        self.stats.sum = sum_activity_values;
        self.stats.avg = sum_activity_values / total_count as f64;
        self.stats.is_dirty = false;
        self.stats.last_epoch = max_epoch;
        event!(
            Level::DEBUG,
            "TimeSeriesChart::calculate_stats: Updated statistics to: {:?}",
            self.stats
        );
    }

    /// `get_deduped_opengl_vecs` returns a minimized version of the opengl_vecs, when the metric
    /// doesn't change it doesn't create a new opengl vertex but rather tries to create a wider
    /// line
    pub fn get_deduped_opengl_vecs(&self, series_idx: usize) -> Vec<f32> {
        let span = span!(Level::TRACE, "get_deduped_opengl_vecs", series_idx);
        let _enter = span.enter();
        if series_idx >= self.opengl_vecs.len() {
            return vec![];
        }
        if self.opengl_vecs[series_idx].len() <= 4 {
            return self.opengl_vecs[series_idx].clone();
        }
        // By default, accomodate memory for as many active items as there are in the series circular buffer.
        let mut res = Vec::with_capacity(self.sources[series_idx].series().active_items * 2);
        // Grab the first reference point
        let mut cur_x = self.opengl_vecs[series_idx][0];
        let mut cur_y = self.opengl_vecs[series_idx][1];
        res.push(cur_x);
        res.push(cur_y);
        // Avoid adding the last item twice:
        let mut last_item_added = false;
        for (idx, vertex) in self.opengl_vecs[series_idx].iter().enumerate() {
            if idx == self.sources[series_idx].series().active_items * 2 {
                break;
            }
            if idx % 2 == 1 {
                // This is a Y value
                // Let's allow this much difference and consider them equal
                if (cur_y - *vertex).abs() > 0.001 {
                    // This means the metric has changed, so let's push old X,Y (old value)
                    // unless it happens to have been the last instered item
                    if !last_item_added {
                        res.push(cur_x);
                        res.push(cur_y);
                    }
                    // Add a point to the new value
                    res.push(cur_x);
                    res.push(*vertex);
                    // And now reset the current y value:
                    cur_y = *vertex;
                    last_item_added = true;
                } else {
                    last_item_added = false;
                }
            } else {
                cur_x = *vertex;
            }
        }
        if !last_item_added {
            // If there are only two items, we should append the last read
            // X, Y
            res.push(cur_x);
            res.push(cur_y);
        }
        debug!(
            "get_deduped_opengl_vecs[{}] len({}) result: {:?}",
            series_idx,
            res.len(),
            res
        );
        res
    }

    /// `synchronize_series_epoch_range` ensures that, for the items inside a chart.series vector,
    /// the epochs are synchronized so that we can draw them and make sense of their values.
    pub fn synchronize_series_epoch_range(&mut self) {
        let span = span!(Level::TRACE, "synchronize_series_epoch_range");
        let _enter = span.enter();
        let last_epoch = self.stats.last_epoch;
        let updated_series: usize = self
            .sources
            .iter_mut()
            .map(|x| x.series_mut().upsert((last_epoch, None)))
            .sum();
        event!(
            Level::DEBUG,
            "synchronize_series_epoch_range: Total number of items added to series {}",
            updated_series
        );
    }
}

impl Default for TimeSeries {
    fn default() -> TimeSeries {
        // This leads to 5 mins of metrics to show by default.
        let default_capacity = 300usize;
        TimeSeries {
            metrics_capacity: default_capacity,
            metrics: Vec::with_capacity(default_capacity),
            stats: TimeSeriesStats::default(),
            collision_policy: ValueCollisionPolicy::default(),
            missing_values_policy: MissingValuesPolicy::default(),
            first_idx: 0,
            active_items: 0,
        }
    }
}

impl TimeSeries {
    /// `with_capacity` builder changes the amount of metrics in the vec
    pub fn with_capacity(self, n: usize) -> TimeSeries {
        let mut new_self = self;
        new_self.metrics = Vec::with_capacity(n);
        new_self.metrics_capacity = n;
        new_self
    }

    /// `with_missing_values_policy` receives a String and returns
    /// a MissingValuesPolicy, TODO: the "Fixed" value is not implemented.
    pub fn with_missing_values_policy(mut self, policy_type: String) -> TimeSeries {
        self.missing_values_policy = match policy_type.as_ref() {
            "zero" => MissingValuesPolicy::Zero,
            "one" => MissingValuesPolicy::One,
            "min" => MissingValuesPolicy::Min,
            "max" => MissingValuesPolicy::Max,
            "last" => MissingValuesPolicy::Last,
            "avg" => MissingValuesPolicy::Avg,
            "first" => MissingValuesPolicy::First,
            _ => {
                // TODO: Implement FromStr somehow
                MissingValuesPolicy::Zero
            }
        };
        self
    }

    /// `calculate_stats` Iterates over the metrics and sets the stats
    pub fn calculate_stats(&mut self) {
        // Recalculating seems to be necessary because we are constantly
        // moving items out of the Vec<> so our cache can easily get out of
        // sync
        let mut max_activity_value = std::f64::MIN;
        let mut min_activity_value = std::f64::MAX;
        let mut sum_activity_values = 0f64;
        let mut filled_metrics = 0usize;
        // XXX What is it the vec is empty? what should `first` and `last` be?
        let mut first = 0.;
        let mut last = 0.;
        let mut is_first_filled = false;
        let mut max_epoch = 0u64;
        for entry in self.iter() {
            if entry.0 > max_epoch {
                max_epoch = entry.0;
            }
            if let Some(metric) = entry.1 {
                if !is_first_filled {
                    is_first_filled = true;
                    first = metric;
                }
                if metric > max_activity_value {
                    max_activity_value = metric;
                }
                if metric < min_activity_value {
                    min_activity_value = metric;
                }
                sum_activity_values += metric;
                filled_metrics += 1;
                last = metric;
            } else {
                if !is_first_filled {
                    is_first_filled = true;
                    first = self.get_missing_values_fill();
                }
                last = self.get_missing_values_fill();
            }
        }
        self.stats.max = max_activity_value;
        self.stats.min = min_activity_value;
        self.stats.sum = sum_activity_values;
        self.stats.avg = sum_activity_values / (filled_metrics as f64);
        self.stats.count = filled_metrics;
        self.stats.first = first;
        self.stats.last = last;
        self.stats.last_epoch = max_epoch;
        self.stats.is_dirty = false;
    }

    /// `get_missing_values_fill` uses the MissingValuesPolicy to decide
    /// which value to place on empty metric timeslots when drawing
    pub fn get_missing_values_fill(&self) -> f64 {
        match self.missing_values_policy {
            MissingValuesPolicy::Zero => 0f64,
            MissingValuesPolicy::One => 1f64,
            MissingValuesPolicy::Min => self.stats.min,
            MissingValuesPolicy::Max => self.stats.max,
            MissingValuesPolicy::Last => self.get_last_filled(),
            MissingValuesPolicy::First => self.get_first_filled(),
            MissingValuesPolicy::Avg => self.stats.avg,
            MissingValuesPolicy::Fixed(val) => val,
        }
    }

    /// `resolve_metric_collision` ensures the policy for colliding values is
    /// applied.
    pub fn resolve_metric_collision(&self, existing: Option<f64>, new: Option<f64>) -> Option<f64> {
        if let Some(new) = new {
            if let Some(existing) = existing {
                Some(match self.collision_policy {
                    ValueCollisionPolicy::Increment => existing + new,
                    ValueCollisionPolicy::Overwrite => new,
                    ValueCollisionPolicy::Decrement => existing - new,
                    ValueCollisionPolicy::Ignore => existing,
                })
            } else {
                Some(new)
            }
        } else {
            // Return existing regardless of type as new is None
            existing
        }
    }

    /// `circular_push` adds an item to the circular buffer
    fn circular_push(&mut self, input: (u64, Option<f64>)) {
        if self.metrics.len() < self.metrics_capacity {
            self.metrics.push(input);
            self.active_items += 1;
        } else {
            let target_idx = (self.first_idx + self.active_items) % self.metrics_capacity;
            self.metrics[target_idx] = input;
            if self.active_items < self.metrics_capacity {
                self.active_items += 1;
            } else if self.active_items == self.metrics_capacity {
                self.first_idx = (self.first_idx + 1) % self.metrics_capacity;
            }
        }
        self.stats.is_dirty = true;
    }

    /// `get_last_idx` returns the last index that was used in the circular buffer
    fn get_last_idx(&self) -> usize {
        (self.first_idx + self.active_items - 1) % self.metrics.len()
    }

    /// `get_tail_negative_offset_idx` return a negative offset from the last index in the array
    /// useful when metrics arrive that occurred in the past of the active metrics epoch range
    /// The value of offset should be negative
    fn get_tail_negative_offset_idx(&self, offset: i64) -> usize {
        ((self.metrics.len() as i64 + self.get_last_idx() as i64 + offset)
            % self.metrics.len() as i64) as usize
    }

    /// `upsert` Adds values to the circular buffer adding empty entries for
    /// missing entries, may invalidate the buffer if all data is outdated
    /// it returns the number of inserted records
    pub fn upsert(&mut self, input: (u64, Option<f64>)) -> usize {
        // maybe accept a batch to overwrite the data receiving an array.
        let span = span!(Level::TRACE, "upsert");
        let _enter = span.enter();
        if self.metrics.is_empty() {
            self.circular_push(input);
            return 1;
        }
        let last_idx = self.get_last_idx();
        if (self.metrics[last_idx].0 as i64 - input.0 as i64) >= self.metrics_capacity as i64 {
            // The timestamp is too old and should be discarded.
            // This means we cannot scroll back in time.
            // i.e. if the date of the computer needs to go back in time
            // we would need to restart the terminal to see metrics
            // XXX: What about timezones?
            return 0;
        }
        // as_vec() is 5, 6, 7, 3, 4
        // active_items: 3
        // input.0: 5
        // inactive_time = -2
        let inactive_time = input.0 as i64 - self.metrics[last_idx].0 as i64;
        if inactive_time > self.metrics_capacity as i64 {
            // The whole vector should be discarded
            self.first_idx = 0;
            self.metrics[0] = input;
            self.active_items = 1;
            return 1;
        } else if inactive_time < 0 {
            // We have a metric for an epoch in the past.
            let current_min_epoch = self.metrics[self.first_idx].0;
            // input 98
            // [ 100 ] [ ] [ ] [ ]
            if current_min_epoch > input.0 {
                // The input epoch before anything we have registered.
                // But still within our capacity boundaries
                let padding_items = (current_min_epoch - input.0) as usize;
                // XXX: This is wrong, we should add as many padding_items as possible without
                // breaking the metrics_capacity.
                if self.metrics.len() + 1 < self.metrics_capacity {
                    // The vector is not full, let's shift the items to the right
                    // The array items have not been allocated at this point:
                    self.metrics.insert(0, input);
                    for idx in 1..padding_items {
                        self.metrics.insert(idx, (input.0 + idx as u64, None));
                    }
                    self.active_items += padding_items;
                    return padding_items;
                } else {
                    // The vector is full, write the new epoch at first_idx and then fill the rest
                    // up to current_min value with None
                    let previous_min_epoch = self.metrics[self.first_idx].0;
                    // Find what would be the first index given the current input, in case we need
                    // to roll back from the end of the array
                    let target_idx = self.get_tail_negative_offset_idx(inactive_time);
                    self.first_idx = target_idx;
                    self.metrics[target_idx] = input;
                    for fill_epoch in (input.0 + 1)..previous_min_epoch {
                        self.circular_push((fill_epoch, None));
                    }
                    self.active_items += 1;
                    return (previous_min_epoch - input.0) as usize;
                }
            } else {
                // The input epoch has already been inserted in our array
                let target_idx = self.get_tail_negative_offset_idx(inactive_time);
                if self.metrics[target_idx].0 != input.0 {
                    event!(Level::ERROR,
                        "upsert: lost synchrony len: {}, first_idx: {}, last_idx: {}, target_idx: {}, inactive_time: {}, input: {}, target_idx data: {} metrics: {:?}",
                        self.metrics.len(),
                        self.first_idx,
                        last_idx,
                        target_idx,
                        inactive_time,
                        input.0,
                        self.metrics[target_idx].0,
                        self.metrics
                    );
                }
                self.metrics[target_idx].1 =
                    self.resolve_metric_collision(self.metrics[target_idx].1, input.1);
                return 0;
            }
        } else if inactive_time == 0 {
            // We have a metric for the last indexed epoch
            self.metrics[last_idx].1 =
                self.resolve_metric_collision(self.metrics[last_idx].1, input.1);
            return 0;
        } else {
            // The input epoch is in the future
            let max_epoch = self.metrics[last_idx].0;
            // Fill missing entries with None
            for fill_epoch in (max_epoch + 1)..input.0 {
                self.circular_push((fill_epoch, None));
            }
            self.circular_push(input);
            return 1;
        }
    }

    /// `get_last_filled` Returns the last filled entry in the circular buffer
    pub fn get_last_filled(&self) -> f64 {
        let mut idx = self.get_last_idx();
        loop {
            if let Some(res) = self.metrics[idx].1 {
                return res;
            }
            idx = if idx == 0 {
                self.metrics.len()
            } else {
                idx - 1
            };
            if idx == self.first_idx {
                break;
            }
        }
        0f64
    }

    /// `get_first_filled` Returns the first filled entry in the circular buffer
    pub fn get_first_filled(&self) -> f64 {
        for entry in self.iter() {
            if let Some(metric) = entry.1 {
                return metric;
            }
        }
        0f64
    }

    /// `as_vec` Returns the circular buffer in flat vec format
    // ....[c]
    // ..[b].[d]
    // [a].....[e]
    // ..[h].[f]
    // ....[g]
    // first_idx = "^"
    // last_idx  = "v"
    // [a][b][c][d][e][f][g][h]
    //  0  1  2  3  4  5  6  7
    //  ^v                        # empty
    //  ^  v                      # 0
    //  ^                       v # vec full
    //  v                    ^    # 7
    pub fn as_vec(&self) -> Vec<(u64, Option<f64>)> {
        if self.metrics.is_empty() {
            return vec![];
        }
        let mut res: Vec<(u64, Option<f64>)> = Vec::with_capacity(self.metrics_capacity);
        for entry in self.iter() {
            res.push(entry.clone());
        }
        res
    }

    pub fn push_current_epoch(&mut self, input: f64) {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.upsert((now, Some(input)));
    }

    // `iter` Returns an Iterator from the current start.
    fn iter(&self) -> IterTimeSeries<'_> {
        IterTimeSeries {
            inner: self,
            pos: self.first_idx,
            current_item: 0,
        }
    }
}

impl<'a> Iterator for IterTimeSeries<'a> {
    type Item = &'a (u64, Option<f64>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.inner.metrics.is_empty() || self.current_item == self.inner.active_items {
            return None;
        }
        let curr_pos = self.pos % self.inner.metrics.len();
        self.pos = (self.pos + 1) % (self.inner.metrics.len() + 1);
        self.current_item += 1;
        Some(&self.inner.metrics[curr_pos])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn it_pushes_circular_buffer() {
        // The circular buffer inserts rotating the first and last index
        let mut test = TimeSeries::default().with_capacity(4);
        test.circular_push((10, Some(0f64)));
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 1);
        test.circular_push((11, Some(1f64)));
        test.circular_push((12, None));
        test.circular_push((13, Some(3f64)));
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 4);
        assert_eq!(
            test.metrics,
            vec![
                (10, Some(0f64)),
                (11, Some(1f64)),
                (12, None),
                (13, Some(3f64))
            ]
        );
        test.circular_push((14, Some(4f64)));
        assert_eq!(
            test.metrics,
            vec![
                (14, Some(4f64)),
                (11, Some(1f64)),
                (12, None),
                (13, Some(3f64))
            ]
        );
        assert_eq!(test.first_idx, 1);
        assert_eq!(test.active_items, 4);
        test.circular_push((15, Some(5f64)));
        assert_eq!(
            test.metrics,
            vec![
                (14, Some(4f64)),
                (15, Some(5f64)),
                (12, None),
                (13, Some(3f64))
            ]
        );
    }

    #[test]
    fn it_upserts() {
        // 12th should be overwritten.
        let mut test = TimeSeries::default().with_capacity(4);
        test.upsert((13, Some(3f64)));
        test.upsert((10, Some(0f64)));
        test.upsert((11, Some(1f64)));
        test.upsert((12, None));
        assert_eq!(
            test.metrics,
            vec![
                (10, Some(0f64)),
                (11, Some(1f64)),
                (12, None),
                (13, Some(3f64))
            ]
        );
        assert_eq!(test.first_idx, 0);
        test.upsert((15, Some(5f64)));
        assert_eq!(
            test.metrics,
            vec![(14, None), (15, Some(5f64)), (12, None), (13, Some(3f64))]
        );
        assert_eq!(test.first_idx, 2);
        let input = (11, Some(11f64));
        let last_idx = test.get_last_idx();
        assert_eq!(last_idx, 1);
        let last_input_epoch = test.metrics[last_idx].0;
        assert_eq!(last_input_epoch, 15);
        let inactive_time = input.0 as i64 - last_input_epoch as i64;
        assert_eq!(inactive_time, -4);
        let target_idx = test.get_tail_negative_offset_idx(inactive_time);
        assert_eq!(test.metrics.len(), 4);
        // This is an erroneous calculation because 11th is too old for little range
        assert_eq!(target_idx, 1);
        // 11th should have been dropped.
        assert_eq!(
            (last_input_epoch as i64 - input.0 as i64) >= test.metrics_capacity as i64,
            true
        );
        test.upsert(input);
        test.upsert((14, Some(4f64)));
        test.upsert((12, Some(20f64)));
        assert_eq!(
            test.metrics,
            vec![
                (14, Some(4f64)),
                (15, Some(5f64)),
                (12, Some(20f64)),
                (13, Some(3f64))
            ]
        );
        assert_eq!(test.first_idx, 2);
        assert_eq!(test.active_items, 4);
        test.upsert((20, None));
        assert_eq!(
            test.metrics,
            vec![
                (20, None),
                (15, Some(5f64)),
                (12, Some(20f64)),
                (13, Some(3f64))
            ]
        );
        test.upsert((20, Some(200f64)));
        assert_eq!(
            test.metrics,
            vec![
                (20, Some(200f64)),
                (15, Some(5f64)),
                (12, Some(20f64)),
                (13, Some(3f64))
            ]
        );
        test.upsert((19, Some(190f64)));
        assert_eq!(
            test.metrics,
            vec![
                (20, Some(200f64)),
                (15, Some(5f64)),
                (12, Some(20f64)),
                (19, Some(190f64))
            ]
        );
        assert_eq!(test.first_idx, 3);
        assert_eq!(test.get_last_idx(), 0);
        assert_eq!(test.active_items, 2);
        assert_eq!(test.as_vec(), vec![(19, Some(190f64)), (20, Some(200f64))]);
        test.upsert((21, Some(210f64)));
        assert_eq!(
            test.metrics,
            vec![
                (20, Some(200f64)),
                (21, Some(210f64)),
                (12, Some(20f64)),
                (19, Some(190f64))
            ]
        );
        assert_eq!(test.first_idx, 3);
        assert_eq!(test.get_last_idx(), 1);
        assert_eq!(test.active_items, 3);
        test.upsert((22, Some(220f64)));
        assert_eq!(
            test.metrics,
            vec![
                (20, Some(200f64)),
                (21, Some(210f64)),
                (22, Some(220f64)),
                (19, Some(190f64))
            ]
        );
        assert_eq!(test.first_idx, 3);
        assert_eq!(test.get_last_idx(), 2);
        assert_eq!(test.active_items, 4);
        test.upsert((24, Some(240f64)));
        assert_eq!(
            test.metrics,
            vec![
                (24, Some(240f64)),
                (21, Some(210f64)),
                (22, Some(220f64)),
                (23, None),
            ]
        );
        assert_eq!(test.first_idx, 1);
        assert_eq!(test.get_last_idx(), 0);
        test.upsert((84, Some(840f64)));
        test.upsert((81, Some(810f64)));
        test.upsert((82, Some(820f64)));
        assert_eq!(
            test.metrics,
            vec![
                (84, Some(840f64)),
                (81, Some(810f64)),
                (82, Some(820f64)),
                (83, None),
            ]
        );
        assert_eq!(test.first_idx, 1);
        assert_eq!(test.active_items, 4);
        // Let's try with broader vectors
        let mut test = TimeSeries::default().with_capacity(10);
        test.upsert((1, Some(1f64)));
        test.upsert((2, Some(2f64)));
        test.upsert((3, Some(3f64)));
        test.upsert((4, Some(4f64)));
        test.upsert((5, Some(5f64)));
        test.upsert((6, Some(6f64)));
        test.upsert((7, Some(7f64)));
        test.upsert((8, Some(8f64)));
        test.upsert((9, Some(9f64)));
        test.upsert((10, Some(10f64)));
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.get_last_idx(), 9);
        assert_eq!(
            test.metrics,
            vec![
                (1, Some(1f64)),
                (2, Some(2f64)),
                (3, Some(3f64)),
                (4, Some(4f64)),
                (5, Some(5f64)),
                (6, Some(6f64)),
                (7, Some(7f64)),
                (8, Some(8f64)),
                (9, Some(9f64)),
                (10, Some(10f64)),
            ]
        );
        test.upsert((11, Some(11f64)));
        assert_eq!(test.first_idx, 1);
        assert_eq!(test.get_last_idx(), 0);
        assert_eq!(
            test.metrics,
            vec![
                (11, Some(11f64)),
                (2, Some(2f64)),
                (3, Some(3f64)),
                (4, Some(4f64)),
                (5, Some(5f64)),
                (6, Some(6f64)),
                (7, Some(7f64)),
                (8, Some(8f64)),
                (9, Some(9f64)),
                (10, Some(10f64)),
            ]
        );
        test.upsert((84, Some(840f64)));
        test.upsert((80, Some(800f64)));
        assert_eq!(
            test.metrics,
            vec![
                (84, Some(840f64)),
                (2, Some(2f64)),
                (3, Some(3f64)),
                (4, Some(4f64)),
                (5, Some(5f64)),
                (6, Some(6f64)),
                (80, Some(800f64)),
                (81, None),
                (82, None),
                (83, None),
            ]
        );
        test.upsert((79, Some(790f64)));
        test.upsert((81, Some(810f64)));
        test.upsert((85, Some(850f64)));
        test.upsert((81, Some(811f64)));
        assert_eq!(
            test.metrics,
            vec![
                (84, Some(840f64)),
                (85, Some(850f64)),
                (3, Some(3f64)),
                (4, Some(4f64)),
                (5, Some(5f64)),
                (79, Some(790f64)),
                (80, Some(800f64)),
                (81, Some(1621f64)), // 81 has been added twice
                (82, None),
                (83, None),
            ]
        );
    }

    #[test]
    fn it_uses_last_idx() {
        let mut test = TimeSeries::default().with_capacity(5);
        test.upsert((0, Some(0f64)));
        assert_eq!(test.get_last_idx(), 0);
        test.upsert((1, Some(1f64)));
        assert_eq!(test.get_last_idx(), 1);
        test.upsert((2, Some(2f64)));
        assert_eq!(test.get_last_idx(), 2);
        test.upsert((3, Some(3f64)));
        assert_eq!(test.get_last_idx(), 3);
        test.upsert((4, Some(4f64)));
        assert_eq!(test.get_last_idx(), 4);
        assert_eq!(
            test.metrics,
            vec![
                (0, Some(0f64)),
                (1, Some(1f64)),
                (2, Some(2f64)),
                (3, Some(3f64)),
                (4, Some(4f64))
            ]
        );
        test.upsert((5, Some(5f64)));
        assert_eq!(test.get_last_idx(), 0);
        assert_eq!(
            test.metrics,
            vec![
                (5, Some(5f64)),
                (1, Some(1f64)),
                (2, Some(2f64)),
                (3, Some(3f64)),
                (4, Some(4f64))
            ]
        );
        test.upsert((6, Some(6f64)));
        assert_eq!(test.get_last_idx(), 1);
        test.upsert((7, Some(7f64)));
        assert_eq!(test.get_last_idx(), 2);
        assert_eq!(test.metrics_capacity, 5);
        let last_input = test.metrics[test.get_last_idx()];
        let old_input = (2, Some(20f64));
        assert_eq!(last_input.0 as i64 - old_input.0 as i64, 5i64);
        test.upsert((2, Some(20f64)));
        assert_eq!(
            test.metrics,
            vec![
                (5, Some(5f64)),
                (6, Some(6f64)),
                (7, Some(7f64)),
                (3, Some(3f64)),
                (4, Some(4f64))
            ]
        );
        // This shouldn't even be inserted because it's too old
        assert_eq!(test.active_items, 5);
        let input = (4, Some(40f64));
        let last_idx = test.get_last_idx();
        let inactive_time = input.0 as i64 - test.metrics[last_idx].0 as i64;
        assert_eq!(inactive_time, -3);
        let target_idx = test.get_tail_negative_offset_idx(inactive_time);
        assert_eq!(target_idx, 4);
        assert_eq!(test.metrics[target_idx].0, 4);
    }

    #[test]
    fn it_gets_last_filled_value() {
        let mut test = TimeSeries::default().with_capacity(4);
        // Some values should be inserted as None
        test.upsert((10, Some(0f64)));
        test.upsert((11, None));
        test.upsert((12, None));
        test.upsert((13, None));
        assert_eq!(test.get_last_filled(), 0f64);
        let mut test = TimeSeries::default().with_capacity(4);
        test.upsert((11, None));
        test.upsert((12, Some(2f64)));
        assert_eq!(test.get_last_filled(), 2f64);
    }

    #[test]
    fn it_transforms_to_flat_vec() {
        let mut test = TimeSeries::default().with_capacity(4);
        // Some values should be inserted as None
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 0);
        test.upsert((10, Some(0f64)));
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 1);
        test.upsert((13, Some(3f64)));
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 4);
        assert_eq!(
            test.as_vec(),
            vec![(10, Some(0f64)), (11, None), (12, None), (13, Some(3f64))]
        );
        test.upsert((14, Some(4f64)));
        // Starting at 11
        test.first_idx = 1;
        assert_eq!(
            test.as_vec(),
            vec![(11, None), (12, None), (13, Some(3f64)), (14, Some(4f64))]
        );
        // Only 11
        test.active_items = 1;
        test.first_idx = 1;
        assert_eq!(test.as_vec(), vec![(11, None)]);
        // Only 13
        test.first_idx = 3;
        test.active_items = 1;
        assert_eq!(test.as_vec(), vec![(13, Some(3f64))]);
        // 13, 14
        test.first_idx = 3;
        test.active_items = 2;
        assert_eq!(test.as_vec(), vec![(13, Some(3f64)), (14, Some(4f64))]);
    }

    #[test]
    fn it_fills_empty_epochs() {
        init_log();
        let mut test = TimeSeries::default().with_capacity(4);
        // Some values should be inserted as None
        test.upsert((10, Some(0f64)));
        test.upsert((13, Some(3f64)));
        assert_eq!(
            test.metrics,
            vec![(10, Some(0f64)), (11, None), (12, None), (13, Some(3f64))]
        );
        assert_eq!(test.active_items, 4);
        // Test the whole vector is discarded
        test.upsert((18, Some(8f64)));
        assert_eq!(test.active_items, 1);
        assert_eq!(
            test.metrics,
            vec![(18, Some(8f64)), (11, None), (12, None), (13, Some(3f64))]
        );
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 1);
        assert_eq!(test.as_vec(), vec![(18, Some(8f64))]);
        test.upsert((20, Some(0f64)));
        assert_eq!(
            test.metrics,
            vec![
                (18, Some(8f64)),
                (19, None),
                (20, Some(0f64)),
                (13, Some(3f64))
            ]
        );
        assert_eq!(test.first_idx, 0);
        assert_eq!(test.active_items, 3);
        assert_eq!(
            test.as_vec(),
            vec![(18, Some(8f64)), (19, None), (20, Some(0f64))]
        );
        test.upsert((50, Some(5f64)));
        assert_eq!(
            test.metrics,
            // Many outdated entries
            vec![
                (50, Some(5f64)),
                (19, None),
                (20, Some(0f64)),
                (13, Some(3f64))
            ]
        );
        assert_eq!(test.as_vec(), vec![(50, Some(5f64))]);
        test.upsert((53, Some(3f64)));
        assert_eq!(
            test.metrics,
            vec![(50, Some(5f64)), (51, None), (52, None), (53, Some(3f64))]
        );
        //  Ensure we can overwrite previous entries
        test.upsert((50, Some(3f64)));
        test.upsert((51, Some(3f64)));
        test.upsert((52, Some(3f64)));
        assert_eq!(
            test.metrics,
            vec![
                (50, Some(8f64)),
                (51, Some(3f64)),
                (52, Some(3f64)),
                (53, Some(3f64))
            ]
        );
    }

    #[test]
    fn it_applies_missing_policies() {
        let mut test_zero = TimeSeries::default().with_capacity(5);
        let mut test_one = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("one".to_string());
        let mut test_min = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("min".to_string());
        let mut test_max = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("max".to_string());
        let mut test_last = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("last".to_string());
        let mut test_first = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("first".to_string());
        let mut test_avg = TimeSeries::default()
            .with_capacity(5)
            .with_missing_values_policy("avg".to_string());
        test_zero.upsert((0, Some(9f64)));
        test_zero.upsert((2, Some(1f64)));
        test_one.upsert((0, Some(9f64)));
        test_one.upsert((2, Some(1f64)));
        test_min.upsert((0, Some(9f64)));
        test_min.upsert((2, Some(1f64)));
        test_max.upsert((0, Some(9f64)));
        test_max.upsert((2, Some(1f64)));
        test_last.upsert((0, Some(9f64)));
        test_last.upsert((2, Some(1f64)));
        test_first.upsert((0, Some(9f64)));
        test_first.upsert((2, Some(1f64)));
        test_avg.upsert((0, Some(9f64)));
        test_avg.upsert((2, Some(1f64)));
        test_zero.calculate_stats();
        test_one.calculate_stats();
        test_min.calculate_stats();
        test_max.calculate_stats();
        test_last.calculate_stats();
        test_first.calculate_stats();
        test_avg.calculate_stats();
        assert_eq!(test_zero.get_missing_values_fill(), 0f64);
        assert_eq!(test_one.get_missing_values_fill(), 1f64);
        assert_eq!(test_min.get_missing_values_fill(), 1f64);
        assert_eq!(test_max.get_missing_values_fill(), 9f64);
        assert_eq!(test_last.get_missing_values_fill(), 1f64);
        assert_eq!(test_first.get_missing_values_fill(), 9f64);
        assert_eq!(test_avg.get_missing_values_fill(), 5f64);
        // TODO: add Fixed value test
    }

    #[test]
    fn it_gets_deduped_opengl_vecs() {
        let size_test = SizeInfo {
            padding_x: 0.,
            padding_y: 0.,
            height: 200., // Display Height: 200px test
            width: 200.,  // Display Width: 200px test
            ..SizeInfo::default()
        };
        let mut all_dups = TimeSeriesChart::default();
        all_dups.sources.push(TimeSeriesSource::default());
        all_dups.dimensions = Some(Value2D { x: 10., y: 10. });
        // Test with 10 items only
        // So that every item takes 0.01
        all_dups.sources[0].series_mut().metrics_capacity = 10;
        all_dups.sources[0].series_mut().upsert((10, Some(5f64)));
        all_dups.sources[0].series_mut().upsert((11, Some(5f64)));
        all_dups.sources[0].series_mut().upsert((12, Some(5f64)));
        all_dups.sources[0].series_mut().upsert((13, Some(5f64)));
        all_dups.sources[0].series_mut().upsert((14, Some(5f64)));
        all_dups.sources[0].series_mut().upsert((15, Some(5f64)));
        all_dups.update_series_opengl_vecs(0, size_test);
        // we expect a line from X -1.0 to X: -0.95
        assert_eq!(all_dups.get_deduped_opengl_vecs(0).len(), 4);
        let mut no_dups = TimeSeriesChart::default();
        no_dups.sources.push(TimeSeriesSource::default());
        no_dups.dimensions = Some(Value2D { x: 10., y: 10. });
        // Test with 10 items only
        // So that every item takes 0.01
        no_dups.sources[0].series_mut().metrics_capacity = 10;
        no_dups.sources[0].series_mut().upsert((10, Some(5f64)));
        no_dups.sources[0].series_mut().upsert((11, Some(9f64)));
        no_dups.sources[0].series_mut().upsert((12, Some(7f64)));
        no_dups.sources[0].series_mut().upsert((13, Some(9f64)));
        no_dups.sources[0].series_mut().upsert((14, Some(5f64)));
        no_dups.sources[0].series_mut().upsert((15, Some(7f64)));
        no_dups.update_series_opengl_vecs(0, size_test);
        // we expect a line from 1, 1->2, 3, 4, 5, 6
        assert_eq!(no_dups.get_deduped_opengl_vecs(0).len(), 14usize);
    }

    #[test]
    fn it_adds_old_items() {
        init_log();
        let mut test0: TimeSeries = TimeSeries::default().with_capacity(10usize);
        // Assume something sets a value in the present.
        // And then we get records for items in the past.
        assert_eq!(test0.upsert((22, Some(22.))), 1usize);
        assert_eq!(test0.metrics[0], (22, Some(22.)));
        assert_eq!(test0.as_vec(), vec![(22, Some(22.))]);
        assert_eq!(test0.first_idx, 0usize);
        assert_eq!(test0.upsert((21, Some(21.))), 1usize);
        assert_eq!(test0.metrics[0], (21, Some(21.)));
        assert_eq!(test0.metrics[1], (22, Some(22.)));
        assert_eq!(test0.first_idx, 0usize);
        assert_eq!(test0.as_vec(), vec![(21, Some(21.)), (22, Some(22.))]);
        // This value is too old and should be discarded.
        assert_eq!(test0.upsert((11, None)), 0usize);
        assert_eq!(test0.as_vec(), vec![(21, Some(21.)), (22, Some(22.))]);
        // This value should be the new item[0]
        assert_eq!(test0.upsert((13, Some(13.))), 8usize);
        assert_eq!(test0.first_idx, 0usize);
        assert_eq!(test0.metrics[0], (13, Some(13.)));
        assert_eq!(test0.metrics[1], (14, None));
        assert_eq!(
            test0.as_vec(),
            vec![
                (13, Some(13.)),
                (14, None),
                (15, None),
                (16, None),
                (17, None),
                (18, None),
                (19, None),
                (20, None),
                (21, Some(21.)),
                (22, Some(22.)),
            ]
        );
    }

    #[test]
    fn it_iterates_trait() {
        // Iterator Trait
        // Test an empty TimeSeries vec
        let test0: TimeSeries = TimeSeries::default().with_capacity(4);
        let mut iter_test0 = test0.iter();
        assert_eq!(iter_test0.pos, 0);
        assert!(iter_test0.next().is_none());
        assert!(iter_test0.next().is_none());
        assert_eq!(iter_test0.pos, 0);
        // Simple test with one item
        let mut test1 = TimeSeries::default().with_capacity(4);
        test1.upsert((10, Some(0f64)));
        let mut iter_test1 = test1.iter();
        assert_eq!(iter_test1.next(), Some(&(10, Some(0f64))));
        assert_eq!(iter_test1.pos, 1);
        assert!(iter_test1.next().is_none());
        assert!(iter_test1.next().is_none());
        assert_eq!(iter_test1.pos, 1);
        // Simple test with 3 items, rotated to start first item and 2nd
        // position and last item at 3rd position
        let mut test2 = TimeSeries::default().with_capacity(4);
        test2.upsert((10, Some(0f64)));
        test2.upsert((11, Some(1f64)));
        test2.upsert((12, Some(2f64)));
        test2.upsert((13, Some(3f64)));
        test2.first_idx = 1;
        assert_eq!(
            test2.metrics,
            vec![
                (10, Some(0f64)),
                (11, Some(1f64)),
                (12, Some(2f64)),
                (13, Some(3f64))
            ]
        );
        let mut iter_test2 = test2.iter();
        assert_eq!(iter_test2.pos, 1);
        assert_eq!(iter_test2.next(), Some(&(11, Some(1f64))));
        assert_eq!(iter_test2.next(), Some(&(12, Some(2f64))));
        assert_eq!(iter_test2.pos, 3);
        // A vec that is completely full
        let mut test3 = TimeSeries::default().with_capacity(4);
        test3.upsert((10, Some(0f64)));
        test3.upsert((11, Some(1f64)));
        test3.upsert((12, Some(2f64)));
        test3.upsert((13, Some(3f64)));
        {
            let mut iter_test3 = test3.iter();
            assert_eq!(iter_test3.next(), Some(&(10, Some(0f64))));
            assert_eq!(iter_test3.next(), Some(&(11, Some(1f64))));
            assert_eq!(iter_test3.next(), Some(&(12, Some(2f64))));
            assert_eq!(iter_test3.next(), Some(&(13, Some(3f64))));
            assert!(iter_test3.next().is_none());
            assert!(iter_test3.next().is_none());
            assert_eq!(iter_test2.pos, 3);
        }
        // After changing the data the idx is recreatehd at 11 as expected
        test3.upsert((14, Some(4f64)));
        let mut iter_test3 = test3.iter();
        assert_eq!(iter_test3.next(), Some(&(11, Some(1f64))));
    }

    #[test]
    fn it_scales_x_to_display_size() {
        let mut test = SizeInfo {
            padding_x: 0.,
            padding_y: 0.,
            height: 100.,
            width: 100.,
            ..SizeInfo::default()
        };
        // display size: 100 px, input the value: 0, padding_x: 0
        // The value should return should be left-most: -1.0
        let min = test.scale_x(0f32);
        assert_eq!(min, -1.0f32);
        // display size: 100 px, input the value: 100, padding_x: 0
        // The value should return should be right-most: 1.0
        let max = test.scale_x(100f32);
        assert_eq!(max, 1.0f32);
        // display size: 100 px, input the value: 50, padding_x: 0
        // The value should return should be the center: 0.0
        let mid = test.scale_x(50f32);
        assert_eq!(mid, 0.0f32);
        test.padding_x = 50.;
        // display size: 100 px, input the value: 50, padding_x: 50px
        // The value returned should be the right-most: 1.0
        let mid = test.scale_x(50f32);
        assert_eq!(mid, 1.0f32);
    }

    #[test]
    fn it_scales_y_to_display_size() {
        let mut size_test = SizeInfo {
            padding_x: 0.,
            padding_y: 0.,
            height: 100.,
            chart_height: 100.,
            ..SizeInfo::default()
        };
        let mut chart_test = TimeSeriesChart::default();
        // To make testing easy, let's make three values equivalent:
        // - Chart height
        // - Max Metric collected
        // - Max resolution in pixels
        chart_test.stats.max = 100f64;
        // display size: 100 px, input the value: 0, padding_y: 0
        // The value should return should be lowest: -1.0
        let min = size_test.scale_y(100f64, 0f64);
        assert_eq!(min, -1.0f32);
        // display size: 100 px, input the value: 100, padding_y: 0
        // The value should return should be upper-most: 1.0
        let max = size_test.scale_y(100f64, 100f64);
        assert_eq!(max, 1.0f32);
        // display size: 100 px, input the value: 50, padding_y: 0
        // The value should return should be the center: 0.0
        let mid = size_test.scale_y(100f64, 50f64);
        assert_eq!(mid, 0.0f32);
        size_test.padding_y = 25.;
        // display size: 100 px, input the value: 50, padding_y: 25
        // The value returned should be upper-most: 1.0
        // In this case, the chart (100px) is bigger than the display,
        // which means some values would have been chopped (anything above
        // 50f32)
        let mid = size_test.scale_y(100f64, 50f64);
        assert_eq!(mid, 1.0f32);
    }

    fn simple_chart_setup_with_none() -> (SizeInfo, TimeSeriesChart) {
        init_log();
        let size_test = SizeInfo {
            padding_x: 0.,
            padding_y: 0.,
            height: 200., // Display Height: 200px test
            width: 200.,  // Display Width: 200px test
            ..SizeInfo::default()
        };
        let mut chart_test = TimeSeriesChart::default();
        chart_test.sources.push(TimeSeriesSource::default());
        chart_test.dimensions = Some(Value2D { x: 10., y: 10. });
        // Test with 5 items only
        // So that every item takes 0.01
        chart_test.sources[0].series_mut().metrics_capacity = 10;
        // |             |   -
        // |             |   |
        // |             |   200
        // |             |   |
        // |XX           |   -
        //
        // |---- 200 ----|
        chart_test.sources[0].series_mut().upsert((10, Some(0f64)));
        chart_test.sources[0].series_mut().upsert((11, Some(1f64)));
        chart_test.sources[0].series_mut().upsert((12, Some(2f64)));
        // A metric with None will be added for the (13, None)
        // Let's make a None value and check the MissingValuesPolicy
        chart_test.sources[0].series_mut().upsert((14, None));
        // This makes the top value 4
        chart_test.sources[0].series_mut().upsert((15, Some(4f64)));
        (size_test, chart_test)
    }

    #[test]
    fn it_updates_opengl_vertices() {
        init_log();
        let (size_test, mut chart_test) = simple_chart_setup_with_none();
        chart_test.update_series_opengl_vecs(0, size_test);
        assert_eq!(
            chart_test.opengl_vecs[0],
            vec![
                -1.0,   // 1st X value, leftmost.
                -1.0,   // Y value is 0, so -1.0 is the bottom-most
                -0.99,  // X plus 0.01
                -0.975, // Y value is 1, so 25% of the line, so 0.025
                -0.98,  // leftmost plus  0.01 * 2
                -0.95,  // Y value is 2, so 50% from bottom to top
                -0.97,  // leftmost plus 0.01 * 3
                -1.0,   // Top-most value, so the chart height
                -0.96,  // leftmost plus 0.01 * 4, rightmost
                -1.0,   // A None value is set
                -0.95,  // leftmost plus 0.01 * 5, rightmost
                -0.9    // Top-most value, so the chart height
            ]
        );
        let mut prom_test = TimeSeriesChart::default();
        // Let's add a reference point
        // XXX: How does this behave without a reference point?
        prom_test
            .decorations
            .push(Decoration::Reference(ReferencePointDecoration::default()));
        prom_test.sources.push(TimeSeriesSource::default());
        prom_test.dimensions = Some(Value2D { x: 12., y: 10. });
        prom_test.sources[0].series_mut().metrics_capacity = 24;
        let point_1_metric = 4.5f64;
        let point_2_metric = 4.25f64;
        let point_3_metric = 4.0f64;
        let point_4_metric = 4.75f64;
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918913, Some(point_1_metric))); // Point 1
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918914, Some(point_1_metric))); //  |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918915, Some(point_1_metric))); //  |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918916, Some(point_1_metric))); //  |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918917, Some(point_1_metric))); //  |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918918, Some(point_1_metric))); //  |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918919, Some(point_2_metric))); // Point 2 -> Point 3
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918920, Some(point_2_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918921, Some(point_2_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918922, Some(point_2_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918923, Some(point_2_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918924, Some(point_2_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918925, Some(point_3_metric))); // Point 4 -> Point 5
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918926, Some(point_3_metric))); //   |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918927, Some(point_3_metric))); //   |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918928, Some(point_3_metric))); //   |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918929, Some(point_3_metric))); //   |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918930, Some(point_3_metric))); //   |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918931, Some(point_4_metric))); // Point 6 -> Point 7
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918932, Some(point_4_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918933, Some(point_4_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918934, Some(point_4_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918935, Some(point_4_metric))); // |
        prom_test.sources[0]
            .series_mut()
            .upsert((1566918936, Some(point_4_metric))); // Point 8
        prom_test.update_all_series_opengl_vecs(size_test);
        // We expect to see these dedupped vertices:
        // |              7--8  |   -     metric value: 4.75, point 4
        // |  1---2       |     |   |                   4.5, point 1
        // |      3---4   |     |   |                   4.25, point 2
        // |          5---6     |   |                   4., point 3
        // |                    |   |
        // |                    |   10px
        // |                    |   |
        // |                    |   |
        // |                    |   |
        // | __________________ |   |  <- reference point, metric value 1.0
        // |                    |   -
        //
        // Each point in the above should be a point returned by dedupped
        // |------- 12px -------|
        // - The middle of the drawing board, 0,0 is X=100 and Y=100 in pixels
        let deduped_opengl_vecs = prom_test.get_deduped_opengl_vecs(0);
        assert_eq!(deduped_opengl_vecs.len(), 16);

        //
        // - The reference point takes 1px width, so draw space for metrics is 10px.
        assert_eq!(prom_test.decorations[0].width(), 2.);
        let tick_space = 0.10f32 / 24f32;
        // The draw space horizontally is 0.10. from 0.99 to 0.90
        // Start of the line:
        assert!((deduped_opengl_vecs[0] - (-0.99f32 + 0f32 * tick_space)).abs() < 0.001f32); // Point 1, 1st item
                                                                                             // Horizontal line Point 1 to Point 2
        assert!((deduped_opengl_vecs[2] - (-0.99f32 + 6f32 * tick_space)).abs() < 0.001f32); // Point 2, 6th item
                                                                                             // Vertical line Point 2 to Point 3
        assert!((deduped_opengl_vecs[4] - (-0.99f32 + 6f32 * tick_space)).abs() < 0.001f32); // Point 3, 6th item
                                                                                             // Horizontal line Point 3 to Point 4
        assert!((deduped_opengl_vecs[6] - (-0.99f32 + 12f32 * tick_space)).abs() < 0.001f32); // Point 4, 12th item
                                                                                              // Vertical line Point 4 to Point 5
        assert!((deduped_opengl_vecs[8] - (-0.99f32 + 12f32 * tick_space)).abs() < 0.001f32); // Point 4, 12th item
                                                                                              // Horizontal line Point 5 to Point 6
        assert!((deduped_opengl_vecs[10] - (-0.99f32 + 18f32 * tick_space)).abs() < 0.001f32); // Point 4, 12th item
                                                                                               // Vertical line Point 6 to Point 7
        assert!((deduped_opengl_vecs[12] - (-0.99f32 + 18f32 * tick_space)).abs() < 0.001f32); // 4 X value, rightmost.
                                                                                               // Horizontal line Point 7 to Point 8
        assert!((deduped_opengl_vecs[14] - (-0.99f32 + 23f32 * tick_space)).abs() < 0.001f32); // 4 X value, rightmost.
                                                                                               // XXX: Shouldn't the above test be 24f32 ?

        // Y values
        let max_y_metric = 4.75f32;
        let chart_top_y = 0.10f32;
        let bottom_y = -1.0f32;
        assert!(
            (deduped_opengl_vecs[1]
                - bottom_y
                - (point_1_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[3]
                - bottom_y
                - (point_1_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[5]
                - bottom_y
                - (point_2_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[7]
                - bottom_y
                - (point_2_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[9]
                - bottom_y
                - (point_3_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[11]
                - bottom_y
                - (point_3_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[13]
                - bottom_y
                - (point_4_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
        assert!(
            (deduped_opengl_vecs[15]
                - bottom_y
                - (point_4_metric as f32 * chart_top_y) / max_y_metric)
                .abs()
                < 0.001f32
        ); // top Y value, 4.75
    }

    #[test]
    fn it_calculates_reference_point() {
        init_log();
        let (size_test, mut chart_test) = simple_chart_setup_with_none();
        chart_test
            .decorations
            .push(Decoration::Reference(ReferencePointDecoration::default()));
        // Calling update_series_opengl_vecs also calls the decoration update opengl vecs
        chart_test.update_series_opengl_vecs(0, size_test);
        let deco_vecs = chart_test.decorations[0].opengl_vertices();

        assert_eq!(chart_test.decorations[0].opengl_vertices().len(), 12);
        // At this point we know 1 unit in the current drawn metrics is equals to
        // 0.025
        assert_eq!(
            deco_vecs,
            vec![
                -1.0,     // Left-most
                -0.97375, // 0.25 + 5% height multiplier, so 30% of the line
                -1.0,     // Left-most
                -0.97625, // Y value - 5% height multiplier, so 20% of the line
                -1.0,     // Left-most
                -0.975,   // Y value, so 25% of the line
                -0.9,     // Right-most
                -0.975,   // Y value is 1, so 25% of the line
                -0.9,     // Right-most
                -0.97625, // Y value is 1, so 25% of the line
                -0.9,     // Right-most
                -0.97375, // Y value is 1, so 25% of the line
            ]
        );
        // Since we have added a Reference point, it needs some space to be
        // drawn, its default width is 1px, turns out to be 0.9 between ticks
        // Also there is an offset of 10 px so divided by 2 (for each side) becomes:
        // 0.05
        assert_eq!(
            chart_test.opengl_vecs[0],
            vec![
                -0.99,       // 1st X value, leftmost.
                -1.0,        // Y value is 0, so -1.0 is the bottom-most
                -0.982,      // X plus 0.01
                -0.975,      // Y value is 1, so 25% of the line, so 0.025
                -0.97400004, // leftmost plus  0.01 * 2
                -0.95,       // Y value is 2, so 50% from bottom to top
                -0.96599996, // leftmost plus 0.01 * 0.3
                -1.0,        // A none value means MissingValuesPolicy::Zero
                -0.958,      // leftmost plus 0.01 * 4
                -1.0,        // A none value means MissingValuesPolicy::Zero
                -0.95,       // leftmost plus 0.01 * 5, rightmost
                -0.9         // A bit below the max
            ]
        );
    }

    #[test]
    fn it_spaces_chart_config_dimensions_and_position() {
        init_log();
        let mut chart_config = ChartsConfig {
            default_dimensions: Some(Value2D { x: 25., y: 100. }),
            position: Some(Value2D { x: 200., y: 0. }),
            charts: vec![],
        };
        let (_size_test, mut chart_test) = simple_chart_setup_with_none();
        chart_test.position = None;
        chart_test.dimensions = None;
        chart_config.charts.push(chart_test.clone());
        chart_config.charts.push(chart_test.clone());
        chart_config.charts.push(chart_test.clone());
        chart_config.charts.push(chart_test.clone());
        chart_config.charts.push(chart_test.clone());
        chart_config.charts.push(chart_test.clone());
        chart_config.setup_chart_spacing();
        assert_eq!(
            chart_config.charts[0].dimensions,
            chart_config.default_dimensions
        );
        assert_eq!(
            chart_config.charts[0].position,
            Some(Value2D { x: 200., y: 0. })
        );
        assert_eq!(
            chart_config.charts[1].position,
            Some(Value2D { x: 225., y: 0. })
        );
        assert_eq!(
            chart_config.charts[2].position,
            Some(Value2D { x: 250., y: 0. })
        );
        assert_eq!(
            chart_config.charts[3].position,
            Some(Value2D { x: 275., y: 0. })
        );
        assert_eq!(
            chart_config.charts[4].position,
            Some(Value2D { x: 300., y: 0. })
        );
        assert_eq!(
            chart_config.charts[5].position,
            Some(Value2D { x: 325., y: 0. })
        );
        assert_eq!(
            chart_config.charts[5].dimensions,
            chart_config.default_dimensions
        );
    }
}
