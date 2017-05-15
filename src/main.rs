extern crate xdg;
extern crate toml;
extern crate imap;
extern crate rayon;
extern crate openssl;
extern crate systray;
extern crate mailparse;
extern crate notify_rust;

use openssl::ssl::{SslConnectorBuilder, SslMethod};
use imap::client::Client;
use rayon::prelude::*;

use std::collections::HashSet;
use std::process::Command;
use std::io::prelude::*;
use std::time::Duration;
use std::sync::mpsc;
use std::fs::File;
use std::thread;

struct Account<'a> {
    name: &'a str,
    server: (&'a str, u16),
    username: &'a str,
    password: String,
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
        Some(t) => {
            t.iter()
                .filter_map(|(name, v)| match v.as_table() {
                                None => {
                                    println!("Configuration for account {} is broken: not a table",
                                             name);
                                    None
                                }
                                Some(t) => {
                                    let pwcmd = match t.get("pwcmd").and_then(|p| p.as_str()) {
                                        None => return None,
                                        Some(pwcmd) => pwcmd,
                                    };

                                    let password =
                                        match Command::new("sh").arg("-c").arg(pwcmd).output() {
                                            Ok(output) => {
                                                String::from_utf8_lossy(&output.stdout).into_owned()
                                            }
                                            Err(e) => {
                            println!("Failed to launch password command for {}: {}", name, e);
                            return None;
                        }
                                        };

                                    Some(Account {
                                             name: name,
                                             server: (t["server"].as_str().unwrap(),
                                                      t["port"].as_integer().unwrap() as u16),
                                             username: t["username"].as_str().unwrap(),
                                             password: password,
                                         })
                                }
                            })
                .collect()
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
    if let Err(e) = app.set_icon_from_file(&"/usr/share/icons/Faenza/stock/24/stock_disconnect.png"
                                                .to_string()) {
        println!("Could not set application icon: {}", e);
    }
    if let Err(e) = app.add_menu_item(&"Quit".to_string(), |window| { window.quit(); }) {
        println!("Could not add application Quit menu option: {}", e);
    }

    // TODO: w.set_tooltip(&"Whatever".to_string());
    // TODO: app.wait_for_message();

    let accounts: Vec<_> = accounts
        .par_iter()
        .filter_map(|account| {
            let mut wait = 1;
            for _ in 0..5 {
                let tls = SslConnectorBuilder::new(SslMethod::tls()).unwrap().build();
                let c = Client::secure_connect(account.server, account.server.0, tls)
                    .and_then(|mut c| {
                        try!(c.login(account.username, &account.password));
                        let cap = try!(c.capability());
                        if !cap.iter().any(|c| c == "IDLE") {
                            return Err(imap::error::Error::BadResponse(cap));
                        }
                        try!(c.select("INBOX"));
                        Ok((String::from(account.name), c))
                    });

                match c {
                    Ok(c) => return Some(c),
                    Err(imap::error::Error::Io(e)) => {
                        println!("Failed to connect account {}: {}; retrying in {}s",
                                 account.name,
                                 e,
                                 wait);
                        thread::sleep(Duration::from_secs(wait));
                    }
                    Err(e) => {
                        println!("{} host produced bad IMAP tunnel: {}", account.name, e);
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
    for (i, (account, mut imap_socket)) in accounts.into_iter().enumerate() {
        let tx = tx.clone();
        thread::spawn(move || {
            // Keep track of all the e-mails we have already notified about
            let mut last_notified = 0;

            loop {
                // check current state of inbox
                let mut unseen = imap_socket
                    .run_command_and_read_response("UID SEARCH UNSEEN 1:*")
                    .unwrap();

                // remove last line of response (OK Completed)
                unseen.pop();

                let mut num_unseen = 0;
                let mut uids = Vec::new();
                let unseen = unseen.join(" ");
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
                    let mut finish = |message: &[u8]| -> bool {
                        match mailparse::parse_headers(message) {
                            Ok((headers, _)) => {
                                use mailparse::MailHeaderMap;
                                match headers.get_first_value("Subject") {
                                    Ok(Some(subject)) => {
                                        subjects.push(subject);
                                        return true;
                                    }
                                    Ok(None) => {
                                        subjects.push(String::from("<no subject>"));
                                        return true;
                                    }
                                    Err(e) => {
                                        println!("failed to get message subject: {:?}", e);
                                    }
                                }
                            }
                            Err(e) => println!("failed to parse headers of message: {:?}", e),
                        }
                        false
                    };

                    let lines = imap_socket
                        .uid_fetch(&uids.join(","), "RFC822.HEADER")
                        .unwrap();
                    let mut message = Vec::new();
                    for line in &lines {
                        if line.starts_with("* ") {
                            if !message.is_empty() {
                                finish(&message[..]);
                                message.clear();
                            }
                            continue;
                        }
                        message.extend(line.as_bytes());
                    }
                    finish(&message[..]);
                }

                if !subjects.is_empty() {
                    use notify_rust::{Notification, NotificationHint};
                    let title = format!("@{} has new mail ({} unseen)", account, num_unseen);
                    let notification = format!("> {}", subjects.join("\n> "));
                    Notification::new()
                        .summary(&title)
                        .body(&notification)
                        .icon("notification-message-email")
                        .hint(NotificationHint::Category("email".to_owned()))
                        .timeout(-1)
                        .show()
                        .expect("failed to launch notify-send");
                }

                tx.send((i, num_unseen)).unwrap();

                // IDLE until we see changes
                let mut idle = imap_socket.idle().unwrap();
                if let Err(e) = idle.wait_keepalive() {
                    println!("IDLE failed: {:?}", e);
                    break;
                }
            }
            imap_socket.logout().unwrap();
        });
    }

    for (i, num_unseen) in rx {
        unseen[i] = num_unseen;
        if unseen.iter().sum::<usize>() == 0 {
            app.set_icon_from_file(&"/usr/share/icons/oxygen/base/32x32/status/mail-unread.png"
                                         .to_string())
                .unwrap();
        } else {
            app.set_icon_from_file(&"/usr/share/icons/oxygen/base/32x32/status/mail-unread-new.png"
                                         .to_string())
                .unwrap();
        }
    }
}
