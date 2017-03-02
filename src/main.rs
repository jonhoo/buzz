extern crate xdg;
extern crate toml;
extern crate imap;
extern crate openssl;
extern crate systray;
extern crate mailparse;

use openssl::ssl::{SslContext, SslMethod};
use imap::client::Client;

use std::collections::HashSet;
use std::process::Command;
use std::io::prelude::*;
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
    let t = match s.parse::<toml::Value>() {
        Ok(t) => t,
        Err(e) => {
            println!("Could not parse configuration file buzz.toml: {}", e);
            return;
        }
    };

    // Figure out what accounts we have to deal with
    let accounts: Vec<_> = match t.as_table() {
        Some(t) => {
            t.iter()
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
                            name: &name,
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
    if let Err(e) = app.set_icon_from_file(&"/usr/share/icons/Faenza/stock/24/stock_disconnect.png".to_string()) {
        println!("Could not set application icon: {}", e);
    }
    if let Err(e) = app.add_menu_item(&"Quit".to_string(), |window| { window.quit(); }) {
        println!("Could not add application Quit menu option: {}", e);
    }

    // TODO: w.set_tooltip(&"Whatever".to_string());
    // TODO: app.wait_for_message();

    let accounts: Vec<_> = accounts.into_iter()
        .filter_map(|a| {
            let name = a.name;
            Client::secure_connect(a.server, SslContext::new(SslMethod::Sslv23).unwrap())
                .map(move |mut c| {
                    c.login(a.username, &a.password).unwrap();
                    assert!(c.capability().unwrap().iter().any(|c| c == "IDLE"));
                    c.select("INBOX").unwrap();
                    (String::from(name), c)
                })
                .map_err(move |e| {
                    println!("Failed to connect account {}: {}", name, e);
                })
                .ok()
        })
        .collect();

    // We have now connected
    app.set_icon_from_file(&"/usr/share/icons/Faenza/stock/24/stock_connect.png".to_string()).ok();

    let (tx, rx) = mpsc::channel();
    let mut unseen: Vec<_> = accounts.iter().map(|_| 0).collect();
    for (i, (account, mut imap_socket)) in accounts.into_iter().enumerate() {
        let tx = tx.clone();
        thread::spawn(move || {
            // Keep track of all the e-mails we have already notified about
            let mut notified = HashSet::new();

            loop {
                // check current state of inbox
                let unseen = imap_socket.run_command_and_read_response("SEARCH UNSEEN").unwrap();

                let mut num_unseen = 0;
                let mut uids = Vec::new();
                for line in &unseen {
                    for uid in line.split_whitespace()
                        .skip(2)
                        .take_while(|&e| e != "" && e != "Completed") {
                        if let Ok(uid) = usize::from_str_radix(uid, 10) {
                            if notified.insert(uid) {
                                uids.push(format!("{}", uid));
                            }
                            num_unseen += 1;
                        }
                    }
                }

                let mut subjects = Vec::new();
                {
                    let mut finish = |message: &[u8]| {
                        if message.is_empty() {
                            return;
                        }

                        if let Ok((headers, _)) = mailparse::parse_headers(message) {
                            use mailparse::MailHeaderMap;
                            if let Ok(Some(subject)) = headers.get_first_value("Subject") {
                                subjects.push(subject);
                            }
                        }
                    };

                    let lines = imap_socket.fetch(&uids.join(","), "RFC822.HEADER").unwrap();
                    let mut message = Vec::new();
                    for line in lines {
                        if line.starts_with("* ") {
                            finish(&message[..]);
                            message.clear();
                            continue;
                        }
                        message.extend(line.into_bytes());
                    }
                    finish(&message[..]);
                }

                if !subjects.is_empty() {
                    let title = format!("@{} has new mail ({} unseen)", account, num_unseen);
                    let notification = format!("> {}", subjects.join("\n> "));
                    Command::new("/usr/bin/notify-send")
                        .arg("-i")
                        .arg("notification-message-email")
                        .arg("-c")
                        .arg("email")
                        .arg(title)
                        .arg(notification)
                        .status()
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
