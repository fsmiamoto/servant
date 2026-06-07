# Servant

Always-Running Static File Server

## Problem

Remote agents and humans on dev hosts frequently need to expose local static
artifacts — HTML files, images, generated reports, whole folders — over HTTP.

Usually each agent has to spint its own ad-hoc HTTP server but that creates a
maze of ports being used.

This is my attempt to alleviate this a bit by having a single static server that
provides a CLI for agents.

## Solution

A per-user, always-running daemon (`servant`) plus a CLI:

```
servant serve file.html
→ http://<host>:4769/file.html
```

The daemon serves registered files and folders **in-place** (no copying), with
live refresh, folder mounts, sliding TTL cleanup, persistent SQLite registry,
and a **CLI-only control plane** over a Unix domain socket. The serving plane
binds `0.0.0.0:4769` and is read-only; registration cannot be performed over
the network.

## Quickstart

```
cargo build --release
target/release/servant install        # writes ~/.config/systemd/user/servant.service
servant serve ./report.html           # prints public URL
servant ls
servant rm <id|url|path>
```
