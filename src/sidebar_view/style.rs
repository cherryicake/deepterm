use gpui::Hsla;

pub fn glass_bg(is_dark: bool) -> Hsla {
    if is_dark {
        Hsla::from(gpui::rgb(0x2C2C2E)).opacity(0.85)
    } else {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.85)
    }
}

pub fn border_color(is_dark: bool) -> Hsla {
    if is_dark {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.1)
    } else {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.6)
    }
}

pub fn hover_bg(is_dark: bool) -> Hsla {
    if is_dark {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.08)
    } else {
        Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
    }
}

pub fn button_bg(is_dark: bool) -> Hsla {
    if is_dark {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.10)
    } else {
        Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
    }
}

pub fn button_bg_hover(is_dark: bool) -> Hsla {
    if is_dark {
        Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.15)
    } else {
        Hsla::from(gpui::rgb(0x000000)).opacity(0.10)
    }
}
