use anyhow::Context;
use chrono::prelude::*;
use core::option::Option;
use core::option::Option::{None, Some};
use itertools::Itertools;
use std::str::FromStr;
use tui::style::Style;
use tui::symbols;
use tui::widgets::{Dataset, GraphType, Paragraph};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Measure {
    Float,
    Bytes,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AxisSide {
    Left,
    Right,
}

impl FromStr for AxisSide {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" | "l" => Ok(Self::Left),
            "right" | "r" => Ok(Self::Right),
            other => Err(format!(
                "Unknown axis '{other}'. Supported values: left, right"
            )),
        }
    }
}

impl FromStr for Measure {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "float" => Ok(Self::Float),
            "bytes" => Ok(Self::Bytes),
            other => Err(format!(
                "Unknown measure '{other}'. Supported values: float, bytes"
            )),
        }
    }
}

impl Measure {
    pub fn format_value(self, value: f64) -> String {
        if !value.is_finite() {
            return "n/a".to_string();
        }

        match self {
            Self::Float => format_float(value),
            Self::Bytes => format_bytes_1024(value),
        }
    }

    pub const fn axis_name(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Bytes => "bytes",
        }
    }
}

fn format_float(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1000.0 {
        format!("{value:.0}")
    } else if abs >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn format_bytes_1024(value: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

    let sign = if value.is_sign_negative() { "-" } else { "" };
    let mut current = value.abs();
    let mut unit_idx = 0;

    while current >= 1024.0 && unit_idx < UNITS.len() - 1 {
        current /= 1024.0;
        unit_idx += 1;
    }

    let formatted = if current >= 100.0 {
        format!("{current:.1}")
    } else {
        format!("{current:.2}")
    };

    format!("{sign}{formatted} {}", UNITS[unit_idx])
}

pub struct PlotData {
    pub display: String,
    pub data: Vec<(f64, f64)>,
    pub style: Style,
    pub measure: Measure,
    pub axis: AxisSide,
    buffer: chrono::Duration,
    simple_graphics: bool,
}

impl PlotData {
    pub fn new(
        display: String,
        buffer: u64,
        style: Style,
        simple_graphics: bool,
        measure: Measure,
        axis: AxisSide,
    ) -> PlotData {
        PlotData {
            display,
            data: Vec::with_capacity(150),
            style,
            measure,
            axis,
            buffer: chrono::Duration::try_seconds(buffer as i64)
                .with_context(|| format!("Error converting {buffer} to seconds"))
                .unwrap(),
            simple_graphics,
        }
    }

    pub fn update(&mut self, item: Option<f64>) {
        let now = Local::now();
        let idx = now.timestamp_millis() as f64 / 1_000f64;
        match item {
            Some(value) => self.data.push((idx, value)),
            None => self.data.push((idx, f64::NAN)),
        }

        let earliest_timestamp = (now - self.buffer).timestamp_millis() as f64 / 1_000f64;
        let last_idx = self
            .data
            .iter()
            .enumerate()
            .filter(|(_, (timestamp, _))| *timestamp < earliest_timestamp)
            .map(|(idx, _)| idx)
            .next_back();
        if let Some(idx) = last_idx {
            self.data.drain(0..idx).for_each(drop)
        }
    }

    pub fn header_stats(&self) -> Vec<Paragraph<'_>> {
        let watch_header = Paragraph::new(self.display.clone()).style(self.style);
        let items: Vec<&f64> = self
            .data
            .iter()
            .filter(|(_, x)| !x.is_nan())
            .map(|(_, v)| v)
            .sorted_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .collect();

        if items.is_empty() {
            return vec![watch_header];
        }

        let min = **items.first().unwrap();
        let max = **items.last().unwrap();
        let avg = items.iter().copied().sum::<f64>() / items.len() as f64;
        let jtr = if items.len() > 1 {
            items
                .iter()
                .zip(items.iter().skip(1))
                .map(|(&prev, &curr)| (curr - prev).abs())
                .sum::<f64>()
                / (items.len() - 1) as f64
        } else {
            0.0
        };

        let p95_idx = (((items.len() - 1) as f64) * 0.95).round() as usize;
        let p95 = *items[p95_idx.min(items.len() - 1)];

        let to = self.data.iter().filter(|(_, x)| x.is_nan()).count();
        let last = self
            .data
            .last()
            .map(|(_, value)| *value)
            .unwrap_or(f64::NAN);

        vec![
            watch_header,
            Paragraph::new(format!("last {}", self.measure.format_value(last))).style(self.style),
            Paragraph::new(format!("min {}", self.measure.format_value(min))).style(self.style),
            Paragraph::new(format!("max {}", self.measure.format_value(max))).style(self.style),
            Paragraph::new(format!("avg {}", self.measure.format_value(avg))).style(self.style),
            Paragraph::new(format!("jtr {}", self.measure.format_value(jtr))).style(self.style),
            Paragraph::new(format!("p95 {}", self.measure.format_value(p95))).style(self.style),
            Paragraph::new(format!("t/o {to:?}")).style(self.style),
        ]
    }

    pub fn dataset<'a>(&'a self, data: &'a [(f64, f64)]) -> Dataset<'a> {
        Dataset::default()
            .marker(if self.simple_graphics {
                symbols::Marker::Dot
            } else {
                symbols::Marker::Braille
            })
            .style(self.style)
            .graph_type(GraphType::Line)
            .data(data)
    }
}

impl<'a> From<&'a PlotData> for Dataset<'a> {
    fn from(plot: &'a PlotData) -> Self {
        plot.dataset(plot.data.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::{AxisSide, Measure};

    #[test]
    fn parse_measure() {
        assert!(matches!("float".parse::<Measure>(), Ok(Measure::Float)));
        assert!(matches!("bytes".parse::<Measure>(), Ok(Measure::Bytes)));
        assert!("duration".parse::<Measure>().is_err());
    }

    #[test]
    fn parse_axis_side() {
        assert!(matches!("left".parse::<AxisSide>(), Ok(AxisSide::Left)));
        assert!(matches!("right".parse::<AxisSide>(), Ok(AxisSide::Right)));
        assert!("center".parse::<AxisSide>().is_err());
    }

    #[test]
    fn bytes_format_uses_1024() {
        let output = Measure::Bytes.format_value(1024.0);
        assert_eq!(output, "1.00 KiB");
    }
}
