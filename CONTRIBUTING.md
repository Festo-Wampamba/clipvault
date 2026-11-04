# Contributing to ClipVault

Thank you for helping build ClipVault! Every contribution counts — code, design, docs, testing on your
desktop, packaging, or just a clear bug report.

## Ways to help

| You know… | Good places to start |
|---|---|
| Rust | Capture backends (`win32`, `data-control`, `x11`, `gnome-bridge`), core storage and search, paste injection |
| TypeScript / React | Popup UI, settings window, keyboard navigation, accessibility |
| GJS / GNOME Shell | The GNOME Shell extension (selection watcher, D-Bus bridge, top-bar indicator) |
| Packaging | winget, Flathub, AUR, `.deb` / `.rpm`, code signing |
| Design | UI sketches, icons, light theme |
| Your own desktop | Testing on KDE, Sway, Hyprland, older Xorg setups, Windows 10 |

Issues labelled **good first issue** are scoped for newcomers; **help wanted** marks bigger pieces where a
hand is welcome. Comment on an issue before starting so work is not duplicated.

## Development setup

See [README §14 — Development setup](README.md#14-development-setup) for the toolchain and system
packages for Ubuntu, Fedora, and Windows.

## Workflow

1. Fork the repository and create a branch from `main`:
   `feat/short-description`, `fix/short-description`, or `docs/short-description`.
2. Make small, focused commits using [Conventional Commits](https://www.conventionalcommits.org):

   ```text
   feat(capture): read image/png from ext-data-control offers
   fix(paste): use Shift+Insert for terminal windows
   docs: add Hyprland hotkey example
   ```

   Types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `ci`, `chore`.
3. Run the checks locally before pushing (once code exists):

   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   pnpm -r lint && pnpm -r test
   ```

4. Open a pull request against `main` and fill in the template. CI must be green before merge.

## Code guidelines

- **Keep it small.** Prefer the standard library and existing dependencies; justify every new crate or
  npm package in the PR description.
- **Platform code stays in its backend.** `clipvault-core` must never import OS-specific crates.
- **Tests verify behaviour, not implementation.** One behaviour per test, descriptive names.
- **Privacy first.** Never log clipboard contents. Never add network calls outside the sync crates.
- **Accessibility.** Every UI action must be reachable by keyboard and labelled for screen readers.
- Rust: `rustfmt` defaults, `clippy` clean. TypeScript: ESLint + Prettier. Use **pnpm**, not npm or yarn.

## Testing on your platform

Please say which platform you tested on in your PR, for example *"Tested on Fedora 41, GNOME 47,
Wayland"* or *"Tested on Windows 10 22H2"*. Clipboard behaviour differs a lot between desktops, so this
information is valuable.

## Reporting bugs and requesting features

Use the [issue templates](https://github.com/Festo-Wampamba/clipvault/issues/new/choose). For security
problems, **do not open a public issue** — follow [SECURITY.md](SECURITY.md).

## License of contributions

ClipVault is licensed under the [GPL-3.0](LICENSE). By submitting a contribution you agree that it is
licensed under the same terms.

## Code of Conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). Be kind and assume good intent.
