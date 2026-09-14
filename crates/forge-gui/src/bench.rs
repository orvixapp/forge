//! In-process benchmark scenarios driven by `--benchmark-*` flags. Each
//! prints one JSON line that `forge-bench` parses.

use crate::window::{ForgeWindow, FrameProbe};
use gpui::{App, Timer, WindowHandle};
use proto_ipc::{CursorStyle, Rgb, ScreenCell, ScreenCursor, ScreenRow, ServerMessage, Viewport};
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

#[derive(Serialize)]
pub struct GuiMetrics {
    pub scenario: &'static str,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples_ms: Option<Vec<f64>>,
    pub frames: u64,
    pub pss_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub painted_cells: Option<u64>,
}

pub fn emit_metrics(metrics: &GuiMetrics) {
    if let Ok(json) = serde_json::to_string(metrics) {
        println!("{json}");
    }
}

/// Proportional set size of this process, Linux only.
#[must_use]
pub fn process_pss_kib() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    contents.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

pub fn spawn_idle_benchmark(idle_ms: u64, render_count: Arc<AtomicU64>, cx: &mut App) {
    cx.spawn(async move |cx| {
        Timer::after(Duration::from_secs(1)).await;
        let baseline = render_count.load(Ordering::Relaxed);
        Timer::after(Duration::from_millis(idle_ms)).await;
        emit_metrics(&GuiMetrics {
            scenario: "idle",
            elapsed_ms: Duration::from_millis(idle_ms).as_secs_f64() * 1_000.0,
            samples_ms: None,
            frames: render_count
                .load(Ordering::Relaxed)
                .saturating_sub(baseline),
            pss_kib: process_pss_kib(),
            painted_cells: None,
        });
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

/// Measures `iterations` frames where `mutate` changes the window state at
/// the start of a frame tick, so each sample covers state update → draw →
/// present and not the wait for the compositor.
fn spawn_frame_benchmark(
    scenario: &'static str,
    iterations: usize,
    window: WindowHandle<ForgeWindow>,
    mutate: impl Fn(&mut ForgeWindow, usize) + Clone + 'static,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        Timer::after(Duration::from_secs(1)).await;
        let Ok(view) = window.update(cx, |_, _, cx| cx.entity()) else {
            return;
        };
        let mut samples_ms = Vec::with_capacity(iterations);
        for iteration in 1..=iterations {
            let (presented_tx, presented_rx) = oneshot::channel();
            let view = view.clone();
            let mutate = mutate.clone();
            let scheduled = window.update(cx, |_, window, _| {
                window.on_next_frame(move |_, cx| {
                    let started = Instant::now();
                    view.update(cx, |view, cx| {
                        mutate(view, iteration);
                        view.frame_probe = Some(FrameProbe {
                            started,
                            presented: presented_tx,
                        });
                        cx.notify();
                    });
                });
            });
            if scheduled.is_err() {
                break;
            }
            if let Ok(sample) = presented_rx.await {
                samples_ms.push(sample);
            }
        }
        let painted_cells = window
            .update(cx, |view, _, _| view.active_tab().terminal.painted_cells)
            .ok()
            .and_then(|cells| u64::try_from(cells).ok());
        emit_metrics(&GuiMetrics {
            scenario,
            elapsed_ms: samples_ms.last().copied().unwrap_or_default(),
            samples_ms: Some(samples_ms),
            frames: u64::try_from(iterations).unwrap_or(u64::MAX),
            pss_kib: process_pss_kib(),
            painted_cells,
        });
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

/// 200×60 grid where every cell changes glyph and colour each frame.
pub fn spawn_grid_benchmark(iterations: usize, window: WindowHandle<ForgeWindow>, cx: &mut App) {
    spawn_frame_benchmark(
        "grid_full",
        iterations,
        window,
        |view, revision| {
            let patch = synthetic_grid_patch(revision);
            view.active_tab_mut()
                .terminal
                .grid
                .apply_server_message(patch)
                .expect("synthetic grid patch");
        },
        cx,
    );
}

/// `count` empty panes; each frame moves the focus so every pane re-renders
/// its chrome. Measures layout and paint of the shell itself.
pub fn spawn_panes_benchmark(
    count: usize,
    iterations: usize,
    window: WindowHandle<ForgeWindow>,
    cx: &mut App,
) {
    let _ = window.update(cx, |view, _, cx| view.open_benchmark_panes(count, cx));
    spawn_frame_benchmark(
        "panes",
        iterations,
        window,
        |view, iteration| {
            view.active_tab = iteration % view.tabs.len();
        },
        cx,
    );
}

/// Full 200×60 frame where every cell changes glyph and colour each revision,
/// so the renderer cannot reuse anything from the previous frame.
#[must_use]
pub fn synthetic_grid_patch(revision: usize) -> ServerMessage {
    const PRINTABLE_ASCII: usize = 94;
    let cell = |x: usize, y: usize| {
        let glyph = u8::try_from((x + y * 7 + revision * 13) % PRINTABLE_ASCII)
            .expect("index below 94 fits a byte")
            + b'!';
        ScreenCell {
            text: char::from(glyph).into(),
            foreground: Some(Rgb {
                r: 0x88,
                g: u8::try_from((x + revision) % 128).expect("bounded synthetic color") + 0x40,
                b: 0xd0,
            }),
            background: None,
            styled: true,
        }
    };
    ServerMessage::ScreenPatch {
        session_id: 0,
        revision: u64::try_from(revision).unwrap_or(u64::MAX),
        cols: 200,
        rows: 60,
        full: true,
        dirty_rows: (0..60)
            .map(|y| ScreenRow {
                y,
                cells: (0..200).map(|x| cell(x, usize::from(y))).collect(),
            })
            .collect(),
        cursor: Some(ScreenCursor {
            x: u16::try_from(revision % 200).expect("column below 200"),
            y: u16::try_from(revision % 60).expect("row below 60"),
            visible: true,
            blinking: false,
            style: CursorStyle::Block,
        }),
        viewport: Viewport {
            total: 60,
            offset: 0,
            len: 60,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_benchmark_builds_a_fully_dirty_200_by_60_grid() {
        let ServerMessage::ScreenPatch {
            cols,
            rows,
            full,
            dirty_rows,
            ..
        } = synthetic_grid_patch(1)
        else {
            panic!("expected screen patch");
        };
        assert_eq!((cols, rows), (200, 60));
        assert!(full);
        assert_eq!(dirty_rows.len(), 60);
        assert!(dirty_rows.iter().all(|row| row.cells.len() == 200));
        assert!(
            dirty_rows
                .iter()
                .flat_map(|row| &row.cells)
                .all(|cell| cell.text.len() == 1 && cell.text.is_ascii() && cell.text != " ")
        );
        let ServerMessage::ScreenPatch {
            dirty_rows: next, ..
        } = synthetic_grid_patch(2)
        else {
            panic!("expected screen patch");
        };
        assert!(
            dirty_rows.iter().zip(&next).all(|(a, b)| a
                .cells
                .iter()
                .zip(&b.cells)
                .all(|(a, b)| a != b)),
            "every cell changes between revisions"
        );
    }
}
