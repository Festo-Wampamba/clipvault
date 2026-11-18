// ClipVault Bridge: lets the ClipVault app see and set the clipboard on GNOME Wayland.
//
// Security model (see ../README.md): trust comes from the process tree. The extension
// starts the ClipVault binary itself with a clean environment, opens a one-time socket in
// a private folder, accepts only the connection whose kernel-reported PID is that child,
// then deletes the socket. Nothing else can connect, and clipboard data never touches the
// session bus. Copies marked secret by password managers never leave the shell.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const SOCKET_ENV = 'CLIPVAULT_BRIDGE_SOCKET';
const SOCKET_NAME = 'bridge.sock';
const PASTE_SHORTCUT = 'paste-previous-shortcut';
// Everything else in GNOME Shell's environment (LD_PRELOAD and friends) is withheld.
// XDG_DATA_HOME must match the user's shell, or `clipvault list` would read another history.
const PASSED_ENV = ['HOME', 'LANG', 'PATH', 'XDG_RUNTIME_DIR', 'XDG_DATA_HOME',
    'XDG_CURRENT_DESKTOP', 'WAYLAND_DISPLAY', 'DBUS_SESSION_BUS_ADDRESS'];
const OBJECT_PATH = '/org/clipvault/Bridge';
const MAX_FORMAT_BYTES = 20 * 1024 * 1024;
const TEXT_MIME = 'text/plain;charset=utf-8';
// Tried in order; the first one offered is forwarded as TEXT_MIME.
const TEXT_SOURCES = [TEXT_MIME, 'UTF8_STRING', 'text/plain', 'STRING', 'TEXT'];
const OTHER_FORWARDED = [
    'text/html',
    'text/rtf',
    'application/rtf',
    'text/uri-list',
    'x-special/gnome-copied-files',
];
// Tried in order; PNG is preferred and others are converted to PNG by ClipVault.
const IMAGE_SOURCES = [
    'image/png',
    'image/jpeg',
    'image/bmp',
    'image/gif',
    'image/webp',
    'image/tiff',
];
const WRITABLE = [TEXT_MIME, 'image/png'];
// KDE convention, also set by KeePassXC and other password managers.
const SENSITIVE_HINT = 'x-kde-passwordManagerHint';
// Modifiers that must be up before the synthetic Ctrl+V; Super can report as either mask.
const HELD_MODIFIERS = Clutter.ModifierType.SHIFT_MASK | Clutter.ModifierType.CONTROL_MASK |
    Clutter.ModifierType.MOD1_MASK | Clutter.ModifierType.MOD4_MASK | Clutter.ModifierType.SUPER_MASK;
const RELEASE_POLL_MS = 20;
const RELEASE_TIMEOUT_MS = 1000;
const RESTART_INITIAL_MS = 1000;
const RESTART_MAX_MS = 30000;
const MAX_RESTART_FAILURES = 5;
const STABLE_UPTIME_US = 10 * 1000 * 1000;

const BRIDGE_XML = `
<node>
  <interface name="org.clipvault.Bridge1">
    <signal name="ClipboardChanged">
      <arg name="formats" type="a(say)"/>
      <arg name="source_app" type="s"/>
    </signal>
    <signal name="PasteRequested"/>
    <method name="SetClipboard">
      <arg name="mime" type="s" direction="in"/>
      <arg name="data" type="ay" direction="in"/>
    </method>
    <method name="Paste"/>
  </interface>
</node>`;

export default class ClipVaultBridge extends Extension {
    enable() {
        this._settings = this.getSettings();
        this._client = null;
        this._child = null;
        this._childPid = null;
        this._connectedAtUs = 0;
        this._restartFailures = 0;
        this._restartSourceId = 0;
        this._pasteSourceId = 0;
        this._ownedSource = null;
        this._server = null;
        this._socketDir = null;
        this._generation = 0;
        this._cancellable = new Gio.Cancellable();
        this._selection = global.display.get_selection();
        this._ownerChangedId = this._selection.connect('owner-changed', (_selection, type, owner) => {
            if (type === Meta.SelectionType.SELECTION_CLIPBOARD)
                this._forwardClipboard(owner).catch(e => console.error(`ClipVault Bridge: ${e.message}`));
        });
        // The only paste trigger: a real key press seen by GNOME Shell. Wayland apps cannot
        // synthesise one, unlike signals or messages any process of this user could send.
        Main.wm.addKeybinding(PASTE_SHORTCUT, this._settings, Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
            Shell.ActionMode.NORMAL, () => this._client?.exported.emit_signal('PasteRequested', null));
        this._startClient();
    }

