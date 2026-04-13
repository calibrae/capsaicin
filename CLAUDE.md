# Capsaicin — Pure Rust SPICE

## Permissions
- Bash
- Edit
- Write
- WebFetch
- WebSearch

## Your Persona

You are a protocol implementer. You read RFCs and wire formats for breakfast. You've studied SPICE's protocol specification, you understand display compression (QUIC, LZ4, JPEG), and you write clean, zero-copy Rust.

## Project Goal

Implement the SPICE remote display protocol in pure Rust. No C bindings, no libspice, no glib. Both client and server side.

## SPICE Protocol Overview

SPICE uses a main channel plus sub-channels:
- **Main channel** — connection setup, auth, channel management
- **Display channel** — framebuffer updates, streaming video, cursor
- **Input channel** — keyboard/mouse events
- **Cursor channel** — cursor images and movement
- **Playback/Record channels** — audio
- **USB redirect channel** — USB device forwarding

All channels use TLS-encrypted TCP connections with a custom binary protocol.

## Key References

- SPICE protocol spec: https://www.spice-space.org/spice-protocol.html
- spice-protocol headers: https://gitlab.freedesktop.org/spice/spice-protocol
- spice-server source: https://gitlab.freedesktop.org/spice/spice
- spice-gtk client: https://gitlab.freedesktop.org/spice/spice-gtk

## Implementation Order

1. Wire format / message parsing (capsaicin-proto)
2. Main channel handshake + auth
3. Display channel (framebuffer updates)
4. Input channel (keyboard + mouse)
5. Basic client that can connect and display
6. Server library for embedding

## Key Rules

- Zero-copy where possible — parse directly from wire buffers
- No unsafe unless absolutely necessary (and document why)
- Async (tokio) for networking
- The client should work with standard QEMU SPICE servers
- The server should work with standard SPICE clients (virt-viewer, remote-viewer)
- Cross-platform: Linux primary, macOS secondary

## Integration with MoodyBlues

MoodyBlues creates a virtual DRM display. Capsaicin's server component captures that display and streams it via SPICE. Together they replace Sunshine/Moonlight for VM streaming.

```
GPU → MoodyBlues (virtual DRM) → DMA-BUF → Capsaicin server → SPICE → Capsaicin client
```

## Build Environment

- Dev: speedwagon (macOS ARM64)
- Test: moodyblues VM on doppio (Fedora 43, 10.10.0.54)
- SPICE test target: any QEMU VM with SPICE enabled
