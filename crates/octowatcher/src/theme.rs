//! Colours for both appearances: a rune-kit `Kit` for the components, plus the few roles the
//! app's own views paint (grouped cards, separators, statuses, repository monograms).

use octowatcher_core::appearance::Appearance;
use rune::{Color, color::hex, oklch};
use rune_kit::{Kit, PaletteSeed};

/// Whether the window draws dark: the saved appearance, following the desktop for System.
pub fn is_dark(appearance: Appearance, system_dark: bool) -> bool {
    match appearance {
        Appearance::Light => false,
        Appearance::Dark => true,
        Appearance::System => system_dark,
    }
}

#[derive(Clone)]
pub struct Theme {
    pub k: Kit,
    pub dark: bool,
    /// The window behind everything.
    pub bg: Color,
    /// Grouped lists and settings cards.
    pub card: Color,
    pub card_border: Color,
    pub separator: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    /// The accent as a filled surface under `on_accent`.
    pub fill: Color,
    /// Menus, dialogs and controls.
    pub raised: Color,
    pub on_accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    /// Tinted backgrounds behind status text.
    pub warning_bg: Color,
    pub hover: Color,
    pub pressed: Color,
    pub shadow: Color,
    /// One device pixel, in points.
    pub hairline: f32,
}

impl Theme {
    pub fn hair(&self) -> f32 {
        self.hairline
    }
}

const DARK: Palette = Palette {
    bg: "#161617",
    card: "#1f1f21",
    raised: "#2a2a2d",
    text: "#ededef",
    accent: "#7ea6ff",
    fill: "#3d6be0",
    on_accent: "#ffffff",
    success: "#5fd38d",
    warning: "#f2b35b",
    danger: "#ff7a85",
};

const LIGHT: Palette = Palette {
    bg: "#f2f2f3",
    card: "#ffffff",
    raised: "#ffffff",
    text: "#1c1c1e",
    accent: "#2a5fd6",
    fill: "#2a62e0",
    on_accent: "#ffffff",
    success: "#1d7a46",
    warning: "#9a5a00",
    danger: "#c4283c",
};

struct Palette {
    bg: &'static str,
    card: &'static str,
    /// Menus, dialogs, buttons and fields: white in light, lifted off the cards in dark.
    raised: &'static str,
    text: &'static str,
    /// Accent as text and icons.
    accent: &'static str,
    /// Accent as a filled surface (primary buttons, switches, badges) under white text.
    fill: &'static str,
    on_accent: &'static str,
    success: &'static str,
    warning: &'static str,
    danger: &'static str,
}

/// `t` of the way from `a` to `b` in sRGB-encoded space, as a browser or AppKit blends a
/// translucent tint over a surface. (`Color::mix` blends in linear light, which washes dark
/// surfaces out.)
pub fn blend(a: Color, b: Color, t: f32) -> Color {
    use rune::color::{srgb_decode, srgb_encode};
    let m = |x: f32, y: f32| srgb_decode(srgb_encode(x) + (srgb_encode(y) - srgb_encode(x)) * t);
    Color {
        r: m(a.r, b.r),
        g: m(a.g, b.g),
        b: m(a.b, b.b),
        a: a.a + (b.a - a.a) * t,
    }
}

fn c(s: &str) -> Color {
    hex(s).expect("theme colours are valid hex")
}