    disable() {
        this._cancellable.cancel();
        Main.wm.removeKeybinding(PASTE_SHORTCUT);
        this._cancelRestart();
        this._cancelPaste();
        this._selection.disconnect(this._ownerChangedId);
        this._dropClient();
        this._closeSocket();
        this._child?.force_exit();
        this._child = null;
        this._ownedSource = null;
        this._selection = null;
        this._cancellable = null;
        this._keyboard = null;
        this._settings = null;
    }

    _startClient() {
        try {
            this._launchClient();
        } catch (e) {
            console.error(`ClipVault Bridge: cannot start ClipVault: ${e.message}`);
            this._closeSocket();
        }
    }

    _launchClient() {
        const exe = this._settings.get_string('client-path');
        if (!GLib.path_is_absolute(exe))
            throw new Error(`client-path must be absolute, got "${exe}"`);
        // Without Yama (scope 0), any process of this user could attach a debugger to
        // ClipVault in the moment before it locks itself down.
        if (ptraceScope() < 1)
            throw new Error('kernel.yama.ptrace_scope is 0; set it to 1 or higher to run ClipVault safely');
        const development = !isProtected(exe);
        if (development && !this._settings.get_boolean('allow-user-owned-client'))
            throw new Error(`${exe} can be modified by your account; enable allow-user-owned-client for development builds`);

        this._socketDir = makePrivateDir();
        const socketPath = GLib.build_filenamev([this._socketDir, SOCKET_NAME]);
        this._server = Gio.DBusServer.new_sync(`unix:path=${socketPath}`,
            Gio.DBusServerFlags.AUTHENTICATION_REQUIRE_SAME_USER, Gio.dbus_generate_guid(), null, null);
        this._server.connect('new-connection', (_server, connection) => this._accept(connection));
        this._server.start();

        const launcher = new Gio.SubprocessLauncher({flags: Gio.SubprocessFlags.NONE});
        const environment = PASSED_ENV.filter(key => GLib.getenv(key) !== null)
            .map(key => `${key}=${GLib.getenv(key)}`);
        launcher.set_environ([...environment, `${SOCKET_ENV}=${socketPath}`]);
        const child = launcher.spawnv([exe, 'run']);
        this._child = child;
        this._childPid = Number(child.get_identifier());
        this._connectedAtUs = 0;
        child.wait_async(null, () => this._onChildExited(child));
        if (development && this._restartFailures === 0)
            Main.notify('ClipVault Bridge', `Started a development build with clipboard access: ${exe}`);
    }

