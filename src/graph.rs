// SPDX-License-Identifier: GPL-3.0-only
//! A minimal line chart rendered with the iced canvas widget.

use cosmic::iced::{Color, Point, Rectangle};
use cosmic::widget::canvas::{self, Frame, LineCap, LineJoin, Path, Stroke};

/// One line of a chart.
pub struct Series {
    pub color: Color,
    pub values: Vec<f32>,
}

impl Series {
    pub fn new(color: Color, values: Vec<f32>) -> Self {
        Self { color, values }
    }
}

/// Draws one or more series on a shared, fixed scale.
pub struct Chart {
    series: Vec<Series>,
    max: f32,
    grid: Color,
}

impl Chart {
    pub fn new(series: Vec<Series>, max: f32) -> Self {
        Self {
            series,
            max,
            grid: Color::from_rgba(0.5, 0.5, 0.5, 0.25),
        }
    }
}

impl canvas::Program<crate::Message, cosmic::Theme, cosmic::Renderer> for Chart {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &cosmic::Renderer,
        _theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: cosmic::iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry<cosmic::Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        if width <= 1.0 || height <= 1.0 {
            return vec![frame.into_geometry()];
        }

        let grid_stroke = Stroke::default().with_width(1.0).with_color(self.grid);
        for step in 0..=4 {
            let y = height * step as f32 / 4.0;
            let path = Path::new(|builder| {
                builder.move_to(Point::new(0.0, y));
                builder.line_to(Point::new(width, y));
            });
            frame.stroke(&path, grid_stroke);
        }

        let max = self.max.max(0.001);
        for series in &self.series {
            let count = series.values.len();
            if count < 2 {
                continue;
            }

            let path = Path::new(|builder| {
                for (index, value) in series.values.iter().enumerate() {
                    let x = width * index as f32 / (count - 1) as f32;
                    let y = height - (value / max).clamp(0.0, 1.0) * height;
                    if index == 0 {
                        builder.move_to(Point::new(x, y));
                    } else {
                        builder.line_to(Point::new(x, y));
                    }
                }
            });

            frame.stroke(
                &path,
                Stroke::default()
                    .with_width(2.0)
                    .with_color(series.color)
                    .with_line_cap(LineCap::Round)
                    .with_line_join(LineJoin::Round),
            );
        }

        vec![frame.into_geometry()]
    }
}

/// Minimum, average and maximum of a series.
pub fn stats(values: &[f32]) -> (f32, f32, f32) {
    if values.is_empty() {
        return (0.0, 0.0, 0.0);
    }

    let mut min = f32::MAX;
    let mut max = f32::MIN;
    let mut sum = 0.0;
    for value in values {
        min = min.min(*value);
        max = max.max(*value);
        sum += *value;
    }

    (min, sum / values.len() as f32, max)
}
