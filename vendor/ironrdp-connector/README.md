# IronRDP Connector

Abstract state machine to drive an RDP connection sequence.

This crate is part of the [IronRDP] project.

[IronRDP]: https://github.com/Devolutions/IronRDP

---
Vendored copy of `ironrdp-connector` 0.10.0 for Tarn. Local changes are marked with `[tarn]`:
- `Config::support_gfx` — advertises `SUPPORT_DYN_VC_GFX_PROTOCOL` in Client Core Data (32 bpp only),
  so the server opens the Graphics Pipeline (MS-RDPEGFX) channel.
