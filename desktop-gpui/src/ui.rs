use gpui_kit::{App, Global, Hsla, px, rgb};
use gpui_kit::component::{Theme, ThemeMode};

/// Shared colors for the native desktop interface.
pub struct Palette {
    dark: bool,
    pub bg: Hsla,
    pub panel: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub live: Hsla,
    pub accent_bg: Hsla,
    pub purple: Hsla,
    pub blue: Hsla,
    pub danger: Hsla,
    pub danger_bg: Hsla,
    pub warn_bg: Hsla,
    pub idle: Hsla,
    pub field: Hsla,
    pub subtle: Hsla,
    pub chip: Hsla,
    pub track: Hsla,
    pub row_line: Hsla,
    pub check_border: Hsla,
    pub on_accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_soft: Hsla,
    pub accent_border: Hsla,
    pub danger_hover: Hsla,
    pub danger_border: Hsla,
    pub danger_text: Hsla,
    pub warn_border: Hsla,
    pub warn_text: Hsla,
    pub info_bg: Hsla,
    pub info_border: Hsla,
    pub info_text: Hsla,
    pub code_bg: Hsla,
    pub code_text: Hsla,
}
impl Global for Palette {}

impl Palette {
    fn new(mode: ThemeMode) -> Self {
        let dark = mode.is_dark();
        let c = |light, dark_color| rgb(if dark { dark_color } else { light }).into();
        Self {
            dark,
            bg: c(0xf4f7f8, 0x12181c),
            panel: c(0xffffff, 0x1a2227),
            text: c(0x172c38, 0xe3ecef),
            muted: c(0x71818d, 0x8d9ca6),
            border: c(0xe1e8eb, 0x2b363d),
            accent: c(0x087f75, 0x3fb5a8),
            live: c(0x12a08f, 0x3ee6b4),
            accent_bg: c(0xe8f3f0, 0x17332f),
            purple: c(0x7755b6, 0xa58ae0),
            blue: c(0x356bc4, 0x6e9be6),
            danger: c(0xb25a43, 0xe08a72),
            danger_bg: c(0xfbeee9, 0x3a221c),
            warn_bg: c(0xfbf4e2, 0x352d17),
            idle: c(0x94a1aa, 0x66737b),
            field: c(0xfafcfc, 0x161d21),
            subtle: c(0xf5f8f9, 0x1f292e),
            chip: c(0xf0f3f5, 0x263137),
            track: c(0xe1e9e7, 0x263137),
            row_line: c(0xeef2f4, 0x232d33),
            check_border: c(0xb5c3cb, 0x4a5961),
            on_accent: c(0xffffff, 0x0b1f1c),
            accent_hover: c(0x066d64, 0x52c6b9),
            accent_soft: c(0xf3f8f7, 0x18302c),
            accent_border: c(0xdfeae6, 0x26504a),
            danger_hover: c(0x9a4a35, 0xea9f89),
            danger_border: c(0xe7c8bd, 0x5c3529),
            danger_text: c(0x8a3f2b, 0xf0a993),
            warn_border: c(0xecdca9, 0x5a4a1e),
            warn_text: c(0x7a6218, 0xe5c86b),
            info_bg: c(0xeef5fa, 0x172633),
            info_border: c(0xd3e2ee, 0x27405a),
            info_text: c(0x2f5878, 0x9cc3e6),
            code_bg: c(0x172c38, 0x0e1519),
            code_text: rgb(0xd8e6ea).into(),
        }
    }
}

pub fn palette(cx: &App) -> &Palette {
    cx.global::<Palette>()
}

pub fn apply(cx: &mut App) {
    let mode = Theme::global(cx).mode;
    if cx.try_global::<Palette>().is_some_and(|p| p.dark == mode.is_dark() && Theme::global(cx).colors.primary == p.accent) {
        return;
    }
    let p = Palette::new(mode);
    Theme::update(cx, |theme| {
        theme.font_size = px(14.);
        theme.mono_font_size = px(12.5);
        theme.radius = px(7.);
        theme.radius_lg = px(10.);
        theme.shadow = false;
        let c = &mut theme.colors;
        c.background = p.panel;
        c.foreground = p.text;
        c.border = p.border;
        c.input = p.border;
        c.accent = p.accent_bg;
        c.accent_foreground = p.accent;
        c.muted = p.subtle;
        c.muted_foreground = p.muted;
        c.primary = p.accent;
        c.primary_foreground = p.on_accent;
        c.primary_hover = p.accent_hover;
        c.primary_active = p.accent_hover;
        c.secondary = p.subtle;
        c.secondary_foreground = p.text;
        c.secondary_hover = p.chip;
        c.secondary_active = p.chip;
        c.button = p.panel;
        c.button_foreground = p.text;
        c.button_hover = p.subtle;
        c.button_active = p.chip;
        c.button_primary = p.accent;
        c.button_primary_foreground = p.on_accent;
        c.button_primary_hover = p.accent_hover;
        c.button_primary_active = p.accent_hover;
        c.button_secondary = p.subtle;
        c.button_secondary_foreground = p.text;
        c.button_secondary_hover = p.chip;
        c.button_secondary_active = p.chip;
        c.danger = p.danger;
        c.danger_foreground = p.on_accent;
        c.danger_hover = p.danger_hover;
        c.danger_active = p.danger_hover;
        c.button_danger = p.danger;
        c.button_danger_foreground = p.on_accent;
        c.button_danger_hover = p.danger_hover;
        c.button_danger_active = p.danger_hover;
        c.success = p.accent;
        c.success_foreground = p.on_accent;
        c.warning = p.warn_text;
        c.info = p.blue;
        c.info_foreground = p.on_accent;
        c.link = p.accent;
        c.ring = p.accent;
        c.selection = p.accent_bg;
        c.popover = p.panel;
        c.popover_foreground = p.text;
        c.sidebar = p.panel;
        c.sidebar_border = p.border;
        c.sidebar_foreground = p.text;
        c.sidebar_accent = p.subtle;
        c.sidebar_accent_foreground = p.text;
        c.sidebar_primary = p.accent;
        c.sidebar_primary_foreground = p.on_accent;
        c.list = p.panel;
        c.list_active = p.accent_bg;
        c.list_active_border = p.accent_border;
        c.list_even = p.panel;
        c.list_head = p.subtle;
        c.list_hover = p.subtle;
        c.tab = p.bg;
        c.tab_active = p.bg;
        c.tab_active_foreground = p.accent;
        c.tab_bar = p.bg;
        c.tab_foreground = p.muted;
        c.table = p.panel;
        c.table_active = p.accent_bg;
        c.table_active_border = p.accent_border;
        c.table_even = p.panel;
        c.table_head = p.subtle;
        c.table_head_foreground = p.muted;
        c.table_hover = p.subtle;
        c.table_row_border = p.row_line;
        c.title_bar = p.panel;
        c.title_bar_border = p.border;
        c.chart_1 = p.accent;
        c.chart_2 = p.purple;
        c.chart_3 = p.blue;
        c.progress_bar = p.accent;
        c.scrollbar_thumb = p.check_border;
        c.scrollbar_thumb_hover = p.muted;
    });
    cx.set_global(p);
}
