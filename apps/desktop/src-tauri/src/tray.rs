//! The system-tray icon: left-click toggles the quick-search popup; the menu has
//! Quick search, Show Athrix and Quit.
//!
//! Menu labels follow the UI language. The language lives in the frontend (local storage), so
//! each window emits [`LOCALE_EVENT`] with `"ar"` or `"en"` at start-up and whenever the user
//! switches; until then the labels are Arabic, the UI's default language.

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Listener, Runtime};

/// Event the frontend emits with the UI language (`"ar"` or `"en"`).
pub const LOCALE_EVENT: &str = "app://locale";

const TRAY_ID: &str = "athrix-tray";
const TOOLTIP: &str = "Athrix";
const ITEM_QUICK: &str = "quick-search";
const ITEM_SHOW: &str = "show-main";
const ITEM_QUIT: &str = "quit";

/// Menu labels in one language.
struct Labels {
    quick: &'static str,
    show: &'static str,
    quit: &'static str,
}

const AR: Labels = Labels {
    quick: "بحث سريع",
    show: "إظهار أثر",
    quit: "إنهاء",
};
const EN: Labels = Labels {
    quick: "Quick search",
    show: "Show Athrix",
    quit: "Quit",
};

fn labels(locale: &str) -> &'static Labels {
    if locale == "en" { &EN } else { &AR }
}

/// The menu items whose text changes with the language.
struct TrayMenu<R: Runtime> {
    quick: MenuItem<R>,
    show: MenuItem<R>,
    quit: MenuItem<R>,
}

impl<R: Runtime> TrayMenu<R> {
    fn set_locale(&self, locale: &str) {
        let l = labels(locale);
        for (item, text) in [
            (&self.quick, l.quick),
            (&self.show, l.show),
            (&self.quit, l.quit),
        ] {
            if let Err(err) = item.set_text(text) {
                tracing::warn!("couldn't relabel a tray menu item: {err}");
            }
        }
    }
}

/// Creates the tray icon. An error here is logged by the caller; the app runs without a tray.
pub fn create<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let l = labels("ar");
    let items = TrayMenu {
        quick: MenuItem::with_id(app, ITEM_QUICK, l.quick, true, None::<&str>)?,
        show: MenuItem::with_id(app, ITEM_SHOW, l.show, true, None::<&str>)?,
        quit: MenuItem::with_id(app, ITEM_QUIT, l.quit, true, None::<&str>)?,
    };
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&items.quick, &items.show, &separator, &items.quit])?;

    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(TOOLTIP)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;

    app.listen_any(LOCALE_EVENT, move |event| {
        match serde_json::from_str::<String>(event.payload()) {
            Ok(locale) => items.set_locale(&locale),
            Err(err) => tracing::warn!("bad {LOCALE_EVENT} payload: {err}"),
        }
    });
    Ok(())
}

fn on_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        ITEM_QUICK => crate::popup::show(app),
        ITEM_SHOW => crate::popup::show_main(app),
        ITEM_QUIT => app.exit(0),
        _ => {}
    }
}

fn on_tray_event<R: Runtime>(tray: &TrayIcon<R>, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        crate::popup::toggle(tray.app_handle());
    }
}