    _onChildExited(child) {
        if (this._child !== child)
            return;
        const ranStably = this._connectedAtUs > 0 &&
            (GLib.get_monotonic_time() - this._connectedAtUs) >= STABLE_UPTIME_US;
        this._child = null;
        this._dropClient();
        this._closeSocket();

        if (ranStably)
            this._restartFailures = 0;
        this._restartFailures += 1;
        if (this._restartFailures > MAX_RESTART_FAILURES) {
            console.error(`ClipVault Bridge: ClipVault exited ${MAX_RESTART_FAILURES} times in a row; giving up until the extension is re-enabled`);
            return;
        }
        const delayMs = Math.min(RESTART_INITIAL_MS * (2 ** (this._restartFailures - 1)), RESTART_MAX_MS);
        console.warn(`ClipVault Bridge: ClipVault exited; restarting in ${delayMs} ms`);
        this._restartSourceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, delayMs, () => {
            this._restartSourceId = 0;
            this._startClient();
            return GLib.SOURCE_REMOVE;
        });
    }

    _cancelRestart() {
        if (this._restartSourceId)
            GLib.source_remove(this._restartSourceId);
        this._restartSourceId = 0;
    }

    _accept(connection) {
        const pid = peerPid(connection);
        if (pid === null || pid !== this._childPid || this._client !== null) {
            console.warn(`ClipVault Bridge: refused a connection from pid ${pid}`);
            connection.close(null, null);
            return true;
        }
        const exported = Gio.DBusExportedObject.wrapJSObject(BRIDGE_XML, {
            SetClipboard: (mime, data) => this._setClipboard(mime, data),
            Paste: () => this._paste(),
        });
        exported.export(connection, OBJECT_PATH);
        const closedId = connection.connect('closed', () => this._dropClient());
        this._client = {connection, exported, closedId};
        this._connectedAtUs = GLib.get_monotonic_time();
        // ClipVault is connected: remove the socket so nothing else can even try.
        this._closeSocket();
        return true;
    }

    _dropClient() {
        if (this._client === null)
            return;
        const {connection, exported, closedId} = this._client;
        this._client = null;
        connection.disconnect(closedId);
        exported.unexport();
        if (!connection.is_closed())
            connection.close(null, null);
    }

    _closeSocket() {
        this._server?.stop();
        this._server = null;
        if (this._socketDir !== null) {
            removeIfExists(GLib.build_filenamev([this._socketDir, SOCKET_NAME]));
            removeIfExists(this._socketDir);
            this._socketDir = null;
        }
    }

    async _forwardClipboard(owner) {
        // Nothing is read from the clipboard unless the verified ClipVault is connected
        // and the session is unlocked.
        if (this._client === null || Main.sessionMode.currentMode !== 'user')
            return;
        const generation = ++this._generation;
        const sourceApp = owner === this._ownedSource ? '' : focusedApp();
        const type = Meta.SelectionType.SELECTION_CLIPBOARD;
        const offered = this._selection.get_mimetypes(type);
        if (offered.includes(SENSITIVE_HINT))
            return;

        const formats = [];
        const textSource = TEXT_SOURCES.find(m => offered.includes(m));
        if (textSource)
            await this._readInto(formats, type, textSource, TEXT_MIME);
        for (const mime of OTHER_FORWARDED.filter(m => offered.includes(m)))
            await this._readInto(formats, type, mime, mime);
        const imageSource = IMAGE_SOURCES.find(m => offered.includes(m));
        if (imageSource)
            await this._readInto(formats, type, imageSource, imageSource);

        // A newer copy started while we were reading; that one wins.
        if (generation !== this._generation || this._client === null || formats.length === 0)
            return;
        this._client.exported.emit_signal('ClipboardChanged',
            new GLib.Variant('(a(say)s)', [formats, sourceApp]));
    }

    async _readInto(formats, type, sourceMime, asMime) {
        const bytes = await new Promise((resolve, reject) => {
            const stream = Gio.MemoryOutputStream.new_resizable();
            this._selection.transfer_async(type, sourceMime, MAX_FORMAT_BYTES + 1, stream,
                this._cancellable, (selection, result) => {
                    try {
                        selection.transfer_finish(result);
                        stream.close(null);
                        resolve(stream.steal_as_bytes().toArray());
                    } catch (e) {
                        reject(e);
                    }
                });
        });
        if (bytes.length > MAX_FORMAT_BYTES)
            console.warn(`ClipVault Bridge: skipped ${sourceMime}, larger than ${MAX_FORMAT_BYTES} bytes`);
        else
            formats.push([asMime, bytes]);
    }

    _setClipboard(mime, data) {
        if (!WRITABLE.includes(mime))
            throw new Error(`unsupported format ${mime}`);
        if (data.length > MAX_FORMAT_BYTES)
            throw new Error(`clip larger than ${MAX_FORMAT_BYTES} bytes`);
        const source = Meta.SelectionSourceMemory.new(mime, new GLib.Bytes(data));
        this._ownedSource = source;
        this._selection.set_owner(Meta.SelectionType.SELECTION_CLIPBOARD, source);
    }

    // The paste shortcut's own keys are still down when ClipVault asks to paste. Sent then,
    // Ctrl+V would reach the app as Ctrl+Shift+Super+V, which most apps ignore; so wait for
    // them to be let go. If they never are, skip it: the clip is already on the clipboard.
    _paste() {
        this._cancelPaste();
        let waitedMs = 0;
        const attempt = () => {
            const [, , modifiers] = global.get_pointer();
            if ((modifiers & HELD_MODIFIERS) === 0) {
                this._pasteSourceId = 0;
                this._sendCtrlV();
                return GLib.SOURCE_REMOVE;
            }
            waitedMs += RELEASE_POLL_MS;
            if (waitedMs >= RELEASE_TIMEOUT_MS) {
                this._pasteSourceId = 0;
                console.warn('ClipVault Bridge: shortcut keys still held, not pasting; the clip is on the clipboard');
                return GLib.SOURCE_REMOVE;
            }
            return GLib.SOURCE_CONTINUE;
        };
        if (attempt() === GLib.SOURCE_CONTINUE)
            this._pasteSourceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RELEASE_POLL_MS, attempt);
    }

    _cancelPaste() {
        if (this._pasteSourceId)
            GLib.source_remove(this._pasteSourceId);
        this._pasteSourceId = 0;
    }

    _sendCtrlV() {
        this._keyboard ??= Clutter.get_default_backend().get_default_seat()
            .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        const now = GLib.get_monotonic_time();
        this._keyboard.notify_keyval(now, Clutter.KEY_Control_L, Clutter.KeyState.PRESSED);
        this._keyboard.notify_keyval(now, Clutter.KEY_v, Clutter.KeyState.PRESSED);
        this._keyboard.notify_keyval(now, Clutter.KEY_v, Clutter.KeyState.RELEASED);
        this._keyboard.notify_keyval(now, Clutter.KEY_Control_L, Clutter.KeyState.RELEASED);
    }
}

