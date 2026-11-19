//! Dark and light colours for salash's own chrome. Tabs keep the host
//! terminal's colours; this only matches the sidebar and bars to them.

use ratatui::style::Color;
use std::sync::OnceLock;

pub type Rgb = (u8, u8, u8);

pub struct Theme {
    pub bg_side: Color,
    pub bg_sel: Color,
    pub bg_tab: Color,
    pub bg_tab_active: Color,
    pub bg_tabbar: Color,
    pub bg_status: Color,
    pub border: Color,
    pub fg: Color,
    pub fg_dim: Color,
    /// Selected issue, active tab.
    pub fg_strong: Color,
    pub fg_tab: Color,
    /// Text on accent, amber, orange and fg backgrounds.
    pub on_accent: Color,
    pub orange: Color,
    pub amber: Color,
    /// Working Claude tab.
    pub yellow: Color,
    pub green: Color,
    pub red: Color,
    pub accent: Color,
    /// What tabs are told when they ask for the background/foreground (OSC 11/10).
    pub pane_bg: Rgb,
    pub pane_fg: Rgb,
}

pub const DARK: Theme = Theme {
    bg_side: Color::Rgb(22, 22, 26),
    bg_sel: Color::Rgb(48, 48, 58),
    bg_tab: Color::Rgb(34, 34, 40),
    bg_tab_active: Color::Rgb(78, 78, 90),
    bg_tabbar: Color::Rgb(16, 16, 20),
    bg_status: Color::Rgb(28, 28, 34),
    border: Color::Rgb(50, 50, 58),
    fg: Color::Rgb(205, 205, 214),
    fg_dim: Color::Rgb(120, 120, 132),
    fg_strong: Color::White,
    fg_tab: Color::Rgb(160, 160, 172),
    on_accent: Color::Black,
    orange: Color::Rgb(217, 119, 87), // Claude orange
    amber: Color::Rgb(245, 180, 60),
    yellow: Color::Rgb(230, 220, 80),
    green: Color::Rgb(120, 200, 120),
    red: Color::Rgb(220, 95, 95),
    accent: Color::Rgb(140, 170, 255),
    pane_bg: (0, 0, 0),
    pane_fg: (229, 229, 229),
};

pub const LIGHT: Theme = Theme {
    bg_side: Color::Rgb(244, 244, 247),
    bg_sel: Color::Rgb(222, 224, 233),
    bg_tab: Color::Rgb(232, 232, 237),
    bg_tab_active: Color::Rgb(200, 202, 214),
    bg_tabbar: Color::Rgb(248, 248, 250),
    bg_status: Color::Rgb(234, 234, 239),
    border: Color::Rgb(208, 208, 216),
    fg: Color::Rgb(40, 40, 48),
    fg_dim: Color::Rgb(112, 112, 124),
    fg_strong: Color::Black,
    fg_tab: Color::Rgb(84, 84, 96),
    on_accent: Color::White,
    orange: Color::Rgb(196, 90, 55),
    amber: Color::Rgb(176, 108, 0),
    yellow: Color::Rgb(140, 125, 0),
    green: Color::Rgb(30, 130, 50),
    red: Color::Rgb(190, 45, 45),
    accent: Color::Rgb(50, 90, 210),
    pane_bg: (255, 255, 255),
    pane_fg: (0, 0, 0),
};

/// Above this the background counts as light (terminal-light's suggested pivot).
const LIGHT_LUMA: f32 = 0.6;

static CURRENT: OnceLock<Theme> = OnceLock::new();

/// The theme picked at startup; dark until then.
pub fn get() -> &'static Theme {
    CURRENT.get().unwrap_or(&DARK)
}

/// Pick the theme for the `theme` setting. Call before the TUI takes over
/// stdin: "auto" asks the terminal for its background.
pub fn init(setting: &str) {
    let _ = CURRENT.set(resolve(setting, detect));
}

