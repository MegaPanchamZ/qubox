# coturn Deployment Runbook

This runbook covers deploying and operating the [coturn](https://github.com/coturn/coturn)
TURN/STUN server that brokers NAT traversal for `qubox-host-agent` and
`qubox-client-cli`.

## What This Service Does

coturn is the relay used when a direct host↔client QUIC connection fails
because one or both peers are behind NAT or a stateful firewall. The
signaling server issues short-lived credentials (RFC 7635) to peers, and
both peers use them to authenticate against this coturn instance.

It is **not** an update channel. TUF-update feeds live elsewhere; coturn
only relays user-driven media.

## Pre-Deployment Checklist

- [ ] Domain A record (or AAAA) pointing at the deployment host. The
      coturn realm must match this domain.
- [ ] UDP 3478, TCP 3478 open inbound (STUN/TURN).
- [ ] UDP 5349, TCP 5349 open inbound (TURN over TLS).
- [ ] UDP 49152–65535 open inbound for relay allocations
      (set `min-port` / `max-port` to match).
- [ ] TLS cert + private key (Let's Encrypt works; ECDSA preferred).
- [ ] 64-char random `static-auth-secret`. Generate with:
      ```bash
      openssl rand -hex 32
      ```
      **Never reuse the staging placeholder.** The current
      [`ops/coturn/turnserver.conf`](../../ops/coturn/turnserver.conf) has
      `static-auth-secret=dev_shared_secret_change_me` — that is a
      placeholder and must be replaced before this service is reachable
      from the public internet.
- [ ] If the host is behind NAT, know the external IP. coturn supports
      `external-ip=<priv>:<pub>` for this case.
- [ ] Decide on rate-limiting (`user-quota`, `total-quota`). Staging
      uses `12` per user / `1200` total.

## Deployment (systemd on a dedicated VPS)

1. Copy the repo to the host, or rsync the `ops/coturn/` directory:
   ```bash
   rsync -av ops/coturn/ deploy@turn.example.com:/etc/coturn/
   ```
2. Replace placeholders in `/etc/coturn/turnserver.conf`:
   - `realm` → your domain (e.g. `turn.example.com`).
   - `static-auth-secret` → the generated 64-hex secret.
   - `cert` / `pkey` → paths to the provisioned TLS cert.
   - Uncomment and set `external-ip=…` if applicable.
3. Open firewall:
   ```bash
   sudo ufw allow 3478/udp
   sudo ufw allow 3478/tcp
   sudo ufw allow 5349/udp
   sudo ufw allow 5349/tcp
   ```
4. Start coturn:
   ```bash
   sudo systemctl enable --now coturn
   sudo journalctl -u coturn -f
   ```
5. Smoke-test from a second host:
   ```bash
   turnutils_stunclient -p 3478 turn.example.com
   turnutils_uclient -y -p 5349 turn.example.com \
     -u username -w password
   ```
   The first should print a mapped address; the second should allocate
   a relay address.

## Deployment (Docker)

```bash
docker build -t qubox/coturn:latest \
  -f ops/coturn/Dockerfile .

docker run -d --name bp-coturn --restart unless-stopped \
  -p 3478:3478/udp -p 3478:3478/tcp \
  -p 5349:5349/udp -p 5349:5349/tcp \
  -v /etc/letsencrypt:/certs:ro \
  qubox/coturn:latest
```

Override the realm/secret via mounted config or env (the current image
hardcodes them — adjust before going to prod). See
[`../../ops/coturn/docker-compose.yml`](../../ops/coturn/docker-compose.yml).

## Operational Checks

| Symptom                       | Where to look                                    |
|-------------------------------|--------------------------------------------------|
| Stun binding fails            | UDP 3478 filtered; firewall on cloud provider    |
| `401 Unauthorized` on Allocate | `static-auth-secret` out of sync between coturn and signaling server |
| High allocation churn         | Increase `user-quota`; check for abuse on consumer subnet |
| `no-tlsv1_1` warnings ignored | Confirm `no-tlsv1`, `no-tlsv1_1` are set; never enable TLS 1.0/1.1 |
| Relay bandwidth exceeded      | Cloud provider egress limit; consider switching alloc range or quotas |

## Rotation: `static-auth-secret`

`static-auth-secret` is the shared key between coturn and the signaling
server's credential-issue endpoint. Rotating it invalidates all cached
peer credentials; the next allocation attempt by a peer will request a
fresh credential via the signaling WebSocket and succeed.

```bash
NEW_SECRET=$(openssl rand -hex 32)

# 1. Update coturn
sudo sed -i "s|^static-auth-secret=.*|static-auth-secret=$NEW_SECRET|" \
  /etc/coturn/turnserver.conf
sudo systemctl reload coturn

# 2. Update signaling-server config
sudo sed -i "s|^QUBOX_TURN_SECRET=.*|QUBOX_TURN_SECRET=$NEW_SECRET|" \
  /etc/qubox/signaling.env
sudo systemctl restart qubox-signaling.service

# 3. Smoke-test
turnutils_uclient -y -p 5349 turn.example.com \
  -u "test:$(date +%s)" -w "$(date +%s)"
```

There is no rolling window. Peers reconnect → request new credential →
continue. Brief (<30 s) signaling-service downtime during step 2 is
acceptable.

## Incident: Credential Leak

If `static-auth-secret` is leaked or suspected compromised, rotate per
the above. Since credentials are derived from this secret with an
expiry timestamp, attackers also need the username → the timestamps are
typically 10 min, so impact is bounded even without rotation.

## References

- [RFC 8656 — TURN](https://datatracker.ietf.org/doc/html/rfc8656)
- [RFC 7635 — STUN+REST short-term credentials](https://datatracker.ietf.org/doc/html/rfc7635)
- [coturn admin docs](https://github.com/coturn/coturn/wiki)
- [`../../ops/coturn/turnserver.conf`](../../ops/coturn/turnserver.conf)
