//! Simple cross-platform system tray with no bundled image assets.
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

pub enum TrayAction {
    Show,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

impl Tray {
    pub fn new() -> Result<Self, String> {
        let menu = Menu::new();
        let show = MenuItem::new("Открыть монитор", true, None);
        let quit = MenuItem::new("Завершить программу", true, None);
        menu.append(&show).map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_tooltip("PC Activity Scanner · монитор ресурсов")
            .with_icon(make_icon()?)
            .with_menu(Box::new(menu))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            _icon: icon,
            show_id: show.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    pub fn poll(&self) -> Option<TrayAction> {
        let mut action = None;
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show_id {
                action = Some(TrayAction::Show);
            } else if event.id == self.quit_id {
                action = Some(TrayAction::Quit);
            }
        }
        action
    }
}

fn make_icon() -> Result<Icon, String> {
    let icon = crate::ui::window_icon();
    Icon::from_rgba(icon.rgba, icon.width, icon.height).map_err(|e| e.to_string())
}