// Application class or desktop ID of the focused window, for excluded-app matching.
function focusedApp() {
    const win = global.display.focus_window;
    if (!win)
        return '';
    const wmClass = win.get_wm_class();
    if (wmClass)
        return wmClass;
    const app = Shell.WindowTracker.get_default().get_window_app(win);
    return app ? app.get_id().replace(/\.desktop$/i, '') : '';
}

// PID of the process that connected, from the kernel (SO_PEERCRED), or null if unknown.
function peerPid(connection) {
    const credentials = connection.get_peer_credentials();
    if (credentials === null)
        return null;
    try {
        return credentials.get_unix_pid();
    } catch (e) {
        console.warn(`ClipVault Bridge: no PID for connection: ${e.message}`);
        return null;
    }
}

// GJS has no mkdtemp: make a new, randomly named folder (fails if it already exists)
// inside the user-only runtime folder, and lock it to 0700.
function makePrivateDir() {
    const dir = Gio.File.new_for_path(GLib.build_filenamev(
        [GLib.get_user_runtime_dir(), `clipvault-${GLib.uuid_string_random()}`]));
    dir.make_directory(null);
    dir.set_attribute_uint32('unix::mode', 0o700, Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
    return dir.get_path();
}

// Yama's ptrace restriction level; 0 when Yama is absent, which means no restriction.
function ptraceScope() {
    let contents;
    try {
        [, contents] = GLib.file_get_contents('/proc/sys/kernel/yama/ptrace_scope');
    } catch (e) {
        console.warn(`ClipVault Bridge: cannot read Yama ptrace_scope: ${e.message}`);
        return 0;
    }
    const scope = Number.parseInt(new TextDecoder().decode(contents), 10);
    return Number.isNaN(scope) ? 0 : scope;
}

// The binary and every folder above it must be real (not symlinks), root-owned and not
// group- or world-writable, so no process of this user can swap the binary.
function isProtected(path) {
    for (let p = path; ; p = GLib.path_get_dirname(p)) {
        const info = Gio.File.new_for_path(p).query_info('standard::type,unix::uid,unix::mode',
            Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
        if (info.get_file_type() === Gio.FileType.SYMBOLIC_LINK ||
            info.get_attribute_uint32('unix::uid') !== 0 ||
            (info.get_attribute_uint32('unix::mode') & 0o022) !== 0)
            return false;
        if (p === '/')
            return true;
    }
}

function removeIfExists(path) {
    try {
        Gio.File.new_for_path(path).delete(null);
    } catch (e) {
        if (!e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.NOT_FOUND))
            throw e;
    }
}
