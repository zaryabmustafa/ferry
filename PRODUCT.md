# Product

## Register

product

## Users
Technical people with two file servers at two places they visit (office and home, studio and archive, main and branch site) and a Windows laptop that travels between them. Any SMB server works (Unraid, TrueNAS, Synology, Windows); a VPN is optional. They never want to watch the app; they open the window only to check status, change folders, or fix a problem.

## Product Purpose
Mirror chosen SMB folders from the **source** server to the **destination** server by carrying data on the laptop ("sneakernet"), so bulk data never crosses either internet link. Each side gets a name the user picks, and every message uses it. On the source LAN it fills the laptop up to one shared cap with what the destination is missing (folders in priority order); on the destination LAN it pushes, verifies, and frees the space. If a VPN gives a direct path, file lists and deletions can travel over it (metadata only); without one, Ferry carries a snapshot of the destination instead. Success: the destination is an exact mirror, the user never thinks about it, and the app is nearly invisible in RAM/CPU.

## Brand Personality
Quiet, trustworthy, native. Feels like it shipped with Windows 11 (Settings app, Fluent controls, Mica). Honest about state: where am I, what's carried, what's left.

## Anti-references
SaaS dashboards: big metric cards, gradients, decorative charts. Also avoid dense two-pane sync grids (FreeFileSync/GoodSync).

## Design Principles
- Status before controls: the first glance answers "where am I and is the destination caught up?"
- Native over novel: Fluent vocabulary, system font, Windows accent, light/dark follow the OS.
- Never surprise with data loss: deletions and overwrites are visible and explained.
- Silent when fine, loud only when wrong (toast on errors).
- Light by design: UI is cheap to render and disposable.

## Accessibility & Inclusion
WCAG AA contrast, full keyboard navigation, respects Windows reduced motion and high-contrast themes.
