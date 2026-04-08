use crate::plot_data::{AxisSide, Measure, PlotData};
use anyhow::{anyhow, bail, Context, Result};
use chrono::prelude::*;
use clap::{CommandFactory, Parser};
use crossterm::event::KeyModifiers;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{
    event::{self, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, SetSize},
};
use itertools::{Itertools, MinMaxResult};
use serde::Deserialize;
use std::fs;
use std::io::{self, BufWriter, Read};
use std::iter;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};
use std::thread;
use std::thread::{sleep, JoinHandle};
use std::time::Duration;
use tui::backend::{Backend, CrosstermBackend};
use tui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use tui::style::{Color, Style};
use tui::text::Span;
use tui::widgets::{Axis, Block, Borders, Chart, Dataset, Paragraph};
use tui::Terminal;

mod colors;
mod plot_data;

use colors::Colors;
use shadow_rs::{formatcp, shadow};
use tui::prelude::Position;

shadow!(build);

const VERSION_INFO: &str = formatcp!(
    r#"{}
commit_hash: {}
build_time: {}
build_env: {},{}"#,
    build::PKG_VERSION,
    build::SHORT_COMMIT,
    build::BUILD_TIME,
    build::RUST_VERSION,
    build::RUST_CHANNEL
);

const DEFAULT_PERIOD_SECONDS: f32 = 1.0;
const DEFAULT_CMD_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_TITLE_MAX_CHARS: usize = 40;

#[derive(Parser, Debug)]
#[command(author, version=build::PKG_VERSION, name = "gwatch", about = "Watch command output with a graph.", long_version = VERSION_INFO, styles = clap_cargo::style::CLAP_STYLING
)]
struct Args {
    /// Path to TOML config file. Use '-' to read config from stdin.
    #[arg(short = 'f', long, conflicts_with = "watch_specs")]
    file: Option<String>,

    /// Add a watch using mini-DSL: key=value;key=value (exp is required, axis is left|right).
    #[arg(long = "watch", conflicts_with = "file")]
    watch_specs: Vec<String>,

    /// Default watch period in seconds.
    #[arg(short = 'p', long)]
    period: Option<f32>,

    /// Determines the number of seconds to display in the graph.
    #[arg(short, long, default_value = "30")]
    buffer: u64,

    /// Uses dot characters instead of braille
    #[arg(short = 's', long, help = "")]
    simple_graphics: bool,

    /// Vertical margin around the graph (top and bottom)
    #[arg(long, default_value = "1")]
    vertical_margin: u16,

    /// Horizontal margin around the graph (left and right)
    #[arg(long, default_value = "0")]
    horizontal_margin: u16,

    #[arg(
        name = "color",
        short = 'c',
        long = "color",
        use_value_delimiter = true,
        value_delimiter = ',',
        help = r#"Assign color to a graph entry.

This option can be defined more than once as a comma separated string, and the
order which the colors are provided will be matched against the watches passed to gwatch.

Hexadecimal RGB color codes are accepted in the form of '#RRGGBB' or the
following color names: 'black', 'red', 'green', 'yellow', 'blue', 'magenta',
'cyan', 'gray', 'dark-gray', 'light-red', 'light-green', 'light-yellow',
'light-blue', 'light-magenta', 'light-cyan', and 'white'"#
    )]
    color_codes_or_names: Vec<String>,

    /// Clear the graph from the terminal after closing the program
    #[arg(name = "clear", long = "clear", action)]
    clear: bool,
}

#[derive(Debug, Clone)]
struct WatchSpec {
    title: String,
    exp: String,
    period: Duration,
    measure: Measure,
    axis: Option<AxisSide>,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    period: Option<f32>,
    #[serde(default)]
    watch: Vec<FileWatch>,
}

#[derive(Debug, Deserialize)]
struct FileWatch {
    exp: Option<String>,
    title: Option<String>,
    period: Option<f32>,
    measure: Option<String>,
    axis: Option<String>,
}

struct App {
    data: Vec<PlotData>,
    display_interval: chrono::Duration,
    started: chrono::DateTime<Local>,
}

impl App {
    fn new(data: Vec<PlotData>, buffer: u64) -> Self {
        App {
            data,
            display_interval: chrono::Duration::from_std(Duration::from_secs(buffer)).unwrap(),
            started: Local::now(),
        }
    }

