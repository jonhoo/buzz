use serde::Deserialize;

pub(crate) enum Icon {
    Connected,
    Disconnected,
    UnreadMail,
    NewMail,
}

pub(crate) struct TrayIcon {
    icons: &'static Icons,
    app: tray_item::TrayItem,
}

#[derive(Clone, Deserialize)]
pub(crate) struct Icons {
    pub(crate) connected: String,
    pub(crate) disconnected: String,
    pub(crate) unread: String,
    pub(crate) new_mail: String,
}

impl Icons {
    fn get_icon(&self, icon: Icon) -> &str {
        match icon {
            Icon::Connected => &self.connected,
            Icon::Disconnected => &self.disconnected,
            Icon::UnreadMail => &self.unread,
            Icon::NewMail => &self.new_mail,
        }
    }

    fn get_default() -> Self {
        Self {
            connected: String::from("/usr/share/icons/Faenza/stock/24/stock_connect.png"),
            disconnected: String::from("/usr/share/icons/Faenza/stock/24/stock_disconnect.png"),
            unread: String::from("/usr/share/icons/oxygen/base/32x32/status/mail-unread.png"),
            new_mail: String::from("/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png"),
        }
    }
}

impl TrayIcon {
    pub(crate) fn new(icons: Option<Icons>, initial_icon: Icon) -> anyhow::Result<Self> {
        let icons = icons.unwrap_or_else(Icons::get_default);
        let leaked_icons: &'static Icons = Box::leak(Box::new(icons.clone())) as &'static Icons;
        let tray = tray_item::TrayItem::new(
            "Buzz",
            tray_item::IconSource::Resource(leaked_icons.get_icon(initial_icon)),
        )?;
        let tray_icon = TrayIcon {
            app: tray,
            icons: leaked_icons,
        };

        Ok(tray_icon)
    }

    pub(crate) fn set_icon(&mut self, icon: Icon) -> anyhow::Result<()> {
        let icon_loc = self.icons.get_icon(icon);
        self.app
            .set_icon(tray_item::IconSource::Resource(icon_loc))?;

        Ok(())
    }
}
