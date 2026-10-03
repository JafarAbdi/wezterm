---
tags:
  - multiplexing
---
# `session_db`

{{since('nightly')}}

Path to the SQLite file where `wezterm-mux-server` saves its local windows,
tabs, panes and scrollback. When the server starts, it rebuilds the newest
saved session before clients can attach, so a crash or reboot brings back the
same layout. The default is `sessions.db` in wezterm's data directory,
`~/.local/share/wezterm/sessions.db` on Linux.

Processes do not survive a crash. Each restored pane starts a fresh shell in
its old working directory, shows its old scrollback, and then handles the
program that was in the foreground:

* A program that published a `WEZTERM_RESUME` [user var](../pane/get_user_vars.md)
  is resumed by running that command. The program, or the shell running it,
  must clear the var when the program exits.
* Any other program has its command line typed at the prompt, without
  pressing Enter.

Only panes in local domains are saved. A tab holding a pane from any other
domain, such as an SSH or multiplexer domain, is skipped.

A program publishes its resume command with an escape sequence, for example:

```bash
printf '\033]1337;SetUserVar=%s=%s\007' WEZTERM_RESUME "$(printf 'pi --session %s' "$id" | base64 -w0)"
```

The server keeps the 20 newest snapshots. Only one server can own a given file.
