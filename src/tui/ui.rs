//! Rendering for the TUI client: the fan block, a status line, the
//! temperature bars, and the help popup. All of it is drawn from the last
//! `State` served by the daemon.

use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Widget},
    Frame,
};

use super::app::App;
use crate::ipc::State;

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

        if let Some(state) = &self.state {
            let fan_rows = state.fans.len() as u16;
            let areas = Layout::vertical([
                Constraint::Length(fan_rows + 2),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

            self.draw_fans(frame, areas[0], state);
            self.draw_status(frame, areas[1], state);
            self.draw_temps(frame, areas[2], state);
        } else {
            // The daemon is unreachable (or no state has arrived yet).
            self.draw_unreachable(frame, area);
        }

        if self.show_help {
            self.draw_help(frame, area);
        }
        // The value dialog is topmost.
        if self.value_entry.is_some() {
            self.draw_value_entry(frame, area);
        }
        // The curve editor is topmost.
        if self.curve_editor.is_some() {
            self.draw_curve_editor(frame, area);
        }
    }

    fn draw_unreachable(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered()
            .title(Line::from(" fanctl ".bold()).centered())
            .border_set(ratatui::symbols::border::THICK)
            .padding(Padding::uniform(2));
        let text = if self.status.is_empty() {
            "waiting for the fanctld daemon …".to_string()
        } else {
            self.status.clone()
        };
        let style = if self.status.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Red)
        };
        Paragraph::new(vec![Line::from(Span::styled(text, style))])
            .block(block)
            .render(area, frame.buffer_mut());
    }

    fn draw_fans(&self, frame: &mut Frame, area: ratatui::layout::Rect, state: &State) {
        let block = Block::bordered()
            .title(Line::from(" Fans ".bold()).centered())
            .border_set(ratatui::symbols::border::THICK);
        let inner = block.inner(area);

        let mut lines: Vec<Line> = Vec::new();
        for (i, f) in state.fans.iter().enumerate() {
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
                Span::styled(format!("{marker: <5}{}  ", f.label), style),
                Span::styled(format!("{: <8}", f.mode.label()), style),
                Span::styled(format!("{duty}  "), style),
                Span::styled(rpm, style),
            ];
            if let Some(e) = &f.err {
                spans.push(Span::styled(format!("  {e}"), style.fg(Color::Red)));
            }
            if state.sweeping.as_deref() == Some(f.id.as_str()) {
                spans.push(Span::styled("  calibrating…", style.fg(Color::Cyan)));
            }
            lines.push(Line::from(spans));
        }
        if lines.is_empty() {
            lines.push(Line::from("  no controllable PWM channels in the config"));
        }

        Paragraph::new(lines)
            .block(block)
            .render(area, frame.buffer_mut());
        // `inner` is reserved so the helper stays meaningful for future use.
        let _ = inner;
    }

    fn draw_status(&self, frame: &mut Frame, area: ratatui::layout::Rect, state: &State) {
        let sweep_note = self.recent_sweep_note(state);
        // A calibration sweep is in flight (its progress also shows on the
        // fan row; the result arrives in the next snapshot).
        let calibrating = state
            .sweeping
            .as_ref()
            .map(|fan| format!("{fan}: calibrating (0→100 %) …"));
        let text = if !self.status.is_empty() {
            self.status.clone()
        } else if let Some(c) = &calibrating {
            c.clone()
        } else if let Some(n) = sweep_note {
            n.to_string()
        } else if !state.status.is_empty() {
            state.status.clone()
        } else {
            " ↑↓ select   a/f/o = auto/full/off   Enter = %   e edit   c calibrate   s sort   ? help   q quit "
                .to_string()
        };
        let style = if text.starts_with(" ↑↓") {
            Style::default().fg(Color::DarkGray)
        } else if self.status.is_empty() && (state.sweeping.is_some() || sweep_note.is_some()) {
            // Informational (a sweep in progress or its result), not an
            // error.
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Red)
        };
        Paragraph::new(Line::from(Span::styled(text, style))).render(area, frame.buffer_mut());
    }

    fn draw_temps(&self, frame: &mut Frame, area: ratatui::layout::Rect, state: &State) {
        let block = Block::bordered()
            .title(Line::from(" Temperatures ".bold()).centered())
            .title_bottom(Line::from(" s = sort ").right_aligned())
            .border_set(ratatui::symbols::border::THICK);
        let inner = block.inner(area);

        let rows: Vec<&crate::ipc::Temp> = {
            let mut v: Vec<&crate::ipc::Temp> = state.temps.iter().collect();
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
                let bar: Vec<Span> = (0..bar_w)
                    .map(|i| {
                        let r = i as f64 / bar_w as f64;
                        let col = if r < ratio { light(r) } else { dark(r) };
                        Span::styled("▀", Style::default().fg(col))
                    })
                    .collect();
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
        let h = (20u16).min(area.height);
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
            Line::from("  Enter    Set exact duty (0-100%)"),
            Line::from("  e        Edit curve and default mode (s saves)"),
            Line::from("  c        Calibrate: find the duty range the fan responds to"),
            Line::from(""),
            Line::from("Temperatures"),
            Line::from("  s        Sort by temperature"),
            Line::from("  PgUp/Dn  Scroll"),
            Line::from(""),
            Line::from("Global"),
            Line::from("  ?        Toggle help"),
            Line::from("  Esc      Close help"),
            Line::from("  q / Ctrl+C   Quit"),
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

    /// The "set exact duty" dialog: a small modal box with the input the
    /// user is typing, an error line, and the key hints.
    fn draw_value_entry(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let Some(e) = &self.value_entry else {
            return;
        };
        let w = (46u16).min(area.width);
        let h = (12u16).min(area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect::new(x, y, w, h);
        frame.render_widget(Clear, rect);

        let fan = self
            .state
            .as_ref()
            .and_then(|s| s.fans.get(self.selected))
            .map(|f| f.label.clone())
            .unwrap_or_else(|| "the selected fan".into());
        let cursor = if e.text.is_empty() { " " } else { "▌" };

        let mut lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(format!("  {fan}")),
            Line::from(format!("  New duty (0-100):  {}{cursor}", e.text)),
        ];
        lines.push(match &e.error {
            Some(msg) => Line::from(Span::styled(msg.clone(), Style::default().fg(Color::Red))),
            None => Line::from("  "),
        });
        lines.push(Line::from(""));
        lines.push(Line::from("  Enter = apply    Esc = cancel"));

        Paragraph::new(ratatui::text::Text::from(lines))
            .block(
                Block::bordered()
                    .title(Line::from(" Set exact duty ".bold()).centered())
                    .border_set(ratatui::symbols::border::THICK)
                    .padding(Padding::uniform(1)),
            )
            .render(rect, frame.buffer_mut());
    }

    /// The "edit curve and default mode" dialog: a small modal table for the
    /// selected fan — one row per `{temp, duty}` point, a final
    /// "default mode" row, an error line, and the key hints.
    fn draw_curve_editor(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let Some(e) = &self.curve_editor else {
            return;
        };
        let fan = self
            .state
            .as_ref()
            .and_then(|s| s.fans.get(self.selected))
            .map(|f| f.label.clone())
            .unwrap_or_else(|| "the selected fan".into());

        let n = e.points.len() as u16;
        let w = (46u16).min(area.width);
        let h = (3u16 + n + 4).min(area.height); // header, points, default, error, hints
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect::new(x, y, w, h);
        frame.render_widget(Clear, rect);

        let mut lines: Vec<Line> = vec![Line::from(format!(
            "  {t: >6}  {d: >6}",
            t = "temp",
            d = "duty"
        ))];
        for (i, p) in e.points.iter().enumerate() {
            let is_cur = i == e.row;
            let marker = if is_cur { ">" } else { " " };
            let temp = if is_cur && e.field == 0 {
                format!("{}▌", e.text)
            } else {
                format!("{}", p.temp)
            };
            let duty = if is_cur && e.field == 1 {
                format!("{}▌", e.text)
            } else {
                format!("{}", p.duty)
            };
            let style = |active: bool| {
                if active {
                    Style::default().reversed()
                } else {
                    Style::default()
                }
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{marker} ")),
                Span::styled(format!(" {temp: >6}  "), style(is_cur && e.field == 0)),
                Span::styled(format!("{duty: >6}"), style(is_cur && e.field == 1)),
            ]));
        }
        let on_default = e.on_default_row();
        let mode = match e.default_mode {
            crate::config::types::InitialMode::Auto => "auto",
            crate::config::types::InitialMode::Off => "off",
            crate::config::types::InitialMode::Full => "full",
        };
        let marker = if on_default { ">" } else { " " };
        lines.push(Line::from(Span::styled(
            format!("{marker} default:  {mode}"),
            if on_default {
                Style::default().reversed()
            } else {
                Style::default()
            },
        )));
        if let Some(msg) = &e.error {
            lines.push(Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(Color::Red),
            )));
        }
        let hints = if e.saving {
            "  saving …"
        } else {
            "  a=add x=del u/d=move  s=save  esc=cancel"
        };
        lines.push(Line::from(hints));

        let title = if e.saving {
            " Saving "
        } else {
            &format!(" Edit {fan} ")
        };
        Paragraph::new(ratatui::text::Text::from(lines))
            .block(
                Block::bordered()
                    .title(Line::from(format!("{title}").bold()).centered())
                    .border_set(ratatui::symbols::border::THICK)
                    .padding(Padding::uniform(1)),
            )
            .render(rect, frame.buffer_mut());
    }
}

