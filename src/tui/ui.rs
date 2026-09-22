//! Rendering for the TUI: the fan block, a status line, the temperature bars,
//! and the help popup.

use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Widget},
    Frame,
};

use super::app::App;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Aggregation, Config, ControlKind, InitialMode, Pwm, TempRef, TempSensor};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    /// Build a fake hwmon tree with one temperature chip and one PWM chip.
    fn fake_base() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // hwmon0: k10temp with a temperature
        let t = dir.path().join("hwmon0");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("name"), "k10temp\n").unwrap();
        fs::write(t.join("temp1_input"), "70000\n").unwrap();
        fs::write(t.join("temp1_label"), "Tctl\n").unwrap();
        // hwmon1: it8792 with a PWM
        let p = dir.path().join("hwmon1");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("name"), "it8792\n").unwrap();
        fs::write(p.join("pwm1"), "128\n").unwrap();
        fs::write(p.join("pwm1_enable"), "0\n").unwrap();
        dir
    }

    fn config() -> Config {
        Config {
            refresh_secs: 2.0,
            pwm: vec![Pwm {
                id: "pwm1".into(),
                hwmon: "it8792".into(),
                name: Some("Fan 1".into()),
                pwm_max: 255,
                temp_sensors: vec![TempRef {
                    hwmon: "it8792".into(),
                    sensor: "temp1".into(),
                }],
                temp_sensor: None,
                aggregation: Aggregation::Max,
                control: ControlKind::Curve,
                curve: vec![[30.0, 0.0], [75.0, 100.0]],
                default: InitialMode::Auto,
            }],
            temp_sensors: vec![TempSensor {
                hwmon: "k10temp".into(),
                sensors: vec!["temp1".into()],
            }],
            show_lm_sensors: false,
        }
    }

    #[test]
    fn renders_fan_row_and_temp_bar() {
        let dir = fake_base();
        let mut app = App::from(config(), dir.path());

        // Prime the display state as do_update would.
        if let Some(t) = app.temps.first_mut() {
            t.celsius = Some(70.0);
        }
        if let Some(f) = app.fans.first_mut() {
            f.duty = Some(50.0);
            f.rpm = Some(1650);
        }

        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        // Flatten the buffer to text and check our content is present.
        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();

        assert!(text.contains("Tctl"), "buffer:\n{text}");
        assert!(text.contains("70"), "temp value missing:\n{text}");
        assert!(text.contains("Fan 1"), "fan label missing:\n{text}");
        assert!(text.contains("50%"), "duty missing:\n{text}");
        assert!(text.contains("1650"), "rpm missing:\n{text}");
    }
}

// Light (foreground) bar colors.
const GREEN_LIGHT: Color = Color::Rgb(165, 183, 0);
const YELLOW_LIGHT: Color = Color::Rgb(227, 168, 43);
const RED_LIGHT: Color = Color::Rgb(204, 31, 26);
// Dark (background) bar colors.
const GREEN_DARK: Color = Color::Rgb(68, 68, 37);
const YELLOW_DARK: Color = Color::Rgb(94, 78, 40);
const RED_DARK: Color = Color::Rgb(76, 32, 32);

fn light(ratio: f64) -> Color {
    if ratio < 0.45 {
        GREEN_LIGHT
    } else if ratio < 0.75 {
        YELLOW_LIGHT
    } else {
        RED_LIGHT
    }
}
fn dark(ratio: f64) -> Color {
    if ratio < 0.45 {
        GREEN_DARK
    } else if ratio < 0.75 {
        YELLOW_DARK
    } else {
        RED_DARK
    }
}

impl App {
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let fan_rows = self.fans.len() as u16;

