#![warn(rust_2018_idioms)]

use native_tls::{TlsConnector, TlsStream};
use rayon::prelude::*;

use std::borrow::Cow;
use std::collections::BTreeMap;
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
        imap::connect((&*self.server.0, self.server.1), &self.server.0, &tls).and_then(|c| {
            let mut c = c
                .login(self.username.trim(), self.password.trim())
                .map_err(|(e, _)| e)?;
            let cap = c.capabilities()?;
            if !cap.iter().any(|&c| c == "IDLE") {
                return Err(imap::error::Error::Bad(cap.iter().cloned().collect()));
            }
            c.select("INBOX")?;
            Ok(Connection {
                account: self.clone(),
                socket: c,
            })
        })
    }
}

struct Connection<T: Read + Write> {
    account: Account,
    socket: imap::Session<T>,
}

impl<T: Read + Write + imap::extensions::idle::SetReadTimeout> Connection<T> {
    pub fn handle(mut self, account: usize, mut tx: mpsc::Sender<(usize, usize)>) {
        loop {
            if let Err(e) = self.check(account, &mut tx) {
                // the connection has failed for some reason
                // try to log out (we probably can't)
                eprintln!("connection to {} failed: {:?}", self.account.name, e);
                let _ = self.socket.logout();
                break;
            }
        }

        // try to reconnect
        let mut wait = 1;
        for _ in 0..5 {
            eprintln!(
                "connection to {} lost; trying to reconnect...",
                self.account.name
            );
            match self.account.connect() {
                Ok(c) => {
                    println!("{} connection reestablished", self.account.name);
                    return c.handle(account, tx);
                }
                Err(e) => {
                    eprintln!("failed to connect to {}: {:?}", self.account.name, e);
                    thread::sleep(Duration::from_secs(wait));
                }
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
        let mut notification = None::<notify_rust::NotificationHandle>;

        loop {
            // check current state of inbox
            let mut uids = self.socket.uid_search("UNSEEN 1:*")?;
            let num_unseen = uids.len();
            if uids.iter().all(|&uid| uid <= last_notified) {
                // there are no messages we haven't already notified about
                uids.clear();
            }
            last_notified = std::cmp::max(last_notified, uids.iter().cloned().max().unwrap_or(0));

            let mut subjects = BTreeMap::new();
            if !uids.is_empty() {
                let uids: Vec<_> = uids.into_iter().map(|v: u32| format!("{}", v)).collect();
                for msg in self
                    .socket
                    .uid_fetch(&uids.join(","), "RFC822.HEADER")?
                    .iter()
                {
                    let msg = msg.header();
                    if msg.is_none() {
                        continue;
                    }

                    match mailparse::parse_headers(msg.unwrap()) {
                        Ok((headers, _)) => {
                            use mailparse::MailHeaderMap;

                            let subject = match headers.get_first_value("Subject") {
                                Ok(Some(subject)) => Cow::from(subject),
                                Ok(None) => Cow::from("<no subject>"),
                                Err(e) => {
                                    println!("failed to get message subject: {:?}", e);
                                    continue;
                                }
                            };

                            let date = match headers.get_first_value("Date") {
                                Ok(Some(date)) => {
                                    match chrono::DateTime::parse_from_rfc2822(&date) {
                                        Ok(date) => date.with_timezone(&chrono::Local),
                                        Err(e) => {
                                            println!("failed to parse message date: {:?}", e);
                                            chrono::Local::now()
                                        }
                                    }
                                }
                                Ok(None) => chrono::Local::now(),
                                Err(e) => {
                                    println!("failed to get message date: {:?}", e);
                                    continue;
                                }
                            };

                            subjects.insert(date, subject);
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

                // we want the n newest e-mail in reverse chronological order
                let mut body = String::new();
                for subject in subjects.values().rev() {
                    body.push_str("> ");
                    body.push_str(subject);
                    body.push_str("\n");
                }
                let body = body.trim_end();

                println!("! {}", title);
                println!("{}", body);
                if let Some(mut n) = notification.take() {
                    n.summary(&title).body(&format!(
                        "{}",
                        askama_escape::escape(body, askama_escape::Html)
                    ));
                    n.update();
                } else {
                    notification = Some(
                        Notification::new()
                            .summary(&title)
                            .body(&format!(
                                "{}",
                                askama_escape::escape(body, askama_escape::Html)
                            ))
                            .icon("notification-message-email")
                            .hint(NotificationHint::Category("email.arrived".to_owned()))
                            .id(42) // for some reason, just updating isn't enough for dunst
                            .show()
                            .expect("failed to launch notify-send"),
                    );
                }
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
    let accounts: Vec<_> = match config.get("account") {
        Some(a) => {
            match a.as_array() {
                None => {
                    println!("No accounts were found");
                    return;
                },
                Some(a) => {
                    a.iter()
                    .filter_map(|account| {
                        let pwcmd = match account.get("pwcmd").and_then(|p| p.as_str()) {
                            None => return None,
                            Some(pwcmd) => pwcmd,
                        };

                        let password = match Command::new("sh").arg("-c").arg(pwcmd).output() {
                            Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
                            Err(e) => {
                                println!("Failed to launch password command for {}: {}", account["name"], e);
                                return None;
                            }
                        };

                        Some(Account {
                            name: account["name"].as_str().unwrap().to_owned(),
                            server: (
                                account["server"].as_str().unwrap().to_owned(),
                                account["port"].as_integer().unwrap() as u16,
                                ),
                                username: account["username"].as_str().unwrap().to_owned(),
                                password: password,
                        })
                    }).collect()
                }
            }
        }
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
        })
        .collect();

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
            )
            .unwrap();
        } else {
            app.set_icon_from_file(
                &"/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png".to_string(),
            )
            .unwrap();
        }
    }
}