#[cfg(test)]
mod tests {
    use crate::ipc::{Client, FanState, Mode, State, Temp};
    use crate::tui::app::ValueEntry;
    use crate::tui::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Instant;

    fn app_with(state: Option<State>) -> App {
        App {
            client: Client::new(std::path::PathBuf::from("/nonexistent/fanctld.sock")),
            state,
            selected: 0,
            sorting: false,
            scroll: 0,
            show_help: false,
            status: String::new(),
            quit: false,
            value_entry: None,
            curve_editor: None,
            job: None,
            pending_mode: None,
            next_refresh: Instant::now(),
            last_sweep_report: None,
            sweep_report_at: None,
        }
    }

    fn state() -> State {
        State {
            refresh_secs: 2.0,
            status: String::new(),
            fans: vec![FanState {
                id: "pwm1".into(),
                label: "Fan 1".into(),
                mode: Mode::Manual { percent: 50 },
                duty: Some(50.0),
                rpm: Some(1650),
                err: None,
                curve: vec![
                    crate::curve::CurvePoint::new(30.0, 0.0),
                    crate::curve::CurvePoint::new(75.0, 100.0),
                ],
                default_mode: crate::config::types::InitialMode::Auto,
            }],
            temps: vec![Temp {
                refname: "k10temp:temp1".into(),
                label: "Tctl".into(),
                source: "hwmon".into(),
                celsius: Some(70.0),
            }],
            sweeping: None,
            sweep: None,
        }
    }

