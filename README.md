<img src="./docs/logo.png/" width="300">

Always-on static file server for remote hosts.

## Why

When you work on a remote dev host, you constantly want to *look* at something
in a browser: a generated HTML report, a screenshot a script just produced, a
build artifact, a folder of plots. 

The usual answer is `python -m http.server` but what usually happens is that
soon I have a maze of ad-hoc servers on random ports. 

I got tired of this so wanted a tiny  daemon that is always on and exposes a small
CLI that agents can use for me.

## Install

Install the binary first, then install the per-user **systemd** service:

```
cargo install --path .
servant service install              # writes ~/.config/systemd/user/servant.service
servant service status               # should say "daemon: ok"
```

On minimal hosts where `systemctl --user` / `user@.service` is unavailable,
install the explicit system-service fallback instead. The unit is root-owned,
but the daemon still runs as your user:

```
sudo servant service install --system
# lifecycle commands use the same explicit mode:
sudo servant service restart --system
sudo servant service uninstall --system
servant service logs --system --system-user "$USER"
```

That's it — the daemon is now running and will keep running across reboots.

## Everyday use

Serve a single file:

```
servant serve ./report.html
```

Serve a whole folder (everything under it becomes browsable):

```
servant serve ./build/docs
```

Give it a friendlier URL slug:

```
servant serve ./report.html --name today
# → http://my-dev-host:4769/today
```

Choose how long it should stick around (sliding TTL — accessing the URL resets
the clock):

```
servant serve ./screenshot.png --ttl 30m
servant serve ./longlived.html  --ttl never
```

See what you're currently sharing:

```
$ servant ls
ID     URL                                  EXPIRES   SOURCE
1      http://my-dev-host:4769/report.html  22h 10m   /home/me/work/report.html
2      http://my-dev-host:4769/today        23h 8m    /home/me/work/screenshot.png
```

Stop sharing something:

```
servant rm 1
servant rm /report.html
servant rm /home/me/work/report.html
```
