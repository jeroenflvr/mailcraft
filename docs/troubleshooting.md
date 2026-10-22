# Troubleshooting

When an account misbehaves (mail doesn't arrive, a send fails, syncing is slow), start with the log.

## The log file

MailCraft writes `mailcraft.log` in its app folder. Each start begins a new one; the previous run's
is renamed `mailcraft.old.log`. A run writes up to 4 MB (32 MB with the detailed log); past that
the log says it's full and stops.

| System | Log file |
|---|---|
| Windows | `%APPDATA%\MailCraft\mailcraft.log` |
| macOS | `~/Library/Application Support/MailCraft/mailcraft.log` |
| Linux, FreeBSD | `$XDG_DATA_HOME/mailcraft/mailcraft.log`, usually `~/.local/share/mailcraft/mailcraft.log` |
| Linux, Flatpak | `~/.var/app/ai.storyteller.mailcraft/data/mailcraft/mailcraft.log` |

Started with `--data DIR` or `MAILCRAFT_DATA=DIR`, the log is `DIR/mailcraft.log`; with `--sample`,
it's in the `sample` folder inside the app folder.

The log names accounts, folders, servers' errors and how long things took. It never has a
password, a token or what a message says, so it's safe to share when reporting a problem.

## Following the log

### macOS

```sh
tail -F ~/Library/Application\ Support/MailCraft/mailcraft.log
```

### Linux and FreeBSD

```sh
tail -F "${XDG_DATA_HOME:-$HOME/.local/share}/mailcraft/mailcraft.log"
# Flatpak
tail -F ~/.var/app/ai.storyteller.mailcraft/data/mailcraft/mailcraft.log
```

`-F` (not `-f`) keeps following when MailCraft restarts and starts a new file.

### Windows (PowerShell)

```powershell
Get-Content "$env:APPDATA\MailCraft\mailcraft.log" -Wait -Tail 50
```

After restarting MailCraft, stop this (Ctrl+C) and run it again to follow the new file.

### Only the interesting lines

```sh
tail -F ~/Library/Application\ Support/MailCraft/mailcraft.log | grep -iE 'smtp|send|outbox|inbox|error|warn|refused'
```

```powershell
Get-Content "$env:APPDATA\MailCraft\mailcraft.log" -Wait -Tail 50 | Select-String -Pattern 'smtp|send|outbox|inbox|error|warn|refused'
```

## The detailed log

By default the log has MailCraft's Info lines (each status change, each folder read that changed
something or took over 5 seconds, each change sent to the server, connections starting and
stopping) and other libraries' warnings. The detailed log adds every folder read and every request
to the server.

- **While running:** File › Options › Troubleshooting › *Detailed log (every request to the
  server)*. It takes effect at once and stays on until turned off.
- **For one run:** set `MAILCRAFT_LOG=debug`, or start with `--log-level debug`. Add `--log` to
  also print the lines in the terminal (debug builds always do).

| System | One run with the detailed log |
|---|---|
| macOS | `MAILCRAFT_LOG=debug /Applications/MailCraft.app/Contents/MacOS/MailCraft --log` |
| Linux | `MAILCRAFT_LOG=debug mailcraft --log` |
| Linux, Flatpak | `flatpak run --env=MAILCRAFT_LOG=debug ai.storyteller.mailcraft` |
| Windows | `$env:MAILCRAFT_LOG='debug'; & "$env:ProgramFiles\MailCraft\mailcraft.exe"` |
| From source | `MAILCRAFT_LOG=debug cargo run -p mailcraft` |

On Windows the release app has no console, so `--log` prints nothing there: follow the file
instead.

## Mail doesn't arrive

Send yourself a test message, press Send/Receive, and follow the log while you do. Work through
the path the message takes:

1. **Did it leave?** If it's still in the Outbox, or the log has an SMTP error or a "refused"
   line, the send failed: check the outgoing server, port and security in the account's settings.
2. **Was the Inbox read?** With the detailed log on, every folder read has a line; a read that
   found something says what was new. If Send/Receive doesn't read the Inbox, or the account's
   status is an error, look at the error line.
3. **Did the server deliver it?** If the send worked and the Inbox was read with nothing new, look
   in the provider's webmail. Some providers keep a message you send to yourself only under Sent,
   and spam filters can move it to Junk.

## Checking an account from the command line

`mailcraft-cli` uses the same app folder and accounts as the app, and prints each step to the
terminal (connecting, each folder updated, errors). Close the app first so they don't both sync.

```sh
mailcraft-cli accounts                 # accounts and their folders
mailcraft-cli test --account ID        # sign in and list the folders
mailcraft-cli sync --account ID        # download new mail
mailcraft-cli list --folder INBOX --limit 10
mailcraft-cli send --to you@example.com --subject test --body test --account ID
```

The CLI asks for the password (or reads `MAILCRAFT_PASSWORD`) and never stores it. From source,
use `cargo run -p mailcraft-cli -- <command>`.

## Reporting a problem

Attach `mailcraft.log` (and `mailcraft.old.log` if the problem was in the previous run), ideally
with the detailed log on, and say what you did and when. Times in the log are UTC.
