//! Performance guards. The counts are exact, so a regression in how much
//! ttmux redraws fails straight away. The timings have wide budgets, so they
//! only catch a slowdown of several times (an accidental O(n²), say), not
//! noise. `make bench` prints the real numbers from an optimised build.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ttmux::layout::Rect;
use ttmux::proto::WireCell;
use ttmux::render::draw_screen;

const ROWS: u16 = 50;
const COLS: u16 = 200;

/// What a busy pane gets: colourful log lines, emoji, cursor moves inside a
/// scroll region, and a full-screen redraw now and then.
fn workload() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..20_000u32 {
        let c = i % 256;
        out.extend(
            format!(
                "\x1b[38;5;{c}m{i:>6}\x1b[0m \x1b[1;4:3;58:2::255:0:0mwarn\x1b[0m \
                 some ordinary log text 👩\u{200d}💻 ⚠\u{fe0f} 🇺🇸 \x1b[48;2;10;20;{c}mdone\x1b[0m\r\n"
            )
            .as_bytes(),
        );
        if i % 1000 == 0 {
            out.extend(b"\x1b[2;40r\x1b[H\x1b[2J\x1b[10;5Hredraw\x1b[r");
        }
    }
    out
}

/// Budgets are about five times what a debug build takes on an M-series Mac;
/// an optimised build
/// gets a tenth of that.
fn budget(debug_ms: u64) -> Duration {
    let ms = if cfg!(debug_assertions) {
        debug_ms
    } else {
        debug_ms / 10
    };
    Duration::from_millis(ms)
}

/// Best of three, so one descheduled run does not fail the suite.
fn fastest(mut f: impl FnMut()) -> Duration {
    (0..3)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

fn report(name: &str, took: Duration, budget: Duration) {
    eprintln!("perf {name}: {took:?} (budget {budget:?})");
    assert!(took <= budget, "{name} took {took:?}, budget {budget:?}");
}

fn full_screen() -> vt100::Parser {
    let mut p = vt100::Parser::new(ROWS, COLS, 10_000);
    p.process(&workload()[..200_000]);
    p
}

fn drawn(p: &vt100::Parser) -> Buffer {
    let rect = Rect::new(0, 0, COLS, ROWS);
    let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, COLS, ROWS));
    draw_screen(&mut buf, rect, p.screen(), false);
    buf
}

#[test]
fn parsing_a_busy_stream_keeps_up() {
    let bytes = workload();
    let took = fastest(|| {
        let mut p = vt100::Parser::new(ROWS, COLS, 10_000);
        p.process(&bytes);
    });
    let mib = bytes.len() as f64 / (1 << 20) as f64;
    eprintln!(
        "perf parse: {mib:.1} MiB, {:.0} MiB/s",
        mib / took.as_secs_f64()
    );
    report("parse", took, budget(2_500));
}

#[test]
fn drawing_a_full_pane_keeps_up() {
    let p = full_screen();
    let took = fastest(|| {
        for _ in 0..100 {
            drawn(&p);
        }
    });
    report("100 draws", took, budget(2_500));
}

#[test]
fn a_keystroke_redraws_one_cell_and_an_idle_frame_none() {
    let mut p = full_screen();
    let before = drawn(&p);
    assert_eq!(before.diff(&drawn(&p)).len(), 0, "idle frame");

    p.process(b"\x1b[5;5Hx");
    let after = drawn(&p);
    let changed = before.diff(&after);
    assert_eq!(changed.len(), 1, "one keystroke");

    // And what that costs on the wire.
    let (x, y, cell) = changed[0];
    let msg = vec![WireCell {
        x,
        y,
        symbol: cell.symbol().into(),
        style: cell.style(),
    }];
    let bytes = serde_json::to_vec(&msg).unwrap().len();
    assert!(bytes <= 200, "one cell is {bytes} bytes of JSON");
}

#[test]
fn a_full_repaint_serialises_quickly() {
    let buf = drawn(&full_screen());
    let empty = Buffer::empty(buf.area);
    let cells: Vec<WireCell> = empty
        .diff(&buf)
        .into_iter()
        .map(|(x, y, c)| WireCell {
            x,
            y,
            symbol: c.symbol().into(),
            style: c.style(),
        })
        .collect();
    let took = fastest(|| {
        for _ in 0..10 {
            serde_json::to_vec(&cells).unwrap();
        }
    });
    let kib = serde_json::to_vec(&cells).unwrap().len() / 1024;
    eprintln!("perf repaint: {} cells, {kib} KiB", cells.len());
    report("10 repaints to JSON", took, budget(400));
}
