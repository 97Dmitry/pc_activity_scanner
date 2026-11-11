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
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = (y * SIZE + x) * 4;
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if dx * dx + dy * dy <= 15.0 * 15.0 {
                rgba[idx..idx + 4].copy_from_slice(&[22, 32, 55, 255]);
                let height = if x < 11 {
                    11
                } else if x < 18 {
                    18
                } else {
                    14
                };
                if (7..25).contains(&x) && y >= 27 - height && y <= 26 && x % 7 < 5 {
                    rgba[idx..idx + 4].copy_from_slice(&[57, 209, 166, 255]);
                }
            }
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).map_err(|e| e.to_string())
}
