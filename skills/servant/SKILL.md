---
name: servant
description: Expose local files and folders over HTTP via the always-on servant daemon when an artifact (HTML report, screenshot, build output, docs folder) needs to be shown to the user.
allowed-tools: Bash, Read, servant_serve, servant_ls, servant_rm
---

# Servant — share local artifacts over HTTP

This repo ships a per-user always-on static file server (`servant`). Use
it whenever the user needs to *look at* something you produced on this
host: HTML reports, screenshots, plots, generated docs, build output.

## When to use

- You generated an HTML/image/PDF/static-folder artifact the user should view.
- The user asked for a URL or a way to open the file in a browser.
- You need to share something that lives on this remote host.

Do NOT use it for: arbitrary data exfiltration, secrets, or files the user
did not ask to share.

## URL shape

    http://<this-host>:4769/<slug>

Files mount at `/<basename>`; folders mount at `/<dirname>/` and preserve
relative links.

## Preferred path: extension tools

- `servant_serve { path, ttl?, name? }` — register and get back the URL.
- `servant_ls {}` — see what's currently shared.
- `servant_rm { target }` — remove by id, `/url-path`, full URL, or source path.

## CLI fallback

If the tools aren't loaded, use the raw CLI with JSON output:

    SERVANT_JSON=1 servant serve <path> [--ttl 30m] [--name slug]
    SERVANT_JSON=1 servant ls
    SERVANT_JSON=1 servant rm <id|/url|path>

## TTL guidance

- Default is **24h sliding** (refreshed on every hit). Good for most cases.
- Use `--ttl 30m` (or similar) for one-off previews.
- Use `--ttl never` only when the user explicitly wants a long-lived share.

## Idempotency

Re-serving the same absolute path is a no-op that **slides the TTL
forward** — safe to call repeatedly.

## Cleanup

When the artifact is no longer needed, call `servant_rm` (or
`servant rm <id>`). Otherwise it'll be reaped automatically when the TTL
expires.

## When servant is unreachable

If a tool/CLI call returns "servant daemon unreachable" (exit 2):

    servant service status     # is it running?
    servant service start      # start the user service

## Example

1. Generate `./report.html`.
2. Call `servant_serve { path: "./report.html", ttl: "2h" }`.
3. Report the returned URL to the user.
4. After they're done, `servant_rm { target: "/report.html" }`.
