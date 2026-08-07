use std::process::Command;
use std::time::Duration;

use gpui::{
    App, AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    Timer, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};

const BACKGROUND: u32 = 0x0011_1318;
const PANEL: u32 = 0x001a_1d24;
const BORDER: u32 = 0x0030_3642;
const PRIMARY: u32 = 0x00e8_ecf2;
const MUTED: u32 = 0x008d_96a8;
const GREEN: u32 = 0x0063_d392;
const BLUE: u32 = 0x006b_a6ff;
const AMBER: u32 = 0x00e6_b566;

struct SpikeView {
    timeline: Vec<String>,
    terminal: Vec<String>,
    diff_summary: String,
    base: String,
}

impl SpikeView {
    fn new(
        base: String,
        diff_summary: String,
        stress_events: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |view, cx| {
            let updates = [
                (
                    "assistant.turn_start  root agent running",
                    "$ cargo test --workspace",
                ),
                (
                    "tool.execution_start  shell [observed]",
                    "   Compiling spike-core v0.1.0",
                ),
                (
                    "tool.execution_partial_result  43 ms",
                    "   Compiling sdk-probe v0.1.0",
                ),
                (
                    "subagent.started  telemetry-spike [observed]",
                    "    Finished test profile",
                ),
                (
                    "tool.execution_complete  success  1.28 s",
                    "     Running 3 tests",
                ),
                ("session.idle  completed", "test result: ok. 3 passed"),
            ];

            for (activity, output) in updates {
                Timer::after(Duration::from_millis(350)).await;
                if view
                    .update(cx, |view, cx| {
                        view.timeline.push(activity.to_owned());
                        view.terminal.push(output.to_owned());
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }

            let started = std::time::Instant::now();
            let mut emitted = 0;
            while emitted < stress_events {
                Timer::after(Duration::from_millis(16)).await;
                let batch_size = (stress_events - emitted).min(1_000);
                if view
                    .update(cx, |view, cx| {
                        for offset in 0..batch_size {
                            let index = emitted + offset;
                            view.timeline
                                .push(format!("assistant.message_delta  token batch {index}"));
                            view.terminal
                                .push(format!("terminal output line {index:05}"));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                emitted += batch_size;
            }
            if stress_events > 0 {
                eprintln!(
                    "rendered {stress_events} coalesced timeline and terminal updates in {} ms",
                    started.elapsed().as_millis()
                );
            }
        })
        .detach();

        Self {
            timeline: vec!["session.start  SDK session connected".to_owned()],
            terminal: vec!["GCABB native terminal event stream".to_owned()],
            diff_summary,
            base,
        }
    }

    fn panel(title: &str, accent: u32) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(accent))
                    .child(title.to_owned()),
            )
    }
}

impl Render for SpikeView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let timeline = self
            .timeline
            .iter()
            .skip(self.timeline.len().saturating_sub(200))
            .enumerate()
            .map(|(index, item)| {
                div()
                    .py_1()
                    .text_sm()
                    .text_color(if index + 1 == self.timeline.len() {
                        rgb(PRIMARY)
                    } else {
                        rgb(MUTED)
                    })
                    .child(item.clone())
            });

        let terminal = self
            .terminal
            .iter()
            .skip(self.terminal.len().saturating_sub(200))
            .map(|line| div().text_sm().text_color(rgb(GREEN)).child(line.clone()));

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(PRIMARY))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xl().child(format!(
                        "GCABB / Phase 0  ·  {} events",
                        self.timeline.len()
                    )))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("native GPUI event projection"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(
                        Self::panel("ACTIVITY TIMELINE", BLUE)
                            .flex_1()
                            .overflow_hidden()
                            .children(timeline),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap_3()
                            .child(
                                Self::panel("LIVE TERMINAL", GREEN)
                                    .flex_1()
                                    .overflow_hidden()
                                    .children(terminal),
                            )
                            .child(
                                Self::panel("SELECTABLE-BASE CHANGES", AMBER)
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(PRIMARY))
                                            .child(format!("base: {}", self.base)),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(MUTED))
                                            .child(self.diff_summary.clone()),
                                    ),
                            ),
                    ),
            )
    }
}

fn git_diff_summary(base: &str) -> String {
    let output = Command::new("git")
        .args(["diff", "--shortstat", base, "--"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let summary = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if summary.is_empty() {
                "no changes".to_owned()
            } else {
                summary
            }
        }
        Ok(output) => format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("git unavailable: {error}"),
    }
}

fn main() {
    let base = std::env::args().nth(1).unwrap_or_else(|| "HEAD".to_owned());
    let stress_events = std::env::var("GCABB_STRESS_EVENTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let diff_summary = git_diff_summary(&base);

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        let base = base.clone();
        let diff_summary = diff_summary.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| SpikeView::new(base, diff_summary, stress_events, cx)),
        )
        .expect("failed to open GPUI spike window");
        cx.activate(true);
    });
}
