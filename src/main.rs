extern crate imap;
extern crate mailparse;
extern crate native_tls;
extern crate notify_rust;
extern crate rayon;
extern crate systray;
extern crate toml;
extern crate xdg;

use native_tls::{TlsConnector, TlsStream};
use rayon::prelude::*;

use std::fs::File;
use std::io::prelude::*;
use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Clone)]
struct Account {
    name: String,
    server: (String, u16),
    username: String,
    password: String,
}

impl Account {
    pub fn connect(&self) -> Result<Connection<TlsStream<TcpStream>>, imap::error::Error> {
        let tls = TlsConnector::builder().build()?;
        imap::client::secure_connect((&*self.server.0, self.server.1), &self.server.0, &tls)
            .and_then(|c| {
                let mut c = try!(
                    c.login(self.username.trim(), self.password.trim())
                        .map_err(|(e, _)| e)
                );
                let cap = try!(c.capabilities());
                if !cap.iter().any(|&c| c == "IDLE") {
                    return Err(imap::error::Error::BadResponse(
                        cap.iter().cloned().collect(),
                    ));
                }
                try!(c.select("INBOX"));
                Ok(Connection {
                    account: self.clone(),
                    socket: c,
                })
            })
    }
}

struct Connection<T: Read + Write> {
    account: Account,
    socket: imap::client::Session<T>,
}

impl<T: Read + Write + imap::client::SetReadTimeout> Connection<T> {
    pub fn handle(mut self, account: usize, mut tx: mpsc::Sender<(usize, usize)>) {
        loop {
            if let Err(_) = self.check(account, &mut tx) {
                // the connection has failed for some reason
                // try to log out (we probably can't)
                self.socket.logout().is_err();
                break;
            }
        }

        // try to reconnect
        let mut wait = 1;
        for _ in 0..5 {
            println!(
                "connection to {} lost; trying to reconnect...",
                self.account.name
            );
            match self.account.connect() {
                Ok(c) => {
                    println!("{} connection reestablished", self.account.name);
                    return c.handle(account, tx);
                }
                Err(imap::error::Error::Io(_)) => {
                    thread::sleep(Duration::from_secs(wait));
                }
                Err(_) => break,
            }

            wait *= 2;
        }
    }

    fn check(
        &mut self,
        account: usize,
        tx: &mut mpsc::Sender<(usize, usize)>,
    ) -> Result<(), imap::error::Error> {
        // Keep track of all the e-mails we have already notified about
        let mut last_notified = 0;

        loop {
            // check current state of inbox
            let mut unseen = self
                .socket
                .run_command_and_read_response("UID SEARCH UNSEEN 1:*")?;

            // remove last line of response (OK Completed)
            unseen.pop();

            let mut num_unseen = 0;
            let mut uids = Vec::new();
            let unseen = ::std::str::from_utf8(&unseen[..]).unwrap();
            let unseen = unseen.split_whitespace().skip(2);
            for uid in unseen.take_while(|&e| e != "" && e != "Completed") {
                if let Ok(uid) = usize::from_str_radix(uid, 10) {
                    if uid > last_notified {
                        last_notified = uid;
                        uids.push(format!("{}", uid));
                    }
                    num_unseen += 1;
                }
            }

            let mut subjects = Vec::new();
            if !uids.is_empty() {
                for msg in self
                    .socket
                    .uid_fetch(&uids.join(","), "RFC822.HEADER")?
                    .iter()
                {
                    let msg = msg.rfc822_header();
                    if msg.is_none() {
                        continue;
                    }

                    match mailparse::parse_headers(msg.unwrap()) {
                        Ok((headers, _)) => {
                            use mailparse::MailHeaderMap;
                            match headers.get_first_value("Subject") {
                                Ok(Some(subject)) => {
                                    subjects.push(subject);
                                }
                                Ok(None) => {
                                    subjects.push(String::from("<no subject>"));
                                }
                                Err(e) => {
                                    println!("failed to get message subject: {:?}", e);
                                }
                            }
                        }
                        Err(e) => println!("failed to parse headers of message: {:?}", e),
                    }
                }
            }

            if !subjects.is_empty() {
                use notify_rust::{Notification, NotificationHint};
                let title = format!(
                    "@{} has new mail ({} unseen)",
                    self.account.name, num_unseen
                );
                let notification = format!("> {}", subjects.join("\n> "));
                println!("! {}", title);
                println!("{}", notification);
                Notification::new()
                    .summary(&title)
                    .body(&notification)
                    .icon("notification-message-email")
                    .hint(NotificationHint::Category("email".to_owned()))
                    .timeout(-1)
                    .show()
                    .expect("failed to launch notify-send");
            }

            tx.send((account, num_unseen)).unwrap();

            // IDLE until we see changes
            self.socket.idle()?.wait_keepalive()?;
        }
    }
}

