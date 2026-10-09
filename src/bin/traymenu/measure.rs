//! How wide a label really is, asked of the same text shaper that draws
//! it.
//!
//! The menu used to guess: `0.6 × font size` per character, and a fixed
//! 220px popup on the theory that tray labels are short. The battery row
//! is not ("Battery 77% — charging, full in 1 h 20 min"), and neither is
//! a Wi-Fi network somebody named in a sentence, so the estimate cut
//! labels that would have fitted and the fixed width left no room for
//! the ones that would not. Measuring needs no surface and no
//! `UserInterface` — `iced_graphics`'s paragraph shapes text through the
//! same cosmic-text font system the renderer draws from, which is what
//! makes the number it gives the number the row will occupy.

use iced_runtime::core::text::{self, LineHeight, Paragraph as _, Shaping, Text, Wrapping};
use iced_runtime::core::{alignment, Font, Pixels, Size};
use iced_tiny_skia::graphics::text::Paragraph;
use std::borrow::Cow;

const ELLIPSIS: char = '\u{2026}';

/// The shaped width of `label` on one line, in logical pixels — drawn
/// exactly as `view::row_element` draws it: the popup's default font
/// (`hyprforge-popup` builds its renderer with `Font::DEFAULT`), the
/// text widget's own default shaping, no wrapping.
pub fn width(label: &str, font_size: f32) -> f64 {
    let paragraph = Paragraph::with_text(Text {
        content: label,
        bounds: Size::INFINITE,
        size: Pixels(font_size),
        line_height: LineHeight::default(),
        font: Font::DEFAULT,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::default(),
        wrapping: Wrapping::None,
    });
    paragraph.min_bounds().width as f64
}

/// `label` as it fits in `available` logical pixels: whole when it
/// fits, otherwise as many characters as fit followed by an ellipsis.
///
/// Character-safe, never byte-safe — a label is user-controlled text (a
/// Wi-Fi SSID), and an arbitrary byte offset can split a character in
/// half. The cut is found by bisection over the character count, each
/// step measured, so the result is the longest prefix that fits rather
/// than an estimate of it.
pub fn fit(label: &str, font_size: f32, available: f64) -> Cow<'_, str> {
    if width(label, font_size) <= available {
        return Cow::Borrowed(label);
    }
    let chars: Vec<char> = label.chars().collect();
    let shown = |n: usize| -> String {
        let mut s: String = chars[..n].iter().collect::<String>().trim_end().to_owned();
        s.push(ELLIPSIS);
        s
    };
    // `lo` always fits (zero characters is just the ellipsis, which is
    // shown even when it does not: a row that says "…" says something
    // was there); `hi` never does.
    let (mut lo, mut hi) = (0, chars.len());
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if width(&shown(mid), font_size) <= available {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Cow::Owned(shown(lo))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: f32 = 14.0;

    #[test]
    fn a_label_that_fits_comes_back_untouched() {
        assert!(matches!(fit("Balanced", SIZE, 200.0), Cow::Borrowed("Balanced")));
    }

    /// The bisection's whole point: the longest prefix that fits, and
    /// not one character less.
    #[test]
    fn a_long_label_is_cut_to_the_longest_prefix_that_fits() {
        let long = "Battery 77% — charging, full in 1 h 20 min";
        let available = width(long, SIZE) * 0.6;
        let shown = fit(long, SIZE, available).into_owned();
        assert!(shown.ends_with(ELLIPSIS), "{shown}");
        assert!(width(&shown, SIZE) <= available, "{shown} is wider than {available}");
        // The next longer cut — past any space the cut trimmed — must
        // not have fitted.
        let kept = shown.trim_end_matches(ELLIPSIS).chars().count();
        let cut = |n: usize| long.chars().take(n).collect::<String>().trim_end().to_owned() + "\u{2026}";
        let one_more = (kept + 1..=long.chars().count())
            .map(cut)
            .find(|s| s.trim_end_matches(ELLIPSIS).chars().count() > kept)
            .unwrap();
        assert!(width(&one_more, SIZE) > available, "{one_more} would have fitted");
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let ssid = "Café ☕ Ünïcödé Gäste-WLAN";
        let shown = fit(ssid, SIZE, width(ssid, SIZE) / 2.0);
        assert!(shown.chars().all(|c| ssid.contains(c) || c == ELLIPSIS), "{shown}");
    }

    #[test]
    fn wider_text_measures_wider() {
        assert!(width("Wi-Fi settings…", SIZE) > width("Wi-Fi", SIZE));
        assert!(width("Wi-Fi", SIZE * 2.0) > width("Wi-Fi", SIZE));
    }
}
