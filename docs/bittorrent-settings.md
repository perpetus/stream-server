# BitTorrent Settings

Stream Server exposes libtorrent privacy and network controls through the
existing `/settings` API and persists them in `settings.json`.

These settings are intended for builds that use the `libtorrent` backend:

```bash
cargo build --release --features libtorrent --no-default-features
```

The setting names mirror the JSON keys returned by:

```bash
curl http://127.0.0.1:11470/settings
```

Update only the keys you want to change:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{"btEnableDht":false,"btEncryptionMode":"require"}'
```

The server saves accepted changes back to `settings.json` in the server config
directory. On desktop builds this is usually the OS config directory plus
`stremio-server`, for example `%APPDATA%\stremio-server\settings.json` on
Windows or `~/.config/stremio-server/settings.json` on Linux.

## Privacy And Peer Discovery

| Setting | Type | Default | Description |
| --- | --- | --- | --- |
| `btEnableDht` | boolean | `true` | Enables DHT peer discovery. Disable to avoid announcing through the decentralized DHT network. |
| `btEnablePex` | boolean | `true` | Enables Peer Exchange. Disable to avoid learning and sharing peers through connected peers. |
| `btEnableLsd` | boolean | `true` | Enables Local Service Discovery on the LAN. Disable to avoid local-network peer discovery. |
| `btEncryptionMode` | string or number | `"allow"` | Encryption policy. Accepts `"allow"`/`0`, `"require"`/`1`, or `"disable"`/`2`. |
| `btAnonymousMode` | boolean | `false` | Enables libtorrent anonymous mode, which reduces identifying client metadata where supported. |
| `btAllowMultipleConnectionsPerIp` | boolean | `false` | Allows more than one peer connection per IP address. Keep disabled unless you explicitly need it. |
| `btValidateHttpsTrackers` | boolean | `true` | Validates HTTPS tracker certificates. Disabling this weakens tracker TLS checks. |
| `btSsrfMitigation` | boolean | `true` | Keeps libtorrent SSRF mitigations enabled for tracker and web seed access. |

Privacy-focused example:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btEnableDht": false,
    "btEnablePex": false,
    "btEnableLsd": false,
    "btEncryptionMode": "require",
    "btAnonymousMode": true,
    "btAllowMultipleConnectionsPerIp": false,
    "btValidateHttpsTrackers": true,
    "btSsrfMitigation": true
  }'
```

## Interface And Port Binding

| Setting | Type | Default | Description |
| --- | --- | --- | --- |
| `btListenInterfaces` | string | `"0.0.0.0:42000-42010,[::]:42000-42010"` | Incoming BitTorrent listen interfaces and ports. Use libtorrent's `listen_interfaces` syntax. |
| `btOutgoingInterfaces` | string | `""` | Network interface names or IPs used for outgoing BitTorrent traffic. Empty means system default routing. |
| `btOutgoingPort` | number | `0` | First local outgoing TCP port. `0` lets the OS choose. |
| `btNumOutgoingPorts` | number | `0` | Number of outgoing ports starting at `btOutgoingPort`. `0` means no fixed outgoing range. |

Bind incoming traffic to a specific IP and port range:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btListenInterfaces": "192.168.1.25:42000-42010",
    "btOutgoingInterfaces": "192.168.1.25"
  }'
```

Bind to a VPN interface by name:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btListenInterfaces": "tun0:42000-42010",
    "btOutgoingInterfaces": "tun0"
  }'
```

Use fixed outgoing ports:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btOutgoingPort": 42100,
    "btNumOutgoingPorts": 20
  }'
```

## Tracker And Peer Proxy

| Setting | Type | Default | Description |
| --- | --- | --- | --- |
| `btProxyType` | string or number | `"none"` | Proxy type. Accepts `"none"`/`0`, `"socks4"`/`1`, `"socks5"`/`2`, `"socks5Password"`/`3`, `"http"`/`4`, or `"httpPassword"`/`5`. |
| `btProxyHost` | string | `""` | Proxy host or IP address. |
| `btProxyPort` | number | `0` | Proxy port. |
| `btProxyUsername` | string | `""` | Proxy username for authenticated proxy types. |
| `btProxyPassword` | string | `""` | Proxy password for authenticated proxy types. |
| `btProxyHostnames` | boolean | `true` | Resolves hostnames through the proxy where supported. |
| `btProxyPeerConnections` | boolean | `false` | Routes peer connections through the proxy. |
| `btProxyTrackerConnections` | boolean | `true` | Routes tracker connections through the proxy. |
| `btProxySendHostInConnect` | boolean | `false` | Sends the hostname in HTTP `CONNECT` requests when the libtorrent version supports it. |

Proxy only tracker traffic:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btProxyType": "socks5",
    "btProxyHost": "127.0.0.1",
    "btProxyPort": 1080,
    "btProxyHostnames": true,
    "btProxyTrackerConnections": true,
    "btProxyPeerConnections": false
  }'
```

Proxy tracker and peer traffic through an authenticated SOCKS5 proxy:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btProxyType": "socks5Password",
    "btProxyHost": "127.0.0.1",
    "btProxyPort": 1080,
    "btProxyUsername": "user",
    "btProxyPassword": "password",
    "btProxyHostnames": true,
    "btProxyTrackerConnections": true,
    "btProxyPeerConnections": true
  }'
```

Disable the proxy:

```bash
curl -X POST http://127.0.0.1:11470/settings \
  -H "Content-Type: application/json" \
  -d '{
    "btProxyType": "none",
    "btProxyHost": "",
    "btProxyPort": 0,
    "btProxyUsername": "",
    "btProxyPassword": ""
  }'
```

## Notes

- Existing `settings.json` files are read with defaults for missing keys. The new
  keys may not appear on disk until `/settings` is saved.
- `btEnablePex` can enable PeX dynamically, but disabling PeX for an already
  running libtorrent session may require a restart because libtorrent plugins are
  installed when the session starts.
- `btListenInterfaces`, `btOutgoingInterfaces`, proxy options, and SSRF/TLS
  settings follow libtorrent behavior. See the upstream reference for details:
  <https://www.libtorrent.org/reference-Settings.html>.
