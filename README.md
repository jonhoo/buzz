# Introduction

Using mutt (or pine), but annoyed that it doesn't give you any
notifications when you've received new emails? buzz is a simple tray
application that detects new emails on IMAP servers using IDLE (push
rather than pull). When it detects unseen messages, it shows a OSD style
notification and changes the tray icon to indicate that you have new
mail.

This project is a Rust fork of
[hasmail](https://github.com/jonhoo/hasmail), which provides basically
the same features, and is written in Go.

## What does it look like:

![no new e-mail](assets/no-email.png?raw=true)
![new e-mail](assets/new-email.png?raw=true)

![new e-mail notification](assets/notification.png?raw=true)

# Configuration

buzz looks for a
[TOML](https://github.com/toml-lang/toml#user-content-example)
configuration file in `~/.config/buzz.toml` on startup. The
configuration file consists of a number of sections, each corresponding
to one account:

```toml
[gmail]
server = "imap.gmail.com"
port = 993
username = "jon@gmail.com"
pwcmd = "gnome-keyring-query get gmail_pw"
notificationcmd = "ssh -t somehost wall 'New gmail message!'" #Optional
```

## Account fields

The value in `[]` can be anything (though avoid `.` as it will be parsed
as a new TOML section), and is shown in the tooltip when new e-mails
arrive for an account. The options for an account are as follows:

 - `server`: The address to connect to. MUST currently be SSL/TLS
   enabled.
 - `port`: The port to connect to.
 - `username`: Username for authentication.
 - `pwcmd`: Command to execute to get password for authentication.
 - `notificationcmd`: Additional command to be executed on new messages for this account.

# TODOs

 - [ ] `click` command
 - [ ] hover tooltip
 - [ ] customizeable folder
