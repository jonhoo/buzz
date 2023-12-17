#![warn(rust_2018_idioms)]

use anyhow::Context;
use rayon::prelude::*;
use rustls::{ClientConnection, StreamOwned};

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::prelude::*;
use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use directories_next::ProjectDirs;

#[cfg(feature = "systray")]
mod tray_icon;

#[derive(Clone)]
struct Account {
    name: String,
    server: (String, u16),
    username: String,
    password: String,
    notification_command: Option<String>,
    folders: Vec<String>,
}

#[derive(Clone)]
struct AccountFolder {
    account: Arc<Account>,
    folder: String,
}

impl AccountFolder {
    pub fn connect(&self) -> anyhow::Result<Connection<StreamOwned<ClientConnection, TcpStream>>> {
        let c = imap::ClientBuilder::new(&*self.account.server.0, self.account.server.1)
            .rustls()
            .context("connect")?;
        let mut c = c
            .login(self.account.username.trim(), self.account.password.trim())
            .map_err(|(e, _)| e)
            .context("login")?;
        let cap = c.capabilities().context("get capabilities")?;
        if !cap.has_str("IDLE") {
            anyhow::bail!(
                "server does not support IDLE (in [{}])",
                cap.iter()
                    .map(|s| format!("{:?}", s))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }

        let folder = self.folder.clone();
        c.select(folder).context("select folder")?;

        Ok(Connection {
            account_folder: self.clone(),
            socket: c,
        })
    }
}

struct Connection<T: Read + Write> {
    account_folder: AccountFolder,
    socket: imap::Session<T>,
}

impl<T: Read + Write + imap::extensions::idle::SetReadTimeout> Connection<T> {
    pub fn handle(mut self, account: usize, mut tx: mpsc::Sender<Option<(usize, usize)>>) {
        loop {
            if let Err(e) = self.check(account, &mut tx) {
                // the connection has failed for some reason
                // try to log out (we probably can't)
                eprintln!(
                    "connection to {} failed: {:?}",
                    self.account_folder.account.name, e
                );
                let _ = self.socket.logout();
                break;
            }
        }

        // try to reconnect
        let mut wait = 1;
        for _ in 0..5 {
            eprintln!(
                "connection to {} lost; trying to reconnect...",
                self.account_folder.account.name
            );
            match self.account_folder.connect() {
                Ok(c) => {
                    println!(
                        "{} connection reestablished",
                        self.account_folder.account.name
                    );
                    return c.handle(account, tx);
                }
                Err(e) => {
                    eprintln!(
                        "failed to connect to {}: {:?}",
                        self.account_folder.account.name, e
                    );
                    thread::sleep(Duration::from_secs(wait));
                }
            }

            wait *= 2;
        }
    }

    fn check(
        &mut self,
        account: usize,
        tx: &mut mpsc::Sender<Option<(usize, usize)>>,
    ) -> anyhow::Result<()> {
        // Keep track of all the e-mails we have already notified about
        let mut last_notified = 0;
        let mut notification = None::<notify_rust::NotificationHandle>;

        loop {
            // check current state of inbox
            let mut uids = self
                .socket
                .uid_search("NEW 1:*")
                .context("uid search NEW 1:*")?;
            let num_new = uids.len();
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
                    .uid_fetch(&uids.join(","), "RFC822.HEADER")
                    .context("fetch UIDs with headers")?
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
                                Some(subject) => Cow::from(subject),
                                None => Cow::from("<no subject>"),
                            };

                            let date = match headers.get_first_value("Date") {
                                Some(date) => match chrono::DateTime::parse_from_rfc2822(&date) {
                                    Ok(date) => date.with_timezone(&chrono::Local),
                                    Err(e) => {
                                        println!("failed to parse message date: {:?}", e);
                                        chrono::Local::now()
                                    }
                                },
                                None => chrono::Local::now(),
                            };

                            subjects.insert(date, subject);
                        }
                        Err(e) => println!("failed to parse headers of message: {:?}", e),
                    }
                }
            }

            if !subjects.is_empty() {
                if let Some(notificationcmd) = &self.account_folder.account.notification_command {
                    match Command::new("sh").arg("-c").arg(notificationcmd).status() {
                        Ok(s) if s.success() => {}
                        Ok(s) => {
                            eprint!(
                                "Notification command for {} did not exit successfully.",
                                self.account_folder.account.name
                            );
                            if let Some(exit_code) = s.code() {
                                eprintln!(" Exit code: {}", exit_code);
                            } else {
                                eprintln!(" Process was terminated by a signal.",);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "Could not execute notification command for {}: {}",
                                self.account_folder.account.name, e
                            );
                        }
                    }
                }

                use notify_rust::{Hint, Notification};
                let title = format!(
                    "@{} has new mail ({})",
                    self.account_folder.account.name, num_new
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
                            .hint(Hint::Category("email.arrived".to_owned()))
                            .id(42) // for some reason, just updating isn't enough for dunst
                            .show()
                            .expect("failed to launch notify-send"),
                    );
                }
            }

            if tx.send(Some((account, num_new))).is_err() {
                // we're exiting!
                break Ok(());
            }

            // IDLE until we see changes
            self.socket
                .idle()
                .wait_while(imap::extensions::idle::stop_on_any)
                .context("IDLE failed")?;
        }
    }
}