    fn update(&mut self, host_idx: usize, item: Option<f64>) {
        let host = &mut self.data[host_idx];
        host.update(item);
    }

    fn normalize_y_bounds(min: f64, max: f64) -> [f64; 2] {
        if (max - min).abs() < f64::EPSILON {
            let pad = if min.abs() < 1.0 {
                1.0
            } else {
                min.abs() * 0.1
            };
            return [min - pad, max + pad];
        }

        let span = max - min;
        let pad = span * 0.1;
        [min - pad, max + pad]
    }

    fn has_axis(&self, axis: AxisSide) -> bool {
        self.data.iter().any(|plot| plot.axis == axis)
    }

    fn y_axis_bounds_for(&self, axis: AxisSide) -> Option<[f64; 2]> {
        if !self.has_axis(axis) {
            return None;
        }

        let (min, max) = match self
            .data
            .iter()
            .filter(|plot| plot.axis == axis)
            .flat_map(|plot| plot.data.as_slice())
            .map(|v| v.1)
            .filter(|v| !v.is_nan())
            .minmax()
        {
            MinMaxResult::NoElements => (0.0, 1.0),
            MinMaxResult::OneElement(value) => (value, value),
            MinMaxResult::MinMax(min, max) => (min, max),
        };

        Some(Self::normalize_y_bounds(min, max))
    }

    fn axis_measure_hint(&self, axis: AxisSide) -> Option<Measure> {
        let mut measures = self
            .data
            .iter()
            .filter(|plot| plot.axis == axis)
            .map(|plot| plot.measure);

        let first = measures.next()?;
        if measures.all(|measure| measure == first) {
            Some(first)
        } else {
            None
        }
    }

    fn x_axis_bounds(&self) -> [f64; 2] {
        let now = Local::now();
        let now_idx;
        let before_idx;
        if (now - self.started) < self.display_interval {
            now_idx = (self.started + self.display_interval).timestamp_millis() as f64 / 1_000f64;
            before_idx = self.started.timestamp_millis() as f64 / 1_000f64;
        } else {
            now_idx = now.timestamp_millis() as f64 / 1_000f64;
            let before = now - self.display_interval;
            before_idx = before.timestamp_millis() as f64 / 1_000f64;
        }

        [before_idx, now_idx]
    }

    fn x_axis_labels(&self, bounds: [f64; 2]) -> Vec<Span<'_>> {
        let lower_utc =
            DateTime::<Utc>::from_timestamp(bounds[0] as i64, 0).unwrap_or_else(Utc::now);
        let upper_utc =
            DateTime::<Utc>::from_timestamp(bounds[1] as i64, 0).unwrap_or_else(Utc::now);
        let lower: DateTime<Local> = DateTime::from(lower_utc);
        let upper: DateTime<Local> = DateTime::from(upper_utc);
        let diff = (upper - lower) / 2;
        let midpoint = lower + diff;
        vec![
            Span::raw(format!("{:?}", lower.time())),
            Span::raw(format!("{:?}", midpoint.time())),
            Span::raw(format!("{:?}", upper.time())),
        ]
    }

    fn y_axis_labels_for(measure: Option<Measure>, bounds: [f64; 2]) -> Vec<Span<'static>> {
        let min = bounds[0];
        let max = bounds[1];
        let num_labels = 7;
        let increment = (max - min) / (num_labels - 1) as f64;
        let formatter = measure.unwrap_or(Measure::Float);

        (0..num_labels)
            .map(|i| Span::raw(formatter.format_value(min + increment * i as f64)))
            .collect()
    }

    fn y_axis_label_strings(measure: Option<Measure>, bounds: [f64; 2]) -> Vec<String> {
        let min = bounds[0];
        let max = bounds[1];
        let num_labels = 7;
        let increment = (max - min) / (num_labels - 1) as f64;
        let formatter = measure.unwrap_or(Measure::Float);

        (0..num_labels)
            .map(|i| formatter.format_value(min + increment * i as f64))
            .collect()
    }
}

fn axis_title(measure: Option<Measure>) -> &'static str {
    match measure {
        Some(measure) => measure.axis_name(),
        None => "mixed",
    }
}