fn resolve(setting: &str, detect: impl FnOnce() -> Option<(f32, Option<Rgb>)>) -> Theme {
    let base = |light: bool| if light { LIGHT } else { DARK };
    match setting {
        "dark" => DARK,
        "light" => LIGHT,
        _ => match detect() {
            Some((luma, bg)) => {
                let t = base(luma > LIGHT_LUMA);
                Theme { pane_bg: bg.unwrap_or(t.pane_bg), ..t }
            }
            None => DARK,
        },
    }
}

/// The terminal's background: its luma and, if it answered OSC 11, the colour.
fn detect() -> Option<(f32, Option<Rgb>)> {
    let c = terminal_light::background_color().ok()?;
    let rgb = matches!(c, terminal_light::Color::Rgb(_)).then(|| {
        let c = c.rgb();
        (c.r, c.g, c.b)
    });
    Some((c.luma(), rgb))
}

/// xterm's default colours for ANSI 0–15; the host's palette may differ.
const ANSI: [Rgb; 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// `c` as RGB, with `reset` standing in for the terminal's default.
pub fn to_rgb(c: Color, reset: Rgb) -> Rgb {
    let i = match c {
        Color::Reset => return reset,
        Color::Rgb(r, g, b) => return (r, g, b),
        Color::Indexed(i) => i,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    };
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

/// `from` moved `amount` (0–1) of the way to `to`.
pub fn blend(from: Rgb, to: Rgb, amount: f32) -> Color {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bg(t: &Theme) -> Color {
        t.bg_side
    }

    #[test]
    fn colours_resolve_to_rgb() {
        assert_eq!(to_rgb(Color::Reset, (1, 2, 3)), (1, 2, 3));
        assert_eq!(to_rgb(Color::Rgb(9, 8, 7), (0, 0, 0)), (9, 8, 7));
        assert_eq!(to_rgb(Color::Red, (0, 0, 0)), (205, 0, 0));
        assert_eq!(to_rgb(Color::Indexed(9), (0, 0, 0)), (255, 0, 0));
        assert_eq!(to_rgb(Color::Indexed(16), (0, 0, 0)), (0, 0, 0));
        assert_eq!(to_rgb(Color::Indexed(196), (0, 0, 0)), (255, 0, 0));
        assert_eq!(to_rgb(Color::Indexed(231), (0, 0, 0)), (255, 255, 255));
        assert_eq!(to_rgb(Color::Indexed(232), (0, 0, 0)), (8, 8, 8));
        assert_eq!(to_rgb(Color::Indexed(255), (0, 0, 0)), (238, 238, 238));
    }

    #[test]
    fn blending_moves_toward_either_background() {
        assert_eq!(blend((200, 200, 200), (0, 0, 0), 0.0), Color::Rgb(200, 200, 200));
        assert_eq!(blend((200, 200, 200), (0, 0, 0), 1.0), Color::Rgb(0, 0, 0));
        assert_eq!(blend((200, 100, 0), (0, 0, 0), 0.5), Color::Rgb(100, 50, 0));
        // Dark text on a light background gets lighter.
        assert_eq!(blend((40, 40, 48), (255, 255, 255), 0.5), Color::Rgb(148, 148, 152));
    }

    #[test]
    fn explicit_settings_skip_detection() {
        let never = || -> Option<(f32, Option<Rgb>)> { panic!("detected") };
        assert_eq!(bg(&resolve("dark", never)), DARK.bg_side);
        assert_eq!(bg(&resolve("light", never)), LIGHT.bg_side);
    }

    #[test]
    fn auto_follows_the_terminal() {
        let light = resolve("auto", || Some((0.95, Some((250, 250, 250)))));
        assert_eq!(bg(&light), LIGHT.bg_side);
        assert_eq!(light.pane_bg, (250, 250, 250));
        assert_eq!(bg(&resolve("auto", || Some((0.1, None)))), DARK.bg_side);
        assert_eq!(bg(&resolve("auto", || None)), DARK.bg_side);
        assert_eq!(bg(&resolve("bogus", || Some((0.9, None)))), LIGHT.bg_side);
    }
}