#[inline]
fn parse_failed<T>(key: &str, typename: &str) -> Option<T> {
    println!("Failed to parse '{}' as {}", key, typename);
    None
}

fn main() {
    // Load the user's config
    let config = ProjectDirs::from("", "", "buzz")
        .expect("Could not find valid home directory.")
        .config_dir()
        .with_file_name("buzz.toml");

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
                            match t["server"].as_str() {
                                Some(v) => v.to_owned(),
                                None => return parse_failed("server", "string"),
                            },
                            match t["port"].as_integer() {
                                Some(v) => v as u16,
                                None => {
                                    return parse_failed("port", "integer");
                                }
                            },
                        ),
                        username: match t["username"].as_str() {
                            Some(v) => v.to_owned(),
                            None => {
                                return parse_failed("username", "string");
                            }
                        },
                        password,
                        notification_command: t.get("notificationcmd").and_then(
                            |raw_v| match raw_v.as_str() {
                                Some(v) => Some(v.to_string()),
                                None => return parse_failed("notificationcmd", "string"),
                            },
                        ),
                        folders: {
                            // Parse the folders field
                            let mut folders: Vec<String> = t
                                .get("folders")
                                .and_then(|raw_v| {
                                    raw_v
                                        .as_array()
                                        .map(|v| {
                                            v.iter()
                                                .filter_map(|raw_v| {
                                                    raw_v
                                                        .as_str()
                                                        .map(|v| v.to_string())
                                                        .or_else(|| parse_failed("folders", "str"))
                                                })
                                                .collect()
                                        })
                                        .or_else(|| parse_failed("folders", "array"))
                                })
                                .unwrap_or_default();

                            // Parse the old folder field and push it to the list of folders
                            if let Some(folder) = t.get("folder").and_then(|raw_v| {
                                raw_v
                                    .as_str()
                                    .map(|x| x.to_string())
                                    .or_else(|| parse_failed("folder", "string"))
                            }) {
                                folders.push(folder);
                            }

                            if folders.is_empty() {
                                vec![String::from("INBOX")]
                            } else {
                                folders
                            }
                        },
                    })
                }
            })
            .collect(),
        None => {
            println!("Could not parse configuration file buzz.toml: not a table");
            return;
        }
    };

    if accounts.is_empty() {
        println!("No accounts in config; exiting...");
        return;
    }

    let mut account_folders = Vec::new();

    for account in accounts {
        let account_ref = Arc::new(account);
        for folder in &account_ref.folders {
            account_folders.push(AccountFolder {
                account: Arc::clone(&account_ref),
                folder: folder.clone(),
            });
        }
    }

    let (tx, rx) = mpsc::channel();

    #[cfg(feature = "systray")]
    let mut tray_icon = match tray_icon::TrayIcon::new(tray_icon::Icon::Disconnected) {
        Ok(tray_icon) => tray_icon,
        Err(e) => {
            eprintln!("Could not create tray item\n{}", e);
            return;
        }
    };

    // TODO: w.set_tooltip(&"Whatever".to_string());
    // TODO: app.wait_for_message();

    let accounts: Vec<_> = account_folders
        .par_iter()
        .filter_map(|account_folder| {
            let mut wait = 1;
            for _ in 0..5 {
                match account_folder.connect() {
                    Ok(c) => return Some(c),
                    Err(e) => {
                        if let Some(e) = e.downcast_ref::<imap::error::Error>() {
                            if let imap::error::Error::Io(e) = e {
                                println!(
                                    "Failed to connect account {}: {}; retrying in {}s",
                                    account_folder.account.name, e, wait
                                );
                                thread::sleep(Duration::from_secs(wait));
                                wait *= 2;
                                continue;
                            }
                        }
                        println!(
                            "{} host produced bad IMAP tunnel: {:?}",
                            account_folder.account.name, e
                        );
                        break;
                    }
                }
            }

            None
        })
        .collect();

    if accounts.is_empty() {
        println!("No accounts in config worked; exiting...");
        return;
    }

    // We have now connected
    #[cfg(feature = "systray")]
    if let Err(e) = tray_icon.set_icon(tray_icon::Icon::Connected) {
        eprintln!("Unable to set tray icon\n{}", e);
    };

    let mut new: Vec<_> = accounts.iter().map(|_| 0).collect();
    for (i, conn) in accounts.into_iter().enumerate() {
        let tx = tx.clone();
        thread::spawn(move || {
            conn.handle(i, tx);
        });
    }

    for r in rx {
        let (i, num_new) = if let Some(r) = r {
            r
        } else {
            break;
        };
        new[i] = num_new;

        #[cfg(feature = "systray")]
        {
            let icon = if new.iter().sum::<usize>() == 0 {
                tray_icon::Icon::UnreadMail
            } else {
                tray_icon::Icon::NewMail
            };
            if let Err(e) = tray_icon.set_icon(icon) {
                eprintln!("Could not set tray icon\n{}", e);
            }
        }
    }
}
