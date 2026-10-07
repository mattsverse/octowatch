use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};

/// Saved intent, rather than the last appearance reported by the desktop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const CHOICES: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn palette(self, system: WindowAppearance) -> Palette {
        match self {
            Self::Light => Palette::LIGHT,
            Self::Dark => Palette::DARK,
            Self::System => match system {
                WindowAppearance::Light | WindowAppearance::VibrantLight => Palette::LIGHT,
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Palette::DARK,
            },
        }
    }
}

/// Semantic roles shared by every app-owned view and control. Keep foregrounds
/// readable on both the resting and hover surfaces; don't dim whole subtrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: u32,
    pub surface: u32,
    pub surface_hover: u32,
    pub border: u32,
    pub text: u32,
    pub secondary_text: u32,
    pub muted_text: u32,
    pub snoozed_text: u32,
    pub accent: u32,
    pub on_accent: u32,
    pub focus: u32,
    pub success: u32,
    pub warning: u32,
    pub error: u32,
}

impl Palette {
    pub const DARK: Self = Self {
        background: 0x1e1e2e,
        surface: 0x313244,
        surface_hover: 0x3c3e50,
        border: 0x45475a,
        text: 0xcdd6f4,
        secondary_text: 0xa6adc8,
        muted_text: 0xa6adc8,
        snoozed_text: 0xa6adc8,
        accent: 0x89b4fa,
        on_accent: 0x1e1e2e,
        focus: 0x89b4fa,
        success: 0xa6e3a1,
        warning: 0xfab387,
        error: 0xf38ba8,
    };

    pub const LIGHT: Self = Self {
        background: 0xeff1f5,
        surface: 0xe6e9ef,
        surface_hover: 0xdce0e8,
        border: 0xbcc0cc,
        text: 0x4c4f69,
        secondary_text: 0x5c5f77,
        muted_text: 0x5c5f77,
        snoozed_text: 0x5c5f77,
        accent: 0x1e5fa5,
        on_accent: 0xffffff,
        focus: 0x1e5fa5,
        success: 0x2b6414,
        warning: 0x8d4709,
        error: 0xad2040,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_follows_appearance_and_overrides_ignore_it() {
        for (system, expected) in [
            (WindowAppearance::Light, Palette::LIGHT),
            (WindowAppearance::VibrantLight, Palette::LIGHT),
            (WindowAppearance::Dark, Palette::DARK),
            (WindowAppearance::VibrantDark, Palette::DARK),
        ] {
            assert_eq!(Appearance::System.palette(system), expected);
            assert_eq!(Appearance::Light.palette(system), Palette::LIGHT);
            assert_eq!(Appearance::Dark.palette(system), Palette::DARK);
        }
    }

    fn luminance(color: u32) -> f64 {
        let linear = |shift: u32| {
            let channel = ((color >> shift) & 0xffu32) as f64 / 255.;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn small_text_including_statuses_and_snoozes_has_readable_contrast() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            for background in [palette.background, palette.surface, palette.surface_hover] {
                for foreground in [
                    palette.text,
                    palette.secondary_text,
                    palette.muted_text,
                    palette.snoozed_text,
                    palette.accent,
                    palette.success,
                    palette.warning,
                    palette.error,
                ] {
                    assert!(
                        contrast(foreground, background) >= 4.5,
                        "{foreground:06x} on {background:06x} falls below 4.5:1"
                    );
                }
                assert!(contrast(palette.focus, background) >= 3.);
            }
            assert!(contrast(palette.on_accent, palette.accent) >= 4.5);
        }
    }
}