        let areas = Layout::vertical([
            Constraint::Length(fan_rows + 2),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

        self.draw_fans(frame, areas[0]);
        self.draw_status(frame, areas[1]);
        self.draw_temps(frame, areas[2]);

        if self.show_help {
            self.draw_help(frame, area);
        }
    }

    fn draw_fans(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered()
            .title(Line::from(" Fans ".bold()).centered())
            .border_set(ratatui::symbols::border::THICK);
        let inner = block.inner(area);

        let mut lines: Vec<Line> = Vec::new();
        for (i, f) in self.fans.iter().enumerate() {
            let selected = i == self.selected;
            let marker = if selected { ">" } else { " " };
            let duty = f
                .duty
                .map(|d| format!("{d:4.0}%"))
                .unwrap_or_else(|| "  n/a".into());
            let rpm = f
                .rpm
                .map(|r| format!("{r:>6} rpm"))
                .unwrap_or_else(|| "     -".into());

            let style = if selected {
                Style::default().reversed()
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(
                    format!("{marker: <5}{}  ", f.label),
                    style,
                ),
                Span::styled(format!("{: <8}", f.mode.label()), style),
                Span::styled(format!("{duty}  "), style),
                Span::styled(rpm, style),
            ];
            if let Some(e) = &f.err {
                spans.push(Span::styled(format!("  {e}"), style.fg(Color::Red)));
            }
            lines.push(Line::from(spans));
        }
        if lines.is_empty() {
            lines.push(Line::from("  no controllable PWM channels found"));
        }

        Paragraph::new(lines)
            .block(block)
            .render(area, frame.buffer_mut());
        // `inner` is reserved so the helper stays meaningful for future use.
        let _ = inner;
    }

    fn draw_status(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let text = if self.status.is_empty() {
            " ↑↓ select   a/f/o = auto/full/off   0-9 = %   s sort   ? help   q quit "
        } else {
            &self.status
        };
        let style = if self.status.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Red)
        };
        Paragraph::new(Line::from(Span::styled(text, style)))
            .render(area, frame.buffer_mut());
    }

    fn draw_temps(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered()
            .title(Line::from(" Temperatures ".bold()).centered())
            .title_bottom(Line::from(" s = sort ").right_aligned())
            .border_set(ratatui::symbols::border::THICK);
        let inner = block.inner(area);

        let rows: Vec<&super::app::Temp> = {
            let mut v: Vec<&super::app::Temp> = self.temps.iter().collect();
            if self.sorting {
                v.sort_by(|a, b| {
                    b.celsius
                        .unwrap_or(-999.0)
                        .partial_cmp(&a.celsius.unwrap_or(-999.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            v
        };

        let inner_w = inner.width as usize;
        let label_w = 34usize;
        let val_w = 8usize; // includes the trailing "°C"
        let bar_w = inner_w.saturating_sub(label_w + val_w).max(1);
        let start = self.scroll.min(rows.len());
        let end = (start + inner.height as usize).min(rows.len());

        let lines: Vec<Line> = rows[start..end]
            .iter()
            .map(|t| {
                let ratio = (t.celsius.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0);
                let label = format!(" {: <33}", t.label);
                let val = match t.celsius {
                    Some(c) => format!("{n: >6}°C", n = c.round() as i64),
                    None => "    n/a".into(),
                };
                let bar: Vec<Span> = (0..bar_w).map(|i| {
                    let r = i as f64 / bar_w as f64;
                    let col = if r < ratio { light(r) } else { dark(r) };
                    Span::styled("▀", Style::default().fg(col))
                }).collect();
                let mut spans = vec![Span::raw(label)];
                spans.extend(bar);
                spans.push(Span::raw(val));
                Line::from(spans)
            })
            .collect();

        Paragraph::new(lines)
            .block(block)
            .render(area, frame.buffer_mut());
    }

    fn draw_help(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let w = (40u16).min(area.width);
        let h = (19u16).min(area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect::new(x, y, w, h);
        frame.render_widget(Clear, rect);

        let text = ratatui::text::Text::from(vec![
            Line::from("Fans".bold()),
            Line::from("  ↑↓/jk   Select fan"),
            Line::from("  a        Auto (follow curve or kernel)"),
            Line::from("  f        Full speed"),
            Line::from("  o        Off"),
            Line::from("  0-9      Fixed 0..90%"),
            Line::from(""),
            Line::from("Temperatures"),
            Line::from("  s        Sort by temperature"),
            Line::from("  PgUp/Dn  Scroll"),
            Line::from(""),
            Line::from("Global"),
            Line::from("  ?        Toggle help"),
            Line::from("  Esc      Close help"),
            Line::from("  q        Quit"),
        ]);
        Paragraph::new(text)
            .block(
                Block::bordered()
                    .title(Line::from(" Help ".bold()).centered())
                    .border_set(ratatui::symbols::border::THICK)
                    .padding(Padding::uniform(1)),
            )
            .render(rect, frame.buffer_mut());
    }
}
