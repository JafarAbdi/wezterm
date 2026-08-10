# Mux socket ownership

## Problem

`LocalListener` removed an existing Unix socket before binding. A second foreground
`wezterm-mux-server` could therefore unlink a live daemon's socket, even though the
daemon still owned its listener and mux sessions. When the second server exited,
the original server remained alive but was unreachable.

The daemon PID lock does not solve this: foreground servers do not take it, and a
process-level PID file does not represent ownership of each configured socket.

## Solution

Give each socket an adjacent ownership lock: `<socket>.lock`.

A persistent listener:

1. opens the lock file and takes a non-blocking exclusive lock;
2. holds the file for the listener's lifetime;
3. binds without first deleting the socket;
4. removes and rebinds only when an existing socket refuses connections.

If the lock is held, startup fails without touching the socket. File locks are
released by the operating system when the process exits, so crashes do not leave
stale ownership. The lock file itself is never removed; removing it could allow
two processes to lock different inodes.

## Why this design

Ownership is attached to the resource being protected. This covers foreground and
daemon servers, multiple configured domains, and concurrent startup without
coupling socket paths to the daemon PID file. WSL is the exception: its unreliable
file locks leave the existing narrow concurrent-stale-startup race in place.

The connection check preserves compatibility with servers that predate the lock:
a reachable socket is left alone, while a refused connection identifies a stale
socket. Unknown errors fail closed.

## Implementation

`wezterm-mux-server-impl/src/local.rs` stores the lock file in `LocalListener`.
Unix uses `flock(LOCK_EX | LOCK_NB)`; Windows opens the lock file with exclusive
sharing. WSL retains the connection check but skips file locking because WSL1
locks can remain falsely held after reboot. Before stale cleanup, Unix also verifies
that the path is a socket, so a regular file or symlink is never deleted.

PID-scoped GUI socket paths are already unique for the life of the process, so
they skip the lock and do not create persistent per-launch lock files.

Regression tests cover live-socket preservation, stale-socket recovery, lock
ownership, concurrent startup, ephemeral sockets, and preservation of non-socket
files.
