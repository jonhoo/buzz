pub(crate) enum Icon {
    Connected,
    Disconnected,
    UnreadMail,
    NewMail,
}

pub(crate) struct TrayIcon {
    app: tray_item::TrayItem,
}

impl TrayIcon {
    pub(crate) fn new(default_icon: Icon) -> anyhow::Result<Self> {
        let icon = get_icon(default_icon);
        let tray = tray_item::TrayItem::new("Buzz", tray_item::IconSource::Resource(icon))?;

        Ok(TrayIcon { app: tray })
    }

    pub(crate) fn set_icon(&mut self, icon: Icon) -> anyhow::Result<()> {
        self.app
            .set_icon(tray_item::IconSource::Resource(get_icon(icon)))?;

        Ok(())
    }
}

pub(crate) fn get_icon(icon: Icon) -> &'static str {
    match icon {
        Icon::Connected => "/usr/share/icons/Faenza/stock/24/stock_connect.png",
        Icon::Disconnected => "/usr/share/icons/Faenza/stock/24/stock_disconnect.png",
        Icon::UnreadMail => "/usr/share/icons/oxygen/base/32x32/status/mail-unread.png",
        Icon::NewMail => "/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png",
    }
}