pub fn theme(dark: bool, scale: f32) -> Theme {
    let p = if dark { DARK } else { LIGHT };
    let (bg, card, text, accent) = (c(p.bg), c(p.card), c(p.text), c(p.accent));
    let mut k = Kit::from_palette(PaletteSeed {
        bg,
        fg: text,
        accent,
    });
    let (success, warning, danger) = (c(p.success), c(p.warning), c(p.danger));
    let tint = |color: Color, a: f32| blend(card, color, a);
    k.c.success.fg = success;
    k.c.warning.fg = warning;
    k.c.danger.fg = danger;
    k.c.text_accent = accent;
    k.c.icon_accent = accent;
    // Knobs stay white on both appearances, as AppKit's switches do.
    if !dark {
        // A step darker than the kit's derivation: AA on the #F2 window as well as the cards.
        k.c.text_muted = c("#646467");
    }
    k.c.text_faint = k.c.text_muted;
    // Pop-up buttons (kit Selects) take the native stacked chevrons.
    k.glyph.chevron_down = &crate::ui::icons::CHEVRON_UP_DOWN;
    k.c.knob = Color::WHITE;
    k.c.knob_on = Color::WHITE;
    // Elevated surfaces and controls read as raised: white in light mode, lighter than the
    // cards in dark mode. Primary (Contrast) buttons and switches take the accent fill.
    let (raised, fill, on_accent) = (c(p.raised), c(p.fill), c(p.on_accent));
    // Text fields sit a step into the page rather than raised like buttons.
    k.c.bg_base = if dark {
        blend(card, Color::BLACK, 0.18)
    } else {
        blend(bg, Color::WHITE, 0.55)
    };
    k.c.pressed = text.alpha(if dark { 0.07 } else { 0.09 });
    k.c.layer_01 = raised;
    k.c.surface_raised = if dark {
        blend(raised, text, 0.09)
    } else {
        raised
    };
    if dark {
        k.c.border_weak = blend(raised, text, 0.14);
    }
    k.c.std_border = if dark {
        blend(raised, text, 0.40)
    } else {
        k.c.std_border
    };
    for shadow in k.elevation.md.iter_mut() {
        if shadow.blur > 0.0 {
            shadow.color = Color::BLACK.alpha(if dark { 0.85 } else { 0.16 });
            if dark {
                shadow.blur *= 1.6;
                shadow.y *= 1.5;
            }
        }
    }
    k.c.bg_button_neutral = raised;
    k.c.bg_accent = fill;
    k.c.bg_contrast = fill;
    k.c.text_contrast = on_accent;
    k.c.icon_contrast = on_accent;
    // A clearly visible off-switch track on white cards.
    if !dark {
        k.c.layer_04 = c("#cdcdd3");
    }
    k.c.scrim = Color::BLACK.alpha(if dark { 0.5 } else { 0.22 });
    for shadow in k
        .elevation
        .floating
        .iter_mut()
        .chain(k.elevation.overlay.iter_mut())
    {
        if shadow.blur > 0.0 {
            shadow.color = Color::BLACK.alpha(if dark { 0.7 } else { 0.16 });
        }
    }
    // Menu rows (and other small medium text) on a native-tight line.
    k.ty.text_small_medium = rune::TextStyle::new(13.0, 500.0).lh(15.0);
    k.radius.sm = 6.0;
    k.radius.md = 7.0;
    k.radius.lg = 7.0;
    let ink = |a: f32| rune::theme::for_blend(text.alpha(a), card);
    Theme {
        dark,
        bg,
        card,
        card_border: ink(if dark { 0.09 } else { 0.08 }),
        separator: ink(0.07),
        text,
        muted: k.c.text_muted,
        // Placeholders and footnotes stay readable: tertiary text clears 4.5:1 on the window.
        faint: k.c.text_muted,
        accent,
        fill,
        raised,
        on_accent,
        success,
        warning,
        danger,
        warning_bg: tint(warning, if dark { 0.16 } else { 0.12 }),
        hover: ink(if dark { 0.045 } else { 0.035 }),
        pressed: ink(if dark { 0.08 } else { 0.06 }),
        shadow: Color::BLACK.alpha(if dark { 0.35 } else { 0.06 }),
        hairline: 1.0 / scale.max(1.0),
        k,
    }
}

/// The monogram colour for the owner in `slot` (its place among the owners the app knows), so
/// owners side by side never share a colour. `None` falls back to a hash of the name.
pub fn monogram(owner: &str, slot: Option<usize>, dark: bool) -> Color {
    // FNV-1a with a murmur finaliser, so similar names land far apart on the wheel.
    let mut hash = owner
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^= hash >> 16;
    // Hues that read well under white initials on both appearances, spread wide, with no
    // reds (they'd read as errors) and no muddy yellows.
    const HUES: [f32; 6] = [255.0, 165.0, 315.0, 70.0, 210.0, 125.0];
    let hue = HUES[slot.unwrap_or(hash as usize) % HUES.len()];
    if dark {
        oklch(0.60, 0.12, hue)
    } else {
        oklch(0.56, 0.13, hue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_follows_the_desktop_and_overrides_ignore_it() {
        for system_dark in [false, true] {
            assert_eq!(is_dark(Appearance::System, system_dark), system_dark);
            assert!(!is_dark(Appearance::Light, system_dark));
            assert!(is_dark(Appearance::Dark, system_dark));
        }
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (a, b) = (a.luminance(), b.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn small_text_including_statuses_has_readable_contrast() {
        for dark in [false, true] {
            let t = theme(dark, 2.0);
            for background in [t.bg, t.card] {
                for (name, foreground) in [
                    ("text", t.text),
                    ("muted", t.muted),
                    ("accent", t.accent),
                    ("success", t.success),
                    ("warning", t.warning),
                    ("danger", t.danger),
                ] {
                    let ratio = contrast(foreground, background);
                    assert!(
                        ratio >= 4.5,
                        "{name} {} on {} is {ratio:.2}:1",
                        foreground.hex(),
                        background.hex()
                    );
                }
                assert!(
                    contrast(t.faint, background) >= 4.5,
                    "faint {} on {}",
                    t.faint.hex(),
                    background.hex()
                );
            }
            for (fg, bg) in [(t.warning, t.warning_bg), (t.on_accent, t.fill)] {
                assert!(contrast(fg, bg) >= 4.5, "{} on {}", fg.hex(), bg.hex());
            }
        }
    }
}
