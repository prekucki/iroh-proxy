# Changelog

## 0.2.5

- Fix SSH `--fdpass` relays hanging after SSH closes its socket by preventing socket descriptor inheritance across exec.
- Add global `--no-mdns` to disable local mDNS discovery while retaining other discovery methods; propagate it to relay children and generated systemd services.

## 0.2.4

- Recover unresponsive relay actors without restarting the proxy, preserving the endpoint identity and service mappings.
- Keep relay control responsive during reconnect backoff and bound relay shutdown and internal control waits.
- Allow startup to continue after 20 seconds without relay readiness while reconnection continues in the background.

Established TCP streams are not automatically resumed after a connection failure.