fn remap_value_between_bounds(value: f64, src: [f64; 2], dst: [f64; 2]) -> f64 {
    if value.is_nan() {
        return f64::NAN;
    }

    let src_span = src[1] - src[0];
    if src_span.abs() < f64::EPSILON {
        return (dst[0] + dst[1]) / 2.0;
    }

    let dst_span = dst[1] - dst[0];
    let ratio = (value - src[0]) / src_span;
    dst[0] + ratio * dst_span
}

fn remap_series_to_bounds(data: &[(f64, f64)], src: [f64; 2], dst: [f64; 2]) -> Vec<(f64, f64)> {
    data.iter()
        .map(|(x, y)| (*x, remap_value_between_bounds(*y, src, dst)))
        .collect()
}

fn split_chart_area_for_second_axis(chart_chunk: Rect, dual_axis: bool) -> (Rect, Option<Rect>) {
    if !dual_axis {
        return (chart_chunk, None);
    }

    let areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(13)])
        .split(chart_chunk);
    (areas[0], Some(areas[1]))
}

#[derive(Debug)]
enum Update {
    Value(f64),
    Missing,
}

#[derive(Debug)]
enum Event {
    Update(usize, Update),
    Terminate,
    Render,
}

fn start_render_thread(kill_event: Arc<AtomicBool>, cmd_tx: Sender<Event>) -> JoinHandle<()> {
    thread::spawn(move || {
        while !kill_event.load(Ordering::Acquire) {
            sleep(DEFAULT_CMD_INTERVAL);
            if cmd_tx.send(Event::Render).is_err() {
                break;
            }
        }
    })
}

fn start_watch_thread(
    watch: WatchSpec,
    watch_id: usize,
    cmd_tx: Sender<Event>,
    kill_event: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !kill_event.load(Ordering::Acquire) {
            let update = run_watch_command(&watch.exp);
            if cmd_tx.send(Event::Update(watch_id, update)).is_err() {
                break;
            }
            sleep(watch.period);
        }
    })
}

#[cfg(target_os = "windows")]
fn build_shell_command(exp: &str) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(exp);
    command
}

#[cfg(not(target_os = "windows"))]
fn build_shell_command(exp: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(exp);
    command
}

fn run_watch_command(exp: &str) -> Update {
    let mut command = build_shell_command(exp);

    let output = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(_) => return Update::Missing,
    };

    if !output.status.success() {
        return Update::Missing;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty());

    match first_line.and_then(|line| line.parse::<f64>().ok()) {
        Some(value) => Update::Value(value),
        None => Update::Missing,
    }
}

fn parse_period_seconds(value: f32, field_name: &str) -> Result<f32> {
    if !value.is_finite() || value <= 0.0 {
        bail!("{field_name} must be a positive number")
    }

    Ok(value)
}

fn seconds_to_duration(value: f32, field_name: &str) -> Result<Duration> {
    let seconds = parse_period_seconds(value, field_name)?;
    Ok(Duration::from_secs_f64(seconds as f64))
}

fn normalize_title(title: Option<String>, exp: &str) -> String {
    let maybe_title = title.unwrap_or_default();
    if maybe_title.trim().is_empty() {
        return shorten_title(exp);
    }

    maybe_title.trim().to_string()
}

fn shorten_title(exp: &str) -> String {
    let compact = exp.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "watch".to_string();
    }

    let len = compact.chars().count();
    if len <= DEFAULT_TITLE_MAX_CHARS {
        return compact;
    }

    let short = compact
        .chars()
        .take(DEFAULT_TITLE_MAX_CHARS.saturating_sub(3))
        .collect::<String>();
    format!("{short}...")
}

fn parse_measure(value: Option<&str>) -> Result<Measure> {
    match value {
        Some(raw) => raw
            .parse::<Measure>()
            .map_err(|error| anyhow!(error.to_string())),
        None => Ok(Measure::Float),
    }
}

fn parse_axis(value: Option<&str>) -> Result<Option<AxisSide>> {
    match value {
        Some(raw) => raw
            .parse::<AxisSide>()
            .map(Some)
            .map_err(|error| anyhow!(error.to_string())),
        None => Ok(None),
    }
}

