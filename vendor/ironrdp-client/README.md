# IronRDP client

Reusable RDP client engine library built on top of the IronRDP crates suite.

This crate is **library-only**: it exposes the `Config`, the `RdpClient`
runtime, input/output event types, the WebSocket transport, and the session driver. It is
consumed by `ironrdp-viewer` (the portable GUI client binary) and by any other embedder
(for example, a headless agent).

The library is winit-agnostic. Output events are emitted on a bounded
`tokio::sync::mpsc::Sender<RdpOutputEvent>` channel: the embedder is responsible
for consuming them and dispatching them to whatever event loop or runtime it wishes.

For the end-user RDP client binary, see [`ironrdp-viewer`](../ironrdp-viewer).

This crate is part of the [IronRDP] project.

[IronRDP]: https://github.com/Devolutions/IronRDP

---
Vendored copy of `ironrdp-client` 0.1.0 for Tarn. Local changes are marked with `[tarn]`:
- `ConfigBuilder::with_performance_flags`;
- bulk decompressor is recreated after the Deactivation-Reactivation Sequence;
- `RdpOutputEvent::ImageRegion` (dirty region only) replaces the full-frame `Image` event.
- `RdpInputEvent::Flush` — barrier acknowledged once all earlier input is written to the socket.
- `RdpOutputEvent::FrameEnd` — end of a server frame (Frame Marker End), so the UI can present whole frames only.
- `ConfigBuilder::with_gfx` — passes `support_gfx` to the connector (the EGFX channel itself is registered by Tarn via `with_dvc`).
