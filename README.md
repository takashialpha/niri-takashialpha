# niri-takashialpha

A personal fork of [**niri**](https://github.com/niri-wm/niri) — a scrollable-tiling
Wayland compositor.

> **All credit for niri goes to [Ivan Molodetskikh (YaLTeR)](https://github.com/YaLTeR)
> and niri's contributors.** This fork would not exist without their work.

## What this is

This is *takashialpha's personal fork* of niri. It is not a rewrite, a competitor,
or a separate project — it follows upstream niri and exists for maintenance-flavored
changes: dependency dieting, build/packaging tweaks, and review-driven cleanups. It
also deliberately **removes** features and protocols this fork doesn't use, in favor
of a smaller, cleaner codebase — so behavior can diverge from upstream on purpose.

If you want **niri itself** — what it is, how to install it, how to configure it,
the keybindings, the IPC, screencasting, everything — go to upstream. This README
intentionally does **not** duplicate niri's documentation:

- **niri repository:** <https://github.com/niri-wm/niri>
- **niri documentation (wiki):** <https://github.com/niri-wm/niri/wiki>

## How this fork differs from upstream niri

| | upstream niri | niri-takashialpha |
|---|---|---|
| Goal | the compositor | a leaner personal build of it |
| Versioning | tagged releases | `0.0.0`; builds identified by commit hash (`--version`) |
| Dependencies | full set | trimmed (a lot) where practical |
| Features | upstream of record | a deliberately reduced subset; unused features/protocols removed |
| Upstream changes | the source | ported **by hand**, commit by commit (no clean tracking) |

## Requirements

- **libinput ≥ 1.30**
- Everything else: see upstream's build/runtime requirements.

## Upstream sync policy

There is **no automatic tracking of upstream.** Because this fork removes code and
diverges on purpose, it does not merge upstream wholesale. Everything wanted from
upstream — features and fixes — is reviewed and **ported by hand**, commit by commit.
Merge conflicts are expected and accepted as the cost of a smaller, hand-curated tree.

## License

GPL-3.0-or-later, same as upstream niri. See [LICENSE](LICENSE).

---

## TODO

- None yet :)
