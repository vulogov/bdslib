# bdsweb — SSL/TLS Setup Guide

This guide takes you from **zero to a running HTTPS bdsweb**.  It
covers the built-in TLS server (configured entirely through
`bds.hjson`), the reverse-proxy alternative, certificate generation
for both development and production, verification, and the gotchas
worth knowing before they bite you.

For the config-key reference see
[BDSCONFIG.md § 8.2](BDSCONFIG.md#82-web--security--tls); for the
bdsweb component reference see [BDSWEB.md § 16](BDSWEB.md#16-tls--https).

---

## Table of contents

1. [Overview](#1-overview)
2. [Built-in TLS vs reverse proxy — which to pick](#2-built-in-tls-vs-reverse-proxy--which-to-pick)
3. [Prerequisites](#3-prerequisites)
4. [Step 1 — get a certificate](#4-step-1--get-a-certificate)
5. [Step 2 — place the files](#5-step-2--place-the-files)
6. [Step 3 — configure `web.tls`](#6-step-3--configure-webtls)
7. [Step 4 — start bdsweb](#7-step-4--start-bdsweb)
8. [Step 5 — verify](#8-step-5--verify)
9. [What TLS changes elsewhere](#9-what-tls-changes-elsewhere)
10. [Alternative — TLS at a reverse proxy](#10-alternative--tls-at-a-reverse-proxy)
11. [Renewing certificates](#11-renewing-certificates)
12. [Troubleshooting](#12-troubleshooting)
13. [Security checklist](#13-security-checklist)

---

## 1. Overview

bdsweb can terminate TLS itself.  When the optional `web.tls` block
in `bds.hjson` is enabled, bdsweb loads a PEM certificate + key at
startup and serves HTTPS directly — no nginx, no Caddy, no stunnel
required.  The TLS stack is `rustls` (TLS 1.2 / 1.3) with the `ring`
crypto provider compiled into the binary.

Three things make the feature safe by default:

- **Opt-in.**  Without a `web.tls` block bdsweb serves plain HTTP,
  exactly as before.
- **Fail-fast.**  `web.tls.enabled = true` with a missing or
  unreadable cert/key is a hard startup error — bdsweb never
  silently downgrades to HTTP.
- **Cookie hardening follows.**  Enabling TLS flips the
  `web.secure_cookies` auto-default to `true`, so the session
  cookie is marked `Secure` without extra config.

---

## 2. Built-in TLS vs reverse proxy — which to pick

| | Built-in `web.tls` | Reverse proxy (nginx/Caddy/…) |
|---|---|---|
| Setup | One hjson block | Separate service to install + maintain |
| Cert renewal | You wire it (cron + restart/reload) | Proxy handles it (Caddy: automatic) |
| Multiple backends / vhosts | No | Yes |
| HTTP→HTTPS redirect, gzip, caching | No | Yes |
| Best for | Single bdsweb, simple deploys, air-gapped sites | Fleets, shared edge, existing proxy infra |

Both are fully supported.  If you already run a proxy, terminating
TLS there is fine — jump to [§ 10](#10-alternative--tls-at-a-reverse-proxy).
Otherwise the built-in server is the shortest path.

---

## 3. Prerequisites

- A built `bdsweb` binary (`make all` / `cargo build`).
- A reachable `bdsnode` instance for bdsweb to talk to.
- `openssl` on `PATH` (only for generating a development cert).
- Write access to a directory for the cert + key (this guide uses
  `/etc/bdsweb/tls/`).

---

## 4. Step 1 — get a certificate

You need two PEM files: a **certificate** (`cert.pem`) and its
**private key** (`key.pem`).

> **Important — X.509 v3 only.**  `rustls` rejects ancient v1
> certificates with `UnsupportedCertVersion`.  A bare
> `openssl req -x509` with no extensions produces a v1 cert.  Always
> add at least a `subjectAltName` extension (the commands below do)
> — that forces a v3 cert, which is also what every browser has
> required for years.

### 4a. Development — self-signed certificate

For local dev / internal testing, generate a self-signed cert.  The
`-addext subjectAltName=...` is what makes it a usable v3 cert:

```bash
sudo mkdir -p /etc/bdsweb/tls

sudo openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout /etc/bdsweb/tls/key.pem \
  -out    /etc/bdsweb/tls/cert.pem \
  -subj   "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,DNS:bdsweb.internal,IP:127.0.0.1"
```

Adjust the SAN list to every name/IP clients will use to reach
bdsweb.  Confirm it came out as version 3:

```bash
openssl x509 -in /etc/bdsweb/tls/cert.pem -noout -text | grep -E 'Version|Subject:|DNS:|IP Address'
#         Version: 3 (0x2)
#         Subject: CN = localhost
#         X509v3 Subject Alternative Name:
#             DNS:localhost, DNS:bdsweb.internal, IP Address:127.0.0.1
```

Browsers will warn on a self-signed cert (no trusted CA chain) —
expected for dev.  For internal fleets, sign with your own internal
CA and distribute the CA cert to clients instead.

### 4b. Production — a CA-issued certificate

Use a real certificate for anything beyond local dev.

**Let's Encrypt (public DNS name):**

```bash
# certbot in "certonly" mode — bdsweb is not a certbot plugin target,
# so use the standalone or webroot challenge, then point web.tls at
# the files certbot writes:
sudo certbot certonly --standalone -d bdsweb.example.com

# certbot writes (symlinks):
#   /etc/letsencrypt/live/bdsweb.example.com/fullchain.pem   → cert
#   /etc/letsencrypt/live/bdsweb.example.com/privkey.pem     → key
```

Point `web.tls.cert` at **`fullchain.pem`** (leaf + intermediates),
not `cert.pem` alone — clients need the full chain.

**Internal CA / corporate PKI:** request a server certificate for
bdsweb's hostname, then concatenate leaf + intermediate(s) into one
PEM for `web.tls.cert` and use the matching private key for
`web.tls.key`.

---

## 5. Step 2 — place the files

Put the cert and key where bdsweb can read them, and lock the key
down — it must be readable by the bdsweb user and **no one else**:

```bash
sudo chown bdsweb:bdsweb /etc/bdsweb/tls/cert.pem /etc/bdsweb/tls/key.pem
sudo chmod 644 /etc/bdsweb/tls/cert.pem      # cert is public
sudo chmod 600 /etc/bdsweb/tls/key.pem       # key is secret
```

Accepted formats:

- **Certificate** — PEM, leaf first then any intermediates
  (a "fullchain" file).
- **Private key** — PEM, PKCS#8 (`-----BEGIN PRIVATE KEY-----`) or
  PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`).  Encrypted keys are
  **not** supported — bdsweb cannot prompt for a passphrase at
  startup; decrypt the key first if necessary
  (`openssl rsa -in enc.pem -out key.pem`).

---

## 6. Step 3 — configure `web.tls`

Add a `web.tls` block to the `bds.hjson` you pass to bdsweb with
`--config`:

```hjson
{
  // … your existing cluster / llm / etc. config …

  web: {
    tls: {
      enabled: true
      cert: "/etc/bdsweb/tls/cert.pem"
      key:  "/etc/bdsweb/tls/key.pem"
    }
  }
}
```

| Key                | Required when enabled | Notes |
|--------------------|-----------------------|-------|
| `web.tls.enabled`  | —                     | `true` turns on HTTPS.  Absent or `false` → plain HTTP. |
| `web.tls.cert`     | **yes**               | Path to the PEM cert (fullchain in production). |
| `web.tls.key`      | **yes**               | Path to the PEM private key. |

If `enabled` is `true` but `cert` or `key` is missing/empty, bdsweb
**refuses to start** with a clear error — it will not fall back to
HTTP.

A minimal **dev** config (open-access, loopback, HTTPS) is just:

```hjson
{ web: { tls: { enabled: true, cert: "/etc/bdsweb/tls/cert.pem", key: "/etc/bdsweb/tls/key.pem" } } }
```

A typical **production** config pairs TLS with authentication:

```hjson
{
  cluster: {
    enabled: true
    shared_secret: "…"          // turns on session auth
  }
  web: {
    tls: {
      enabled: true
      cert: "/etc/letsencrypt/live/bdsweb.example.com/fullchain.pem"
      key:  "/etc/letsencrypt/live/bdsweb.example.com/privkey.pem"
    }
  }
}
```

---

## 7. Step 4 — start bdsweb

```bash
bdsweb \
  --host 0.0.0.0 \
  --port 8443 \
  --node http://127.0.0.1:9000 \
  --config /etc/bdslib/bds.hjson
```

On success the startup log shows the `https://` scheme and the
auto-resolved cookie policy:

```
[INFO] bdsweb session cookie Secure flag: true (auto)
[INFO] bdsweb listening on https://0.0.0.0:8443  →  bdsnode at http://127.0.0.1:9000
```

Notes:

- `--port` is just a port number — there is nothing special about
  `443`/`8443`; bind whatever you like (binding `<1024` needs root
  or `CAP_NET_BIND_SERVICE`).
- `--host 0.0.0.0` is allowed here because authentication is on
  (the open-access bind guard only blocks non-loopback binds when
  there is *no* `cluster.shared_secret`).
- The `--node` URL is bdsweb→bdsnode and is independent of bdsweb's
  own TLS — it commonly stays `http://` on a trusted local network.

### As a systemd service

```ini
# /etc/systemd/system/bdsweb.service
[Unit]
Description=bdsweb
After=network-online.target
Wants=network-online.target

[Service]
User=bdsweb
ExecStart=/usr/local/bin/bdsweb --host 0.0.0.0 --port 8443 \
          --node http://127.0.0.1:9000 --config /etc/bdslib/bds.hjson
Restart=on-failure
# Allow binding a privileged port without full root:
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

---

## 8. Step 5 — verify

**TLS handshake + response code:**

```bash
# -k accepts a self-signed/dev cert; drop it for a real CA cert.
curl -sk -o /dev/null -w "HTTP %{http_code}\n" https://127.0.0.1:8443/login
# → HTTP 200
```

**Inspect the negotiated protocol and cert:**

```bash
curl -skv https://127.0.0.1:8443/login 2>&1 | grep -E 'SSL connection|subject:|issuer:'
# * SSL connection using TLSv1.3 / …
# *  subject: CN=localhost
```

**Confirm plain HTTP is refused on the TLS port** (the port speaks
TLS, so an `http://` request gets no valid HTTP response):

```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 3 http://127.0.0.1:8443/login || echo "rejected — as expected"
```

**`/healthz`** works over HTTPS without a session, same as HTTP:

```bash
curl -sk https://127.0.0.1:8443/healthz
# {"status":"ok", …}
```

**Browser:** open `https://<host>:<port>/`.  A real CA cert shows
the padlock; a dev self-signed cert shows a warning you must accept
(expected).

---

## 9. What TLS changes elsewhere

- **Session cookie.**  With TLS on, `web.secure_cookies` defaults to
  `true`, so `bds_session` is issued with the `Secure` attribute
  and never travels in cleartext.  You can still pin it explicitly
  (`web.secure_cookies: false`) for unusual setups, but there is
  rarely a reason to.
- **Auth model is unchanged.**  TLS encrypts the transport; it does
  not authenticate anyone.  Open-access mode is still open-access
  over HTTPS — and bdsweb still refuses to start in open-access
  mode on a non-loopback `--host`.  Run with `cluster.shared_secret`
  for a real deployment.
- **The `--node` leg is separate.**  bdsweb→bdsnode uses whatever
  the `--node` URL says.  TLS on bdsweb's *listener* does not change
  it.

---

## 10. Alternative — TLS at a reverse proxy

If you already run a reverse proxy, terminate TLS there and leave
bdsweb on plain HTTP (bound to loopback so nothing else can reach
it directly).

**nginx:**

```nginx
server {
    listen 443 ssl;
    server_name bdsweb.example.com;

    ssl_certificate     /etc/letsencrypt/live/bdsweb.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/bdsweb.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

Then run bdsweb HTTP-only on loopback:

```bash
bdsweb --host 127.0.0.1 --port 8080 --node http://127.0.0.1:9000 --config /etc/bdslib/bds.hjson
```

Two config keys matter in this topology — set both in the `web`
block:

```hjson
web: {
  // The proxy terminates TLS, so bdsweb's own bind is loopback HTTP
  // and the secure-cookie auto-heuristic would pick `false`.
  // Force it on — the cookie still travels over HTTPS client↔proxy.
  secure_cookies: true

  // bdsweb now sees the proxy's IP as the peer.  Trust the
  // forwarded client IP for the /login rate limiter.  ONLY set this
  // when a trusted proxy actually sets X-Forwarded-For — otherwise
  // a direct client can spoof it.
  trusted_proxy: true
}
```

Make sure the proxy preserves the original `Host` header
(`proxy_set_header Host $host;` above) — bdsweb's same-origin CSRF
check compares `Origin`/`Referer` against `Host`, and a rewritten
`Host` would break state-changing POSTs.

---

## 11. Renewing certificates

bdsweb reads the cert + key **once at startup** — it does not watch
the files or reload them.  After a renewal you must restart bdsweb.

**Let's Encrypt / certbot** — add a deploy hook that restarts the
service:

```bash
# /etc/letsencrypt/renewal-hooks/deploy/restart-bdsweb.sh
#!/bin/sh
systemctl restart bdsweb
```

```bash
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/restart-bdsweb.sh
```

certbot's renewal timer then renews and restarts bdsweb
automatically.  For an internal CA, wire the same restart into
whatever issues your certs.

A restart is a brief blip: in-flight requests drop, the listener
re-binds, and the background pollers re-prime their caches within a
poll interval.  Schedule renewals for a low-traffic window if that
matters.

---

## 12. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `REFUSING TO START: web.tls.enabled = true but web.tls.cert / web.tls.key are not both set` | `enabled: true` with a missing/empty `cert` or `key` | Provide both paths, or remove the `web.tls` block to serve HTTP. |
| `cannot load TLS cert/key (…): invalid peer certificate: … UnsupportedCertVersion` | The certificate is X.509 **v1** (a bare `openssl req -x509` with no extensions) | Regenerate with a `subjectAltName` extension (see [§ 4a](#4a-development--self-signed-certificate)) — that forces v3. |
| `cannot load TLS cert/key (…)`, key error | Wrong file, encrypted key, or key doesn't match the cert | Confirm the key is unencrypted PEM and pairs with the cert: `openssl x509 -noout -modulus -in cert.pem \| md5` must equal `openssl rsa -noout -modulus -in key.pem \| md5`. |
| `cannot bind <addr>: Permission denied` | Binding a port `<1024` without privileges | Use a high port, run as root, or grant `CAP_NET_BIND_SERVICE` (see the systemd unit in [§ 7](#7-step-4--start-bdsweb)). |
| `cannot bind <addr>: Address already in use` | Another process holds the port | Pick another `--port`, or stop the conflicting process. |
| Browser: `ERR_SSL_PROTOCOL_ERROR` / "not secure" | Hitting `https://` on a plain-HTTP bdsweb, or `http://` on a TLS bdsweb | Match the scheme to the startup log line (`listening on http(s)://…`). |
| Browser warns "certificate not trusted" | Self-signed or internal-CA cert the browser doesn't trust | Expected for a dev cert — accept the warning, or import your CA. For production use a CA-issued cert. |
| Clients see a cert error after it worked before | Certificate expired, or only the leaf (not the chain) was served | Renew; point `web.tls.cert` at the **fullchain** PEM, not the leaf alone. |
| Renewed the cert but clients still see the old one | bdsweb only reads the cert at startup | Restart bdsweb (wire a renewal deploy hook — [§ 11](#11-renewing-certificates)). |
| State-changing POSTs fail with 403 behind a proxy | The proxy rewrote the `Host` header, breaking the same-origin CSRF check | Set `proxy_set_header Host $host;` (or equivalent) so bdsweb sees the original host. |

Run bdsweb with `--verbose 2` for debug-level logging while
diagnosing TLS startup problems.

---

## 13. Security checklist

Before exposing bdsweb to a real network:

- [ ] `web.tls.enabled = true` with a **CA-issued** (not self-signed)
      certificate, pointed at the **fullchain** PEM.
- [ ] Private key is `chmod 600`, owned by the bdsweb user.
- [ ] `cluster.shared_secret` is set — TLS encrypts, it does not
      authenticate.  (bdsweb refuses a non-loopback bind without it.)
- [ ] `web.secure_cookies` is `true` (automatic with `web.tls`, or
      set explicitly when a proxy terminates TLS).
- [ ] `web.trusted_proxy` is `true` **only** if a trusted reverse
      proxy actually sets `X-Forwarded-For`.
- [ ] Certificate renewal restarts bdsweb (deploy hook / timer).
- [ ] The bdsweb→bdsnode `--node` leg is on a trusted network, or
      itself secured.

---

## See also

- [BDSWEB.md](BDSWEB.md) — bdsweb component reference (§ 13 auth,
  § 15 resilience, § 16 TLS).
- [BDSCONFIG.md § 8.2](BDSCONFIG.md#82-web--security--tls) — the
  `web.secure_cookies` / `web.trusted_proxy` / `web.tls` key
  reference.
- [BDS_UI.md](BDS_UI.md) — bdsweb user manual.
