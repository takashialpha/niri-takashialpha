# niri-takashialpha

A personal fork of [**niri**](https://github.com/niri-wm/niri) — a scrollable-tiling
Wayland compositor.

> **All credit for niri goes to [Ivan Molodetskikh (YaLTeR)](https://github.com/YaLTeR)
> and niri's contributors.** This fork would not exist without their work.

## What this is

This is *takashialpha's personal fork* of niri. It is not a rewrite, a competitor,
or a separate project — it tracks upstream niri and exists for maintenance-flavored
changes: dependency dieting, build/packaging tweaks, and review-driven cleanups.
Feature behavior is intended to stay as close to upstream as possible.

If you want **niri itself** — what it is, how to install it, how to configure it,
the keybindings, the IPC, screencasting, everything — go to upstream. This README
intentionally does **not** duplicate niri's documentation:

- **niri repository:** <https://github.com/YaLTeR/niri>
- **niri documentation (wiki):** <https://github.com/YaLTeR/niri/wiki>

## How this fork differs from upstream niri

| | upstream niri | niri-takashialpha |
|---|---|---|
| Goal | the compositor | a leaner personal build of it |
| Versioning | tagged releases | `0.0.0`; builds identified by commit hash (`--version`) |
| Dependencies | full set | trimmed (a lot) where practical |
| Features | upstream of record | (tries) to follow upstream when possible; no intentional divergence |

The differences are deliberately small. When in doubt, upstream is the source of truth.

## Requirements

- **libinput ≥ 1.30**
- Everything else: see upstream's build/runtime requirements.

## Upstream sync policy

When niri upstream lands a feature or fix that's wanted here, it should be pulled in
and documented promptly rather than left to drift. The intent is to stay current with
upstream, not to fork away from it.

## License

GPL-3.0-or-later, same as upstream niri. See [LICENSE](LICENSE).

---

## TODO

- Set up CI workflows (none exist yet).
- Finish dependency/code reviews, focusing on cleanups and anything else droppable.