    #[test]
    fn renders_fan_row_and_temp_bar() {
        let app = app_with(Some(state()));

        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

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

    #[test]
    fn renders_unreachable_message() {
        let app = app_with(None);
        let backend = TestBackend::new(60, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(text.contains("fanctld"), "buffer:\n{text}");
    }

    #[test]
    fn renders_value_dialog() {
        let mut app = app_with(Some(state()));
        app.value_entry = Some(ValueEntry {
            text: "42".into(),
            error: None,
        });

        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(text.contains("New duty"), "buffer:\n{text}");
        assert!(text.contains("42"), "buffer:\n{text}");
        assert!(text.contains("Esc = cancel"), "buffer:\n{text}");
    }

    #[test]
    fn renders_curve_editor_dialog() {
        let mut app = app_with(Some(state()));
        app.curve_editor = Some(crate::tui::app::CurveEditor {
            points: vec![
                crate::curve::CurvePoint::new(30.0, 0.0),
                crate::curve::CurvePoint::new(75.0, 100.0),
            ],
            row: 0,
            field: 0,
            text: "30".into(),
            default_mode: crate::config::types::InitialMode::Auto,
            error: None,
            saving: false,
        });

        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(text.contains("Edit Fan 1"), "buffer:\n{text}");
        assert!(text.contains("temp"), "buffer:\n{text}");
        assert!(text.contains("30"), "buffer:\n{text}");
        assert!(text.contains("75"), "buffer:\n{text}");
        assert!(text.contains("100"), "buffer:\n{text}");
        assert!(text.contains("default:  auto"), "buffer:\n{text}");
        assert!(text.contains("s=save"), "buffer:\n{text}");
        assert!(text.contains("esc=cancel"), "buffer:\n{text}");
    }

    #[test]
    fn renders_sweeping_marker_and_calibrate_help() {
        let mut st = state();
        st.sweeping = Some("pwm1".into());
        let mut app = app_with(Some(st));
        app.show_help = true;

        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(
            text.contains("calibrating"),
            "fan row marker missing:\n{text}"
        );
        assert!(text.contains("Calibrate"), "help line missing:\n{text}");
    }

    #[test]
    fn status_line_shows_a_recent_sweep_result_until_it_fades() {
        let mut st = state();
        st.sweep = Some(crate::sweep::SweepReport {
            fan: "pwm1".into(),
            min_duty: Some(20),
            max_duty: Some(80),
            saturated: true,
            note: "pwm1: the fan spins between 20 % and 80 % duty (it saturates at 80 %)".into(),
        });

        // Recently seen: the note shows in the status line.
        let mut app = app_with(Some(st.clone()));
        app.sweep_report_at = Some(Instant::now());
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(
            text.contains("spins between 20 % and 80 %"),
            "buffer:\n{text}"
        );

        // Too old (2 min): it fades back to the key hints.
        let mut app = app_with(Some(st));
        app.sweep_report_at = Some(Instant::now() - std::time::Duration::from_secs(120));
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let h = buf.area().height;
        let w = buf.area().width;
        let text: String = (0..h)
            .flat_map(|y| (0..w).map(move |x| buf[(x, y)].symbol()))
            .collect();
        assert!(!text.contains("spins between"), "buffer:\n{text}");
        assert!(text.contains("c calibrate"), "key hints missing:\n{text}");
    }
}
