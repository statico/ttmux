//! The which-key hint popup: what can follow the prefix.
//!
//! After the leader chord the keymap is invisible, so the second chord is
//! guesswork or a trip to the help screen. This lists every binding that
//! starts with the prefix, in a modal near the bottom of the screen, for as
//! long as the app is waiting. Pure data and drawing: the app owns the
//! "are we pending" state and decides whether to call [`draw`].

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::config::{Chord, Config};
use crate::layout::Rect;
use crate::render::put_cell;

/// Every binding that begins with `prefix`, as (second chord, action) pairs.
///
/// Ordered letters first, then punctuation, then named and modified keys, so
/// the common single-letter bindings are where the eye lands. Ties keep the
/// keymap's own (`BTreeMap`) order, which makes the listing deterministic.
pub fn entries(cfg: &Config, prefix: &Chord) -> Vec<(String, String)> {
    let (map, _) = cfg.keymap();
    let mut v: Vec<(String, String)> = map
        .iter()
        .filter(|(b, _)| b.0.len() == 2 && b.0[0] == *prefix)
        .map(|(b, a)| (b.0[1].to_string(), a.to_string()))
        .collect();
    v.sort_by_key(|(k, _)| (rank(k), k.clone()));
    v
}

fn rank(key: &str) -> u8 {
    let mut c = key.chars();
    match (c.next(), c.next()) {
        (Some(ch), None) if ch.is_alphanumeric() => 0,
        (Some(_), None) => 1,
        _ => 2,
    }
}

/// Draw the popup at the bottom of `area`, one row above the status bar.
///
/// Sized to its contents, spread over as many columns as fit, and clipped to
/// whatever room there is: on a terminal too small for it, less is drawn.
pub fn draw(buf: &mut Buffer, area: Rect, prefix: &Chord, cfg: &Config) {
    let rows = entries(cfg, prefix);
    if rows.is_empty() || area.w < 8 || area.h < 6 {
        return;
    }
    let cells: Vec<String> = rows.iter().map(|(k, a)| format!("{k:>7}  {a}")).collect();
    let cw = cells.iter().map(|s| s.chars().count()).max().unwrap_or(1) as u16 + 2;

    // Chrome is 2 columns of border plus 1 of padding each side, and 2 rows of
    // border plus the title and hint rows.
    let cols = ((area.w - 4) / cw).clamp(1, cells.len() as u16);
    let nrows = cells.len().div_ceil(cols as usize) as u16;
    let w = (cols * cw + 4).min(area.w);
    let h = (nrows + 4).min(area.h.saturating_sub(1));
    let rect = Rect::new(
        area.x + (area.w - w) / 2,
        area.bottom().saturating_sub(1 + h),
        w,
        h,
    );

    let inner = crate::app::modal(buf, rect, &prefix.to_string(), "any other key cancels", cfg);
    let key_style = Style::default()
        .fg(cfg.status.accent.into())
        .add_modifier(Modifier::BOLD);
    let act_style = Style::default().fg(cfg.status.fg.into());
    for (i, ((key, _), cell)) in rows.iter().zip(&cells).enumerate() {
        let x = inner.x + (i as u16 / nrows) * cw;
        let y = inner.y + i as u16 % nrows;
        if y >= inner.bottom() || x >= inner.right() {
            continue;
        }
        // The key is right-aligned in the first 7 columns; colour it apart
        // from its description so the column scans as a key column.
        let keyed = cell.chars().count() - key.chars().count() - 2;
        for (j, ch) in cell.chars().enumerate() {
            let cx = x + j as u16;
            if cx >= inner.right() {
                break;
            }
            let style = if j >= keyed && j < keyed + key.chars().count() {
                key_style
            } else {
                act_style
            };
            put_cell(buf, cx, y, ch.encode_utf8(&mut [0u8; 4]), style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeysPreset;
    use ratatui::layout::Rect as RRect;
    use std::str::FromStr;

    fn chord(s: &str) -> Chord {
        Chord::from_str(s).unwrap()
    }

    fn text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_default_config_turns_which_key_on() {
        assert!(Config::default().general.which_key);
    }

    #[test]
    fn entries_lists_the_second_chords_of_the_prefix() {
        let e = entries(&Config::default(), &chord("ctrl+t"));
        assert!(e.contains(&("s".into(), "split down".into())), "{e:?}");
        assert!(e.contains(&("c".into(), "new-tab".into())), "{e:?}");
        assert!(e.contains(&("ctrl+d".into(), "detach".into())), "{e:?}");
    }

    #[test]
    fn entries_ignores_another_prefix() {
        let mut cfg = Config::default();
        cfg.general.keys_preset = KeysPreset::Tmux;
        // The tmux preset leads with ctrl+b, so the vim leader has nothing.
        assert!(entries(&cfg, &chord("ctrl+t")).is_empty());
        assert!(!entries(&cfg, &chord("ctrl+b")).is_empty());
    }

    #[test]
    fn entries_puts_letters_before_punctuation_before_named_keys() {
        let e = entries(&Config::default(), &chord("ctrl+t"));
        let pos = |k: &str| e.iter().position(|(a, _)| a == k).unwrap();
        assert!(pos("s") < pos("%"));
        assert!(pos("%") < pos("ctrl+d"));
    }

    #[test]
    fn entries_is_deterministic() {
        let cfg = Config::default();
        let p = chord("ctrl+t");
        assert_eq!(entries(&cfg, &p), entries(&cfg, &p));
    }

    #[test]
    fn drawing_shows_a_binding_and_its_action() {
        let mut buf = Buffer::empty(RRect::new(0, 0, 80, 24));
        draw(
            &mut buf,
            Rect::new(0, 0, 80, 24),
            &chord("ctrl+t"),
            &Config::default(),
        );
        let t = text(&buf);
        assert!(t.contains("new-tab"), "{t}");
        assert!(t.contains("toggle-zoom"), "{t}");
        // Nothing is painted over the status bar row.
        assert!(t.lines().last().unwrap().trim().is_empty());
    }

    #[test]
    fn tiny_terminals_do_not_panic_or_escape_the_buffer() {
        let cfg = Config::default();
        for (w, h) in [(1, 1), (2, 2), (20, 5), (20, 8), (8, 24), (200, 3)] {
            let mut buf = Buffer::empty(RRect::new(0, 0, w, h));
            draw(&mut buf, Rect::new(0, 0, w, h), &chord("ctrl+t"), &cfg);
            // A rect larger than the buffer must clip rather than write out.
            let mut buf = Buffer::empty(RRect::new(0, 0, w, h));
            draw(
                &mut buf,
                Rect::new(0, 0, w * 3, h * 3),
                &chord("ctrl+t"),
                &cfg,
            );
            assert_eq!(buf.area, RRect::new(0, 0, w, h));
        }
    }

    #[test]
    fn an_unbound_prefix_draws_nothing() {
        let mut buf = Buffer::empty(RRect::new(0, 0, 80, 24));
        draw(
            &mut buf,
            Rect::new(0, 0, 80, 24),
            &chord("ctrl+z"),
            &Config::default(),
        );
        assert!(text(&buf).trim().is_empty());
    }
}
