# ClipVault Bridge (GNOME Shell extension)

GNOME on Wayland does not let background apps read the clipboard. This extension runs inside
GNOME Shell, which can, and hands clipboard changes to the ClipVault app. It can also put a clip
back on the clipboard and press Ctrl+V for you.

Status: **Phase 0 prototype**, GNOME 46.

## Security model

Reading the clipboard in the background is exactly what GNOME blocks, so the bridge must not
become a way around that for other programs. Trust comes from the **process tree**, not from a
socket anyone could find:

1. When the extension is enabled, GNOME Shell starts the ClipVault binary itself, with a clean
   environment it builds (nothing like `LD_PRELOAD` is passed on).
2. It opens a one-time socket in a new private folder (`$XDG_RUNTIME_DIR/clipvault-<random>`,
   mode 0700) and tells only that child where it is.
3. It accepts exactly one connection: the one whose kernel-reported PID (`SO_PEERCRED`) is the
   child it just started. Then it deletes the socket and the folder.
4. ClipVault accepts the socket only if the other end is its own parent process, running
   `/usr/bin/gnome-shell`, with no debugger attached. Before connecting it marks itself
   non-dumpable, so other processes cannot attach a debugger or read its memory.

**Requires** Yama `kernel.yama.ptrace_scope` of 1 or higher (Ubuntu's default). With 0, the
extension refuses to start ClipVault.

| Threat | What stops it |
|---|---|
| Any app asks the bridge for the clipboard | There is no session-bus API and no fixed socket. The only socket exists for a moment, in a private folder, and accepts only the PID of the child GNOME Shell just started. |
| A program connects, then `exec`s the real ClipVault to pass the check | Trust is "the child GNOME Shell started", not "whatever runs at this PID", so another program can never hold the matching PID. |
| A program snoops on D-Bus traffic (`dbus-monitor`) | Clipboard data goes peer-to-peer over that socket, never over the session bus. |
| A fake bridge collects what ClipVault writes | ClipVault talks only to its parent, GNOME Shell. Another program can replace the socket, but it cannot become ClipVault's parent. |
| A program swaps the ClipVault binary | The binary and every folder above it must be real (not symlinks), owned by root and not writable by group or others. Development builds need an explicit opt-in, and a notification appears each time one starts. |
| Code injected into ClipVault | GNOME Shell sets the environment, so no `LD_PRELOAD` or `LD_AUDIT` reaches it. The extension starts ClipVault only when Yama is on (scope 1 or higher), so only its parent, GNOME Shell, could attach a debugger, and it makes itself non-dumpable before connecting. |
| A frozen GNOME Shell, from a client that connects and never authenticates | GLib authenticates asynchronously; tested with a silent client, the main loop kept running. |
| Flatpak or Snap apps | They cannot reach the private folder, and they are never the child GNOME Shell started. |
| Password-manager copies | Copies marked `x-kde-passwordManagerHint` (KeePassXC and others) are never read or forwarded. |
| Huge clips | Each format is capped at 20 MB; larger ones are skipped. |
| Idle exposure | Nothing is read from the clipboard unless ClipVault is connected. The extension, and the ClipVault it started, stop on the lock screen. |

**What no extension can stop.** A program already running unsandboxed under your own account
can change your GNOME extensions, settings and autostart entries, or read your files. This holds
for every Linux desktop app, not only ClipVault. The bridge gives such a program nothing new: it
cannot use the bridge without first changing your settings, and the binary it would need to
replace is owned by root.

For Phase 1:

- Restart ClipVault automatically if it exits. Today you re-enable the extension.
- Keep ClipVault running while the screen is locked.
- Encrypt history at rest (planned for v0.2).

## Try the Phase 0 spike

From the repository root, build the spike and install the extension:

```bash
cargo build --example spike
EXT=clipvault@festo-wampamba.github.io
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r extensions/gnome-shell/$EXT ~/.local/share/gnome-shell/extensions/
glib-compile-schemas ~/.local/share/gnome-shell/extensions/$EXT/schemas
```

The spike is a development build in your home folder, so point the extension at it and allow
development builds:

```bash
SCHEMAS=~/.local/share/gnome-shell/extensions/clipvault@festo-wampamba.github.io/schemas
gsettings --schemadir $SCHEMAS set org.gnome.shell.extensions.clipvault \
  client-path "$(realpath target/debug/examples/spike)"
gsettings --schemadir $SCHEMAS set org.gnome.shell.extensions.clipvault allow-user-owned-client true
```

Log out and back in (Wayland cannot reload GNOME Shell), then enable it. GNOME Shell starts the
spike and shows a notification:

```bash
gnome-extensions enable clipvault@festo-wampamba.github.io
```

Watch its output in the journal:

```bash
journalctl --user -f -o cat | grep -E 'ClipVault|bytes|pasted'
```

Then test:

1. Copy some text and take a screenshot to the clipboard. Each copy shows up in the journal.
2. In a terminal, run `pkill -USR1 -x spike`, then switch to Text Editor within 3 seconds.
   The spike pastes your last text there.
3. Run `pkill -USR2 -x spike` and switch to an app that accepts images, such as LibreOffice.
   The spike pastes your last screenshot.

Paste is triggered only by you, never by what is on the clipboard. Otherwise a web page could
copy a trigger word and receive your previous copy.

After rebuilding or changing settings, restart the spike:

```bash
gnome-extensions disable clipvault@festo-wampamba.github.io
gnome-extensions enable clipvault@festo-wampamba.github.io
```

When you are done testing, turn development builds back off:

```bash
gsettings --schemadir $SCHEMAS set org.gnome.shell.extensions.clipvault allow-user-owned-client false
```

## Protocol

D-Bus interface `org.clipvault.Bridge1` at `/org/clipvault/Bridge`, on the one-time socket
passed to ClipVault in `CLIPVAULT_BRIDGE_SOCKET`.

| Member | Kind | Arguments |
|---|---|---|
| `ClipboardChanged` | signal | `a(say)`: list of (MIME type, bytes) |
| `SetClipboard` | method | `s` MIME (`text/plain;charset=utf-8` or `image/png`), `ay` bytes |
| `Paste` | method | none; presses Ctrl+V in the focused window |

Phase 0 limits: `SetClipboard` places one format, and only PNG images are forwarded.
