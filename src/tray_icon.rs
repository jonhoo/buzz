pub(crate) enum Icon {
    Connected,
    Disconnected,
    UnreadMail,
    NewMail,
}

pub(crate) struct TrayIcon {
    icons: Icons,
    app: tray_item::TrayItem,
}

pub(crate) struct Icons {
    pub(crate) connected: &'static str,
    pub(crate) disconnected: &'static str,
    pub(crate) unread: &'static str,
    pub(crate) new_mail: &'static str,
}

impl Icons {
    fn get_icon(&self, icon: Icon) -> &'static str {
        match icon {
            Icon::Connected => self.connected,
            Icon::Disconnected => self.disconnected,
            Icon::UnreadMail => self.unread,
            Icon::NewMail => self.new_mail,
        }
    }
}

pub(crate) const DEFAULT_ICONS: Icons = Icons {
    connected: "/usr/share/icons/Faenza/stock/24/stock_connect.png",
    disconnected: "/usr/share/icons/Faenza/stock/24/stock_disconnect.png",
    unread: "/usr/share/icons/oxygen/base/32x32/status/mail-unread.png",
    new_mail: "/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png",
};

impl TrayIcon {
    pub(crate) fn new(icons: Icons) -> anyhow::Result<Self> {
        let tray =
            tray_item::TrayItem::new("Buzz", tray_item::IconSource::Resource(icons.disconnected))?;

        Ok(TrayIcon { app: tray, icons })
    }

    pub(crate) fn set_icon(&mut self, icon: Icon) -> anyhow::Result<()> {
        let icon_loc = self.icons.get_icon(icon);
        self.app
            .set_icon(tray_item::IconSource::Resource(icon_loc))?;

        Ok(())
    }
}