fn resolve_axis_defaults(mut watches: Vec<WatchSpec>) -> Vec<WatchSpec> {
    let mixed_dimensions = watches.iter().any(|watch| watch.measure == Measure::Bytes)
        && watches.iter().any(|watch| watch.measure == Measure::Float);

    for watch in &mut watches {
        if watch.axis.is_none() {
            watch.axis = Some(if mixed_dimensions {
                match watch.measure {
                    Measure::Bytes => AxisSide::Left,
                    Measure::Float => AxisSide::Right,
                }
            } else {
                AxisSide::Left
            });
        }
    }

    watches
}

fn parse_watch_dsl(spec: &str, default_period: f32) -> Result<WatchSpec> {
    let mut exp: Option<String> = None;
    let mut title: Option<String> = None;
    let mut period: Option<f32> = None;
    let mut measure: Option<Measure> = None;
    let mut axis: Option<AxisSide> = None;

    for pair in spec.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }

        let (raw_key, raw_value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("Invalid --watch item '{pair}', expected key=value"))?;

        let key = raw_key.trim().to_ascii_lowercase();
        let value = raw_value.trim();

        match key.as_str() {
            "exp" => {
                if value.is_empty() {
                    bail!("watch exp must not be empty")
                }
                exp = Some(value.to_string());
            }
            "title" => title = Some(value.to_string()),
            "period" => {
                let period_value: f32 = value
                    .parse()
                    .with_context(|| format!("Invalid watch period value '{value}'"))?;
                period = Some(parse_period_seconds(period_value, "watch period")?);
            }
            "measure" => {
                measure = Some(
                    value
                        .parse::<Measure>()
                        .map_err(|error| anyhow!(error.to_string()))?,
                )
            }
            "axis" => {
                axis = Some(
                    value
                        .parse::<AxisSide>()
                        .map_err(|error| anyhow!(error.to_string()))?,
                );
            }
            unknown => bail!("Unknown key '{unknown}' in --watch spec"),
        }
    }

    let exp = exp.ok_or_else(|| anyhow!("`exp` is required in --watch specification"))?;
    let period = seconds_to_duration(period.unwrap_or(default_period), "watch period")?;

    Ok(WatchSpec {
        title: normalize_title(title, &exp),
        exp,
        period,
        measure: measure.unwrap_or(Measure::Float),
        axis,
    })
}

fn parse_file_watch(watch: FileWatch, idx: usize, default_period: f32) -> Result<WatchSpec> {
    let exp = watch
        .exp
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("watch[{idx}] is missing required field `exp`"))?;

    let period = seconds_to_duration(
        watch.period.unwrap_or(default_period),
        &format!("watch[{idx}].period"),
    )?;

    let measure = parse_measure(watch.measure.as_deref())
        .with_context(|| format!("watch[{idx}].measure is invalid"))?;
    let axis = parse_axis(watch.axis.as_deref())
        .with_context(|| format!("watch[{idx}].axis is invalid"))?;

    Ok(WatchSpec {
        title: normalize_title(watch.title, &exp),
        exp,
        period,
        measure,
        axis,
    })
}

fn parse_toml_config(raw: &str, cli_period: Option<f32>) -> Result<Vec<WatchSpec>> {
    let config: FileConfig = toml::from_str(raw).context("Error parsing TOML config")?;

    if config.watch.is_empty() {
        bail!("Config must include at least one [[watch]] block")
    }

    let global_period = parse_period_seconds(
        cli_period.unwrap_or(config.period.unwrap_or(DEFAULT_PERIOD_SECONDS)),
        "period",
    )?;

    config
        .watch
        .into_iter()
        .enumerate()
        .map(|(idx, watch)| parse_file_watch(watch, idx, global_period))
        .collect()
}

