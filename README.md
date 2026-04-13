# Capsaicin 🌶️

Pure Rust SPICE protocol implementation. The heat in SPICE.

## Why

SPICE is the best remote display protocol for VMs — better latency than VNC, supports USB redirection, audio, multi-monitor. But the only implementation is QEMU's C codebase and the libspice client library.

Capsaicin implements SPICE in pure Rust — both client and server components. No C dependencies, no libspice, no glib.

## Goals

- Pure Rust SPICE protocol library (no C bindings)
- SPICE display channel (framebuffer, streaming, cursors)
- SPICE input channel (keyboard, mouse, tablet)
- Optional: USB redirection, audio, clipboard sharing
- Client binary for connecting to SPICE servers
- Server library for embedding in custom hypervisors/tools
- Works with MoodyBlues virtual display driver

## Architecture

```
capsaicin-proto/     # SPICE protocol wire format, message types, serialization
capsaicin-display/   # Display channel — framebuffer, QUIC compression, streaming
capsaicin-input/     # Input channel — keyboard, mouse, tablet
capsaicin-client/    # SPICE client binary
capsaicin-server/    # SPICE server library (embeddable)
```

## Related Projects

- **MoodyBlues** — Virtual display adapter (DRM/KMS kernel module)
- **Spytti** — Spotify Connect daemon (YTT family)
- The whole Passione gang running on Doppio
