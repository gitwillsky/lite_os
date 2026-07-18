# LiteOS userspace

LiteOS userspace deliberately has one interactive product track: standard Linux TUI programs on
PTY/termios/ANSI. Applications are ordinary musl executables or Alpine APKs; no private application
runtime or UI protocol exists.

## Modules

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox configuration, identities and init policy | rootfs builder |
| `console-session/` | `/bin/console-session`; applications see only PTY/termios/ANSI | display, input, terminal model and renderer |
| `diagnostics/` | `cputest`, `memtest`, `cachetest` multicall executable | bounded product diagnostics |

`console-session` is a deep Module. Its external seam is the standard terminal contract; DRM,
evdev, resize transactions, font rasterization, dirty tracking and shell supervision remain private
implementation details. The UART shell is an independent recovery path owned by BusyBox init.
ABI verification consumers live under `scripts/fixtures/` and never enter the product source tree.
