# Security policy

## Reporting

Please report security issues privately: open a [GitHub security advisory](https://github.com/calibrae/capsaicin/security/advisories/new) on the repo. Do not file a public issue.

## Threat model

Capsaicin sits on three attack surfaces:

1. **Malicious server.** A user runs `capsaicin connect` or `capsaicin view` against a server they don't control. The server can send arbitrary bytes; the client has only the public RSA key and the password.
2. **Malicious client.** Anyone reachable on the listening port of `capsaicin serve` can send arbitrary bytes. They need the password to get past auth, but the link-handshake bytes are processed before auth completes.
3. **Adversarial peer in established session.** Post-auth, the peer can still send arbitrary protocol bytes, malformed messages, malicious image payloads, etc.

Capsaicin currently runs over plain TCP — anything in transit is visible to passive observers and modifiable by active ones. **Until TLS lands, treat the wire as fully untrusted by network observers** and only deploy on trusted networks.

## Hardenings landed

The first round of OWASP / red-team review (April 2026) drove these:

- **Pre-auth allocation cap.** `LinkHeader::size` is capped at `MAX_LINK_PAYLOAD = 4 KiB` before any `Vec` is allocated, on both client and server. Was: pre-auth 4 GiB OOM via a single TCP packet.
- **Image dimension cap.** Every codec entry point (`capsaicin-quic`, `capsaicin-lz`, `capsaicin-glz`) validates `width × height × bpp` with `checked_mul` against `MAX_IMAGE_BYTES = 64 MiB` and `MAX_IMAGE_DIM = 16384` per axis before allocating output buffers. Was: 16 GiB allocations / silent u32-wrap on 32-bit / out-of-bounds writes.
- **GLZ placeholder cap.** When a GLZ image fails to decode and we insert a zero-pixel placeholder so the cross-image dictionary chain stays alive, the placeholder is bounds-checked the same way.
- **`ChannelsList`, `read_chunks` allocation caps.** Hostile peer-supplied counts no longer trigger 4 GiB `Vec::with_capacity` calls.
- **MJPEG decoder memory cap.** `jpeg_decoder::Decoder::set_max_decoding_buffer_size(64 MiB)` plus an explicit 16384 dimension cap before allocating the BGRA expansion buffer.
- **Viewer framebuffer wrap.** The viewer no longer panics if a server sends a `SurfaceCreated` whose `width × height` overflows `u32` — dimensions are validated and the surface request is dropped with a warning.
- **`LinkHeader` major-version validation.** Major-version mismatches now reject the handshake; previously any major version was accepted.
- **Random session IDs.** `Server::new_session_id` returns cryptographically random `u32` values from `OsRng`. Was: sequential `AtomicU32::fetch_add(1)` starting at 1, trivially guessable.
- **Session table.** `Server` maintains a `HashSet<u32>` of live sessions. Sub-channel attaches whose claimed `connection_id` is not in the set are refused. Was: any peer with the password could attach sub-channels to another user's session.
- **Sub-channel attach refuses unknown sessions** (CLI). The reference `serve` subcommand calls `is_live_session` before honouring `accepted.connection_id` for non-Main attaches.
- **Connection concurrency cap.** The CLI's `serve` accepts at most `MAX_CONCURRENT_CONNS = 64` simultaneous connections, gated by a `tokio::sync::Semaphore`. Excess connections are dropped with a warning.
- **`--password` semantics.** `serve` now refuses to start without an explicit `--password VALUE` (use `--password ''` to opt in to no-auth, which logs a warning). `--password` without a value is now an error rather than silently empty.
- **Auth-failure logging.** Failed RSA decrypts / wrong-password attempts log `auth: password rejected` at warn level for log-based intrusion detection.
- **`Server::password()` accessor removed.** No reason for callers to read it back; reduces accidental leakage to logs.

## Known gaps

These are tracked in the issue tracker / [`JOURNEY.md`](JOURNEY.md) and are **deliberately not fixed yet**:

- **No TLS.** Plain-TCP only. The wire is unauthenticated and unencrypted. The auth ticket is RSA-OAEP (so the password itself is protected against passive observers), but everything else — keystrokes, framebuffer, future clipboard — is in the clear. **Do not run on untrusted networks until TLS lands.** (`tokio-rustls` integration is the next planned hardening.)
- **`rsa` crate Marvin Attack advisory** ([RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071)). The crate has a known timing side-channel in PKCS#1 v1.5 decryption. We use OAEP not v1.5, but the crate's CRT path is not fully constant-time. No fixed release exists at time of writing; we'll migrate when 0.10 stabilises.
- **RSA-1024 keypair**, regenerated per process. The 1024-bit key size is mandated by the SPICE protocol's 162-byte SPKI slot; below the modern (NIST SP 800-131A) floor. Mitigated only by being short-lived (per-process) and by TLS pinning when TLS lands.
- **Variable-time password compare.** `password == expected.as_bytes()` is plain `==`. The ~10–30 ms RSA decryption that precedes it dominates any timing leak from the byte compare, but a constant-time compare via `subtle::ConstantTimeEq` is on the to-do list.
- **`PROTOCOL_AUTH_SELECTION` cap not implemented.** The protocol's optional auth-mechanism-selection step is skipped (we use the legacy fall-through). Acceptable today since AUTH_SPICE is the only mechanism we support, but worth implementing alongside SASL if/when that lands.
- **No MAC / integrity check on individual messages.** The protocol has no per-message integrity, so an active in-path attacker can alter wire bytes — only TLS will protect against this.
- **No connection IP throttling.** The concurrency cap is global, not per-IP. A single attacker can saturate the cap.
- **Encoded `SpiceAddress` resolution not implemented.** Some SPICE messages reference image data via packed `surface_id:offset` u64 addresses that need a surface cache to resolve. We currently treat all addresses as simple offsets and bail on encoded ones — losing pixel updates but not vulnerable.

## Out of scope

- The viewer pulls in `winit`, `softbuffer`, `jpeg-decoder`, and through them a graph of GUI / X11 / Wayland / macOS / Win32 dependencies. We rely on the upstream maintainers for vulnerabilities in those crates and on `cargo audit` to surface anything actionable. CI integration of `cargo audit` is on the to-do list.
- Codecs intentionally don't implement the full SPICE draw command set yet; unsupported commands are dropped as `UnhandledDraw`. This is a correctness gap (visible artifacts) rather than a security one.

## Audit log

| Date       | Type                  | Findings                          |
|------------|-----------------------|-----------------------------------|
| 2026-04-13 | OWASP Top 10 2021     | 3 HIGH, 5 MEDIUM, multiple LOW    |
| 2026-04-13 | Adversarial / red-team | 3 CRITICAL, 4 HIGH, 4 MEDIUM     |

All findings from the April 2026 reviews that were classified CRITICAL or HIGH have been addressed in the same commit that created this file. MEDIUM findings are addressed where straightforward; the rest are tracked above under "Known gaps."
