#![warn(rust_2018_idioms)]

use anyhow::Context;
use imap::ImapConnection;
use rayon::prelude::*;
use serde::Deserialize;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::prelude::*;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use directories_next::ProjectDirs;

#[cfg(feature = "systray")]
mod tray_icon;

#[derive(Debug, Deserialize)]
struct Config {
    #[cfg(feature = "systray")]
    icons: Option<tray_icon::Icons>,
    #[serde(rename = "account")]
    accounts: Vec<ConfigAccount>,
}

impl Config {
    fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let config_contents = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("read configuration file at: {:?}", path.as_ref()))?;
        toml::from_str(&config_contents).context("Could not parse configuration")
    }
}

/// A representation of an account as is available in the configuration
#[derive(Clone, Debug, Deserialize)]
struct ConfigAccount {
    name: String,
    server: String,
    port: u16,
    username: String,
    password: Option<String>,
    #[serde(alias = "notificationcmd")]
    notification_command: Option<String>,
    folder: Option<String>,
    #[serde(default)]
    folders: Vec<String>,
    pwcmd: Option<String>,
}

/// A representation of an account as is used by Buzz.
///
/// This version of the accounts is sanitized and is duplicated into a
/// separate account for every folder.
#[derive(Clone, Deserialize, Debug)]
struct ConnectionAccount {
    name: String,
    server: String,
    port: u16,
    username: String,
    password: String,
    notification_command: Option<String>,
    folder: String,
}

impl ConfigAccount {
    fn into_connection_accounts(self) -> anyhow::Result<Vec<ConnectionAccount>> {
        let password = match (&self.password, &self.pwcmd) {
            (Some(password), None) => password.clone(),
            (None, Some(pwcmd)) => String::from_utf8(
                Command::new("sh")
                    .arg("-c")
                    .arg(pwcmd)
                    .output()
                    .context("Execute password command")?
                    .stdout,
            )
            .context("pwcmd is not valid UTF-8")?
            .trim()
            .to_string(),
            (Some(_), Some(_)) => anyhow::bail!(
                "Provide only one of password or pwcmd for account: {name}",
                name = self.name
            ),
            (None, None) => {
                anyhow::bail!(
                    "Either password or pwcmd must be provided for account: {name}",
                    name = self.name
                )
            }
        };

        let mut folders = self.folders.clone();

        if let Some(folder) = &self.folder {
            folders.push(folder.to_owned());
        }

        if folders.is_empty() {
            folders.push("INBOX".to_owned());
        }

        Ok(folders
            .into_iter()
            .map(|folder| ConnectionAccount {
                name: self.name.clone(),
                server: self.server.trim().to_string(),
                port: self.port,
                username: self.username.trim().to_string(),
                password: password.clone(),
                notification_command: self.notification_command.clone(),
                folder,
            })
            .collect())
    }
}

impl ConnectionAccount {
    pub fn connect(&self) -> anyhow::Result<Connection<Box<dyn ImapConnection>>> {
        let c = imap::ClientBuilder::new(&*self.server, self.port)
            .mode(imap::ConnectionMode::AutoTls)
            .tls_kind(imap::TlsKind::Rust)
            .connect()
            .context("connect")?;
        let mut c = c
            .login(&self.username, &self.password)
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
            account: self.clone(),
            socket: c,
        })
    }
}

struct Connection<T: Read + Write> {
    account: ConnectionAccount,
    socket: imap::Session<T>,
}

impl<T: Read + Write + imap::extensions::idle::SetReadTimeout> Connection<T> {
    pub fn handle(mut self, account: usize, mut tx: mpsc::Sender<Option<(usize, usize)>>) {
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
                &self.account.name
            );
            match self.account.connect() {
                Ok(c) => {
                    eprintln!("{} connection reestablished", self.account.name);
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
                if let Some(notificationcmd) = &self.account.notification_command {
                    match Command::new("sh").arg("-c").arg(notificationcmd).status() {
                        Ok(s) if s.success() => {}
                        Ok(s) => {
                            eprint!(
                                "Notification command for {} did not exit successfully.",
                                self.account.name
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
                                self.account.name, e
                            );
                        }
                    }
                }

                use notify_rust::{Hint, Notification};
                let title = format!("@{} has new mail ({})", self.account.name, num_new);

                // we want the n newest e-mail in reverse chronological order
                let mut body = String::new();
                for subject in subjects.values().rev() {
                    body.push_str("> ");
                    body.push_str(subject);
                    body.push('\n');
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

fn main() -> anyhow::Result<()> {
    // Load the user's config
    let config = Config::from_file(
        ProjectDirs::from("", "", "buzz")
            .expect("Could not find valid home directory.")
            .config_dir()
            .with_file_name("buzz.toml"),
    )?;

    if config.accounts.is_empty() {
        eprintln!("No accounts in config; exiting...");
        return Ok(());
    }

    let (account_folders, problems): (Vec<_>, Vec<_>) = config
        .accounts
        .into_iter()
        .map(ConfigAccount::into_connection_accounts)
        .partition(Result::is_ok);

    if !problems.is_empty() {
        eprintln!("Encountered some problems parsing the accounts:");
    }
    for problem in problems {
        eprintln!("{}", problem.unwrap_err());
    }

    let (tx, rx) = mpsc::channel();

    #[cfg(feature = "systray")]
    let mut tray_icon = tray_icon::TrayIcon::new(config.icons, tray_icon::Icon::Disconnected)
        .context("Could not create tray item")?;

    let account_folders: Vec<_> = account_folders
        .into_iter()
        .flat_map(Result::unwrap)
        .par_bridge()
        .filter_map(|account_folder| {
            let mut wait = 1;
            for _ in 0..5 {
                match account_folder.connect() {
                    Ok(c) => return Some(c),
                    Err(e) => {
                        if let Some(imap::error::Error::Io(e)) =
                            e.downcast_ref::<imap::error::Error>()
                        {
                            println!(
                                "Failed to connect account {}: {}; retrying in {}s",
                                &account_folder.name, e, wait
                            );
                            thread::sleep(Duration::from_secs(wait));
                            wait *= 2;
                            continue;
                        }
                        println!(
                            "{} host produced bad IMAP tunnel: {:?}",
                            account_folder.name, e
                        );
                        break;
                    }
                }
            }

            None
        })
        .collect();

    if account_folders.is_empty() {
        anyhow::bail!("No accounts in config worked; exiting...");
    }

    // We have now connected
    #[cfg(feature = "systray")]
    tray_icon
        .set_icon(tray_icon::Icon::Connected)
        .context("Unable to set tray icon")?;

    let mut new: Vec<_> = account_folders.iter().map(|_| 0).collect();
    for (i, conn) in account_folders.into_iter().enumerate() {
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

    Ok(())
}
