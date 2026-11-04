# Security policy

A clipboard manager handles passwords, tokens, and personal data, so security reports are taken seriously.

## Supported versions

ClipVault is pre-release. Security fixes land on `main` and in the latest release once releases begin.

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub:

1. Go to the [Security tab → Report a vulnerability](https://github.com/Festo-Wampamba/clipvault/security/advisories/new).
2. Describe the issue, the affected platform (Windows / GNOME / KDE / Sway / Hyprland / X11), and steps to
   reproduce.

What to expect:

- Acknowledgement within **3 days**.
- An assessment and fix plan within **14 days**.
- Credit in the release notes, unless you prefer to stay anonymous.

## In scope

- Clipboard content leaking to disk unencrypted when encryption at rest is enabled
- Sensitive-content hints or excluded apps being ignored
- The GNOME extension's D-Bus interface accepting calls it should reject
- Sync traffic or relay storage exposing plaintext
- Code execution through crafted clipboard content (HTML, RTF, images, file lists)
