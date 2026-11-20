//! The three styles of the Python app. HOT = accent under the mouse, INK = text on the accent, SEL = selected
//! rows and text, SECOND = the second colour (section titles and links).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub struct Theme {
    pub name: &'static str,
    pub bg: Color32,
    pub panel: Color32,
    pub panel2: Color32,
    pub fg: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub hot: Color32,
    pub ink: Color32,
    pub hover: Color32,
    pub sel: Color32,
    pub banner: Color32,
    pub second: Color32,
}

const fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

pub const THEMES: [Theme; 3] = [
    Theme {
        name: "Cyberpunk",
        bg: rgb(0x0e1016),
        panel: rgb(0x181b24),
        panel2: rgb(0x232837),
        fg: rgb(0xe8e8e8),
        dim: rgb(0x8a8f9c),
        accent: rgb(0xfcee0a),
        hot: rgb(0xfff35c),
        ink: rgb(0x000000),
        hover: rgb(0x30364a),
        sel: rgb(0x3a3f12),
        banner: rgb(0x2b2610),
        second: rgb(0x00e5ff),
    },
    Theme {
        name: "Arasaka",
        bg: rgb(0x0a0a0b),
        panel: rgb(0x151517),
        panel2: rgb(0x212125),
        fg: rgb(0xf2f2f2),
        dim: rgb(0x8e8e96),
        accent: rgb(0xe3122d),
        hot: rgb(0xff3b52),
        ink: rgb(0xffffff),
        hover: rgb(0x33333a),
        sel: rgb(0x4a0c15),
        banner: rgb(0x2a0b10),
        second: rgb(0xd8d8de),
    },
    Theme {
        name: "Militech",
        bg: rgb(0x0f120d),
        panel: rgb(0x191e16),
        panel2: rgb(0x252c20),
        fg: rgb(0xe4e8dc),
        dim: rgb(0x8c947e),
        accent: rgb(0xf29d35),
        hot: rgb(0xffb85c),
        ink: rgb(0x000000),
        hover: rgb(0x343d2c),
        sel: rgb(0x3e3216),
        banner: rgb(0x2b2311),
        second: rgb(0xc5d86d),
    },
];

pub const OK: Color32 = rgb(0x3ddc84);
pub const BAD: Color32 = rgb(0xff6b6b);
pub const WARN: Color32 = rgb(0xffb347);

/// The named style, or Cyberpunk for an unknown name.
pub fn theme(name: &str) -> &'static Theme {
    THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

/// Dark visuals in the style's colours, clear spacing and readable sizes.
pub fn apply(ctx: &egui::Context, t: &Theme) {
    let mut v = Visuals::dark();
    v.panel_fill = t.bg;
    v.window_fill = t.panel;
    v.extreme_bg_color = t.panel;
    v.faint_bg_color = t.panel2;
    v.code_bg_color = t.panel;
    v.hyperlink_color = t.second;
    v.override_text_color = Some(t.fg);
    v.selection.bg_fill = t.sel;
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.window_stroke = Stroke::new(1.0, t.panel2);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    let r = CornerRadius::same(5);
    for (w, fill, line) in [
        (&mut v.widgets.noninteractive, t.bg, t.panel2),
        (&mut v.widgets.inactive, t.panel2, t.panel2),
        (&mut v.widgets.hovered, t.hover, t.accent),
        (&mut v.widgets.active, t.hover, t.accent),
        (&mut v.widgets.open, t.panel2, t.dim),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, line);
        w.fg_stroke = Stroke::new(1.5, t.fg);
        w.corner_radius = r;
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.fg);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, t.accent);
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.scroll.bar_width = 8.0;
        s.text_styles = [
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
        ]
        .into();
    });
}

/// Segoe UI for text, Segoe UI Symbol for the marks, Consolas for the log and the icon font of the rail
/// (Windows 11 name first, then the Windows 10 one), all by full path from the Windows fonts folder.
pub fn fonts(ctx: &egui::Context) {
    let dir = shared::sys32().parent().map(|p| p.join("Fonts")).unwrap_or_default();
    let mut f = egui::FontDefinitions::default();
    let mut add = |key: &str, files: &[&str]| {
        let data = files.iter().find_map(|n| std::fs::read(dir.join(n)).ok())?;
        f.font_data.insert(key.into(), egui::FontData::from_owned(data).into());
        Some(())
    };
    let text = add("segoe", &["segoeui.ttf"]).is_some();
    let sym = add("symbol", &["seguisym.ttf"]).is_some();
    let mono = add("consolas", &["consola.ttf"]).is_some();
    let icons = add("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]).is_some();
    let prop = f.families.entry(FontFamily::Proportional).or_default();
    if sym {
        prop.insert(0, "symbol".into());
    }
    if text {
        prop.insert(0, "segoe".into());
    }
    if mono {
        f.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    let mut rail = vec![];
    if icons {
        rail.push("icons".to_string());
    }
    rail.extend(f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    f.families.insert(FontFamily::Name("icons".into()), rail);
    ctx.set_fonts(f);
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}