fn load_watch_specs(args: &Args) -> Result<Vec<WatchSpec>> {
    let watches = match (&args.file, args.watch_specs.is_empty()) {
        (Some(path), true) => {
            let raw = if path == "-" {
                let mut input = String::new();
                io::stdin()
                    .read_to_string(&mut input)
                    .context("Error reading TOML config from stdin")?;
                input
            } else {
                fs::read_to_string(path)
                    .with_context(|| format!("Error reading config file '{path}'"))?
            };
            parse_toml_config(&raw, args.period)
        }
        (None, false) => {
            let default_period =
                parse_period_seconds(args.period.unwrap_or(DEFAULT_PERIOD_SECONDS), "period")?;

            args.watch_specs
                .iter()
                .map(|spec| parse_watch_dsl(spec, default_period))
                .collect()
        }
        (Some(_), false) => bail!("Cannot combine --file and --watch"),
        (None, true) => {
            bail!("No watches provided. Use --file <path> or at least one --watch spec")
        }
    }?;

    Ok(resolve_axis_defaults(watches))
}

fn generate_man_page(path: &Path) -> anyhow::Result<()> {
    let man = clap_mangen::Man::new(Args::command().version(None).long_version(None));
    let mut buffer: Vec<u8> = Default::default();
    man.render(&mut buffer)?;

    std::fs::write(path, buffer)?;
    Ok(())
}