fn main() {
    // Load the user's config
    let xdg = match xdg::BaseDirectories::new() {
        Ok(xdg) => xdg,
        Err(e) => {
            println!("Could not find configuration file buzz.toml: {}", e);
            return;
        }
    };
    let config = match xdg.find_config_file("buzz.toml") {
        Some(config) => config,
        None => {
            println!("Could not find configuration file buzz.toml");
            return;
        }
    };
    let config = {
        let mut f = match File::open(config) {
            Ok(f) => f,
            Err(e) => {
                println!("Could not open configuration file buzz.toml: {}", e);
                return;
            }
        };
        let mut s = String::new();
        if let Err(e) = f.read_to_string(&mut s) {
            println!("Could not read configuration file buzz.toml: {}", e);
            return;
        }
        match s.parse::<toml::Value>() {
            Ok(t) => t,
            Err(e) => {
                println!("Could not parse configuration file buzz.toml: {}", e);
                return;
            }
        }
    };

    // Figure out what accounts we have to deal with
    let accounts: Vec<_> = match config.as_table() {
        Some(t) => t
            .iter()
            .filter_map(|(name, v)| match v.as_table() {
                None => {
                    println!("Configuration for account {} is broken: not a table", name);
                    None
                }
                Some(t) => {
                    let pwcmd = match t.get("pwcmd").and_then(|p| p.as_str()) {
                        None => return None,
                        Some(pwcmd) => pwcmd,
                    };

                    let password = match Command::new("sh").arg("-c").arg(pwcmd).output() {
                        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
                        Err(e) => {
                            println!("Failed to launch password command for {}: {}", name, e);
                            return None;
                        }
                    };

                    Some(Account {
                        name: name.as_str().to_owned(),
                        server: (
                            t["server"].as_str().unwrap().to_owned(),
                            t["port"].as_integer().unwrap() as u16,
                        ),
                        username: t["username"].as_str().unwrap().to_owned(),
                        password: password,
                    })
                }
            }).collect(),
        None => {
            println!("Could not parse configuration file buzz.toml: not a table");
            return;
        }
    };

    if accounts.is_empty() {
        println!("No accounts in config; exiting...");
        return;
    }

    // Create a new application
    let mut app = match systray::Application::new() {
        Ok(app) => app,
        Err(e) => {
            println!("Could not create gtk application: {}", e);
            return;
        }
    };
    if let Err(e) =
        app.set_icon_from_file(&"/usr/share/icons/Faenza/stock/24/stock_disconnect.png".to_string())
    {
        println!("Could not set application icon: {}", e);
    }
    if let Err(e) = app.add_menu_item(&"Quit".to_string(), |window| {
        window.quit();
    }) {
        println!("Could not add application Quit menu option: {}", e);
    }

    // TODO: w.set_tooltip(&"Whatever".to_string());
    // TODO: app.wait_for_message();

    let accounts: Vec<_> = accounts
        .par_iter()
        .filter_map(|account| {
            let mut wait = 1;
            for _ in 0..5 {
                match account.connect() {
                    Ok(c) => return Some(c),
                    Err(imap::error::Error::Io(e)) => {
                        println!(
                            "Failed to connect account {}: {}; retrying in {}s",
                            account.name, e, wait
                        );
                        thread::sleep(Duration::from_secs(wait));
                    }
                    Err(e) => {
                        println!("{} host produced bad IMAP tunnel: {:?}", account.name, e);
                        break;
                    }
                }

                wait *= 2;
            }

            None
        }).collect();

    if accounts.is_empty() {
        println!("No accounts in config worked; exiting...");
        return;
    }

    // We have now connected
    app.set_icon_from_file(&"/usr/share/icons/Faenza/stock/24/stock_connect.png".to_string())
        .ok();

    let (tx, rx) = mpsc::channel();
    let mut unseen: Vec<_> = accounts.iter().map(|_| 0).collect();
    for (i, conn) in accounts.into_iter().enumerate() {
        let tx = tx.clone();
        thread::spawn(move || {
            conn.handle(i, tx);
        });
    }

    for (i, num_unseen) in rx {
        unseen[i] = num_unseen;
        if unseen.iter().sum::<usize>() == 0 {
            app.set_icon_from_file(
                &"/usr/share/icons/oxygen/base/32x32/status/mail-unread.png".to_string(),
            ).unwrap();
        } else {
            app.set_icon_from_file(
                &"/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png".to_string(),
            ).unwrap();
        }
    }
}
