use std::sync::mpsc;

pub(crate) enum Icon {
    Connected,
    Disconnected,
    UnreadMail,
    NewMail,
}

pub(crate) struct TrayIcon {
    app: systray::Application,
}

impl TrayIcon {
    pub(crate) fn new(tx: mpsc::Sender<Option<(usize, usize)>>) -> Result<Self, ()> {
        let mut icon = match systray::Application::new() {
            Ok(app) => Ok(Self { app }),
            Err(e) => {
                eprintln!("Could not create gtk application: {}", e);
                Err(())
            }
        }?;
        icon.set_icon(Icon::Disconnected);

        let tx = std::sync::Mutex::new(tx);
        if let Err(e) = icon.app.add_menu_item("Quit", move |window| {
            tx.lock().unwrap().send(None).unwrap();
            window.quit();
            Ok::<_, systray::Error>(())
        }) {
            eprintln!("Could not add application Quit menu option: {}", e);
        }

        Ok(icon)
    }

    pub(crate) fn set_icon(&self, icon: Icon) {
        let file = match icon {
            Icon::Connected => "/usr/share/icons/Faenza/stock/24/stock_connect.png",
            Icon::Disconnected => "/usr/share/icons/Faenza/stock/24/stock_disconnect.png",
            Icon::UnreadMail => "/usr/share/icons/oxygen/base/32x32/status/mail-unread.png",
            Icon::NewMail => "/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png",
        };
        if let Err(e) = self.app.set_icon_from_file(file) {
            eprintln!("Could not set application icon: {}", e);
        }
    }
}