fn main() -> Result<()> {
    if let Some(path) = std::env::var_os("GENERATE_MANPAGE") {
        return generate_man_page(Path::new(&path));
    };

    let args: Args = Args::parse();
    let watch_specs = load_watch_specs(&args)?;

    let mut data = vec![];
    let colors = Colors::from(args.color_codes_or_names.iter());

    for (watch, color) in watch_specs.iter().zip(colors) {
        let color = color?;
        data.push(PlotData::new(
            watch.title.clone(),
            args.buffer,
            Style::default().fg(color),
            args.simple_graphics,
            watch.measure,
            watch
                .axis
                .expect("Axis should be resolved by load_watch_specs"),
        ));
    }

    let (event_tx, event_rx) = mpsc::channel();
    let killed = Arc::new(AtomicBool::new(false));

    let _watch_threads: Vec<_> = watch_specs
        .into_iter()
        .enumerate()
        .map(|(watch_id, watch)| {
            start_watch_thread(
                watch,
                watch_id,
                event_tx.clone(),
                std::sync::Arc::clone(&killed),
            )
        })
        .collect();

    let _render_thread = start_render_thread(std::sync::Arc::clone(&killed), event_tx.clone());

    let mut app = App::new(data, args.buffer);

    enable_raw_mode()?;
    let stdout = io::stdout();
    let mut backend = CrosstermBackend::new(BufWriter::with_capacity(1024 * 1024 * 4, stdout));
    let rect = backend.size()?;

    if args.clear {
        execute!(
            backend,
            SetSize(rect.width, rect.height),
            EnterAlternateScreen,
        )?;
    } else {
        execute!(backend, SetSize(rect.width, rect.height),)?;
    }

    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let killed_thread = std::sync::Arc::clone(&killed);
    thread::spawn(move || {
        while !killed_thread.load(Ordering::Acquire) {
            match event::poll(Duration::from_secs(5)) {
                Ok(true) => match event::read() {
                    Ok(CEvent::Key(key)) => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            let _ = event_tx.send(Event::Terminate);
                            break;
                        }
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            let _ = event_tx.send(Event::Terminate);
                            break;
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    loop {
        match event_rx.recv()? {
            Event::Update(watch_id, update) => match update {
                Update::Value(value) => app.update(watch_id, Some(value)),
                Update::Missing => app.update(watch_id, None),
            },
            Event::Render => {
                terminal.draw(|f| {
                    let chunks = Layout::default()
                        .flex(Flex::Legacy)
                        .direction(Direction::Vertical)
                        .vertical_margin(args.vertical_margin)
                        .horizontal_margin(args.horizontal_margin)
                        .constraints(
                            std::iter::repeat_n(Constraint::Length(1), app.data.len())
                                .chain(iter::once(Constraint::Percentage(10)))
                                .collect::<Vec<_>>(),
                        )
                        .split(f.area());

                    let total_chunks = chunks.len();
                    let header_chunks = &chunks[0..total_chunks - 1];
                    let chart_chunk = &chunks[total_chunks - 1];

                    for (plot_data, chunk) in app.data.iter().zip(header_chunks) {
                        let header_layout = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints(
                                [
                                    Constraint::Percentage(30),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                    Constraint::Percentage(10),
                                ]
                                .as_ref(),
                            )
                            .split(*chunk);

                        for (area, paragraph) in header_layout.iter().zip(plot_data.header_stats())
                        {
                            f.render_widget(paragraph, *area);
                        }
                    }

                    let x_axis_bounds = app.x_axis_bounds();
                    let left_bounds = app.y_axis_bounds_for(AxisSide::Left);
                    let right_bounds = app.y_axis_bounds_for(AxisSide::Right);
                    let dual_axis = left_bounds.is_some() && right_bounds.is_some();

                    let chart_axis = if left_bounds.is_some() {
                        AxisSide::Left
                    } else {
                        AxisSide::Right
                    };
                    let chart_bounds = match chart_axis {
                        AxisSide::Left => left_bounds.or(right_bounds).unwrap_or([0.0, 1.0]),
                        AxisSide::Right => right_bounds.or(left_bounds).unwrap_or([0.0, 1.0]),
                    };
                    let secondary_axis = match chart_axis {
                        AxisSide::Left => AxisSide::Right,
                        AxisSide::Right => AxisSide::Left,
                    };
                    let secondary_bounds = match secondary_axis {
                        AxisSide::Left => left_bounds,
                        AxisSide::Right => right_bounds,
                    };
                    let chart_measure_hint = app.axis_measure_hint(chart_axis);
                    let secondary_measure_hint = app.axis_measure_hint(secondary_axis);

                    let (chart_area, right_axis_area) =
                        split_chart_area_for_second_axis(*chart_chunk, dual_axis);

                    let mut transformed_series: Vec<Option<Vec<(f64, f64)>>> =
                        Vec::with_capacity(app.data.len());
                    for plot in &app.data {
                        if plot.axis == chart_axis {
                            transformed_series.push(None);
                        } else if let Some(bounds) = secondary_bounds {
                            transformed_series.push(Some(remap_series_to_bounds(
                                plot.data.as_slice(),
                                bounds,
                                chart_bounds,
                            )));
                        } else {
                            transformed_series.push(None);
                        }
                    }

                    let mut datasets: Vec<Dataset> = Vec::new();
                    for (idx, plot) in app.data.iter().enumerate() {
                        if plot.axis == chart_axis {
                            datasets.push(plot.dataset(plot.data.as_slice()));
                        } else if let Some(series) =
                            transformed_series[idx].as_ref().map(Vec::as_slice)
                        {
                            datasets.push(plot.dataset(series));
                        }
                    }

                    let chart = Chart::new(datasets)
                        .block(Block::default().borders(Borders::NONE))
                        .x_axis(
                            Axis::default()
                                .style(Style::default().fg(Color::Gray))
                                .bounds(x_axis_bounds)
                                .labels(app.x_axis_labels(x_axis_bounds)),
                        )
                        .y_axis(
                            Axis::default()
                                .title(axis_title(chart_measure_hint))
                                .style(Style::default().fg(Color::Gray))
                                .bounds(chart_bounds)
                                .labels(App::y_axis_labels_for(chart_measure_hint, chart_bounds)),
                        );

                    f.render_widget(chart, chart_area);

                    if let (Some(axis_area), Some(bounds)) = (right_axis_area, secondary_bounds) {
                        let graph_height = chart_area.height.saturating_sub(2).max(1);
                        let axis_graph_area =
                            Rect::new(axis_area.x, chart_area.y, axis_area.width, graph_height);
                        f.render_widget(
                            Block::default()
                                .borders(Borders::LEFT)
                                .style(Style::default().fg(Color::Gray)),
                            axis_graph_area,
                        );

                        let labels = App::y_axis_label_strings(secondary_measure_hint, bounds);
                        if labels.len() > 1 {
                            let labels_len = labels.len() as u16;
                            let draw_width = axis_area.width.saturating_sub(1);
                            let title_area = Rect::new(
                                axis_area.x + 1,
                                axis_area.y,
                                draw_width,
                                1.min(axis_area.height),
                            );
                            f.render_widget(
                                Paragraph::new(axis_title(secondary_measure_hint))
                                    .style(Style::default().fg(Color::Gray))
                                    .alignment(Alignment::Right),
                                title_area,
                            );

                            for (i, label) in labels.into_iter().enumerate() {
                                let dy =
                                    i as u16 * graph_height.saturating_sub(1) / (labels_len - 1);
                                let y = chart_area.y + graph_height.saturating_sub(1) - dy;
                                let label_area = Rect::new(
                                    axis_area.x + 1,
                                    y,
                                    draw_width,
                                    1.min(axis_area.height),
                                );
                                f.render_widget(
                                    Paragraph::new(label)
                                        .style(Style::default().fg(Color::Gray))
                                        .alignment(Alignment::Right),
                                    label_area,
                                );
                            }
                        }
                    }
                })?;
            }
            Event::Terminate => {
                killed.store(true, Ordering::Release);
                break;
            }
        }
    }

    killed.store(true, Ordering::Relaxed);

    disable_raw_mode()?;
    execute!(terminal.backend_mut())?;
    terminal.show_cursor()?;

    let new_size = terminal.size()?;
    terminal.set_cursor_position(Position {
        x: new_size.width,
        y: new_size.height,
    })?;

    if args.clear {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        parse_toml_config, parse_watch_dsl, remap_series_to_bounds, remap_value_between_bounds,
        resolve_axis_defaults, shorten_title, AxisSide, Measure,
    };

    #[test]
    fn dsl_parses_valid_spec() {
        let watch = parse_watch_dsl("title=CPU;measure=float;period=1.5;exp=echo 42", 1.0).unwrap();
        assert_eq!(watch.title, "CPU");
        assert_eq!(watch.exp, "echo 42");
        assert_eq!(watch.measure, Measure::Float);
        assert_eq!(watch.period.as_millis(), 1500);
        assert_eq!(watch.axis, None);
    }

    #[test]
    fn dsl_parses_explicit_axis() {
        let watch = parse_watch_dsl(
            "title=CPU;measure=float;axis=right;period=1;exp=echo 42",
            1.0,
        )
        .unwrap();
        assert_eq!(watch.axis, Some(AxisSide::Right));
    }

    #[test]
    fn dsl_requires_exp() {
        assert!(parse_watch_dsl("title=CPU;period=1", 1.0).is_err());
    }

    #[test]
    fn dsl_defaults_to_shortened_title() {
        let watch = parse_watch_dsl(
            "exp=echo this is a very long command that should be shortened",
            1.0,
        )
        .unwrap();
        assert!(watch.title.len() <= 40);
    }

    #[test]
    fn toml_parsing_works_with_global_period() {
        let watches = parse_toml_config(
            r#"
                period = 2

                [[watch]]
                exp = "echo 10"
                measure = "bytes"
            "#,
            None,
        )
        .unwrap();

        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].measure, Measure::Bytes);
        assert_eq!(watches[0].period.as_secs(), 2);
        assert_eq!(watches[0].title, "echo 10");
    }

    #[test]
    fn shorten_title_keeps_short_values() {
        assert_eq!(shorten_title("echo 10"), "echo 10");
    }

    #[test]
    fn remap_value_between_axis_bounds() {
        let src = [0.0, 100.0];
        let dst = [10.0, 20.0];
        assert_eq!(remap_value_between_bounds(0.0, src, dst), 10.0);
        assert_eq!(remap_value_between_bounds(50.0, src, dst), 15.0);
        assert_eq!(remap_value_between_bounds(100.0, src, dst), 20.0);
    }

    #[test]
    fn remap_series_keeps_nans_as_gaps() {
        let series = vec![(1.0, 0.0), (2.0, f64::NAN), (3.0, 100.0)];
        let remapped = remap_series_to_bounds(&series, [0.0, 100.0], [10.0, 20.0]);
        assert_eq!(remapped[0].1, 10.0);
        assert!(remapped[1].1.is_nan());
        assert_eq!(remapped[2].1, 20.0);
    }

    #[test]
    fn resolve_axis_defaults_splits_mixed_measures() {
        let memory = parse_watch_dsl("title=mem;measure=bytes;exp=echo 1", 1.0).unwrap();
        let cpu = parse_watch_dsl("title=cpu;measure=float;exp=echo 1", 1.0).unwrap();
        let resolved = resolve_axis_defaults(vec![memory, cpu]);
        assert_eq!(resolved[0].axis, Some(AxisSide::Left));
        assert_eq!(resolved[1].axis, Some(AxisSide::Right));
    }
}
