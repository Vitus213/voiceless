"""Hold a Windows shortcut in another process while the installed desktop tries to use it."""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib

from playwright.sync_api import expect, sync_playwright
from practice import CDP_PORT, INSTALL, REPO, SHORTCUT, UI, Console, attach, either, install, powershell, user32, wait_for

MODIFIERS = 0x0002 | 0x0004 | 0x4000  # MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT
OLD, NEW = 'Ctrl+Shift+KEY_F6', 'Ctrl+Shift+KEY_F7'
VK_F6, VK_F7 = 0x75, 0x76


def registration_error(vk):
    """Try a reservation on this thread and return the Win32 error, or zero after releasing it."""
    if not user32.RegisterHotKey(None, 100, MODIFIERS, vk):
        return ctypes.get_last_error()
    if not user32.UnregisterHotKey(None, 100):
        raise ctypes.WinError(ctypes.get_last_error())
    return 0


def active_chord(status):
    return next((b['chord'] for b in status['hotkey']['bindings'] if b['field'] == 'toggle_key' and b['keyboards'] > 0), None)


def check(root, candidates, evidence):
    version = json.loads((REPO / 'desktop/tauri.conf.json').read_text())['version']
    destination = root / 'desktop'
    env = dict(os.environ, XDG_CONFIG_HOME=str(root/'config'), XDG_DATA_HOME=str(root/'data'),
               XDG_CACHE_HOME=str(root/'cache'), XDG_STATE_HOME=str(root/'state'), WEBVIEW2_USER_DATA_FOLDER=str(root/'webview'),
               WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=f'--remote-debugging-address=127.0.0.1 --remote-debugging-port={CDP_PORT}')
    config = root/'config/voiceless/config.toml'
    config.parent.mkdir(parents=True)
    config.write_text(f'[hotkey]\ntoggle_key = "{OLD}"\n[audio]\ncues = false\n', encoding='utf-8')
    installer = candidates / f'voiceless-desktop-v{version}-windows-x64.exe'
    with installer.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    assert digest == '40a8454a945f411dc38b0054b4acb08ffc2854e835fe83756202ffc4cd61224c', digest
    assert registration_error(VK_F6) == registration_error(VK_F7) == 0, 'Test chords already in use on the fresh guest'
    install(candidates, 'desktop', version, destination, env)
    try:
        shortcut = Path(os.environ['APPDATA']) / SHORTCUT
        powershell(f"(Start-Process -FilePath '{shortcut}' -PassThru).Id", env)
        console = Console(env['XDG_DATA_HOME'])
        wait_for(console.ready, 'installed desktop runtime', 120)
        wait_for(lambda: active_chord(console.status()) == OLD, 'live F6 binding')
        initial = console.status()
        assert registration_error(VK_F6) == 1409, 'The reported F6 binding is free in Windows'
        assert user32.RegisterHotKey(None, 101, MODIFIERS, VK_F7), ctypes.get_last_error()
        held = True
        try:
            with sync_playwright() as manager:
                page = attach(manager, console)
                page.goto(console.url + '#hotkey')
                page.get_by_role('button', name=UI['record_toggle']).click()
                page.keyboard.press('Control+Shift+F7')
                page.get_by_role('button', name=UI['save']).click()
                page.get_by_role('dialog').get_by_role('button', name=UI['apply_dialog']).click()
                wait_for(lambda: console.status()['config_apply']['state'] == 'failed', 'rejection of the occupied F7', 35)
                failed = console.status()
                error = failed['config_apply']['error']
                assert 'hotkey.toggle_key' in error and '1409' in error, error
                assert failed['config_apply']['active_revision'] == initial['config_apply']['active_revision']
                assert active_chord(failed) == OLD and registration_error(VK_F6) == 1409
                assert tomllib.loads(config.read_text(encoding='utf-8'))['hotkey']['toggle_key'] == NEW
                expect(page.get_by_text(re.compile(r'(?:应用失败|Apply failed).*hotkey\.toggle_key.*1409')).first).to_be_visible(timeout=12000)
                retry = page.get_by_role('button', name=either('重试应用','Retry applying'))
                expect(retry).to_be_visible()
                page.screenshot(path=str(evidence/'occupied.png'))
                print(f'Conflict visible: {error}; running {active_chord(failed)}, saved {NEW}', flush=True)
                if not user32.UnregisterHotKey(None, 101):
                    raise ctypes.WinError(ctypes.get_last_error())
                held = False
                retry.click()
                wait_for(lambda: active_chord(console.status()) == NEW, 'F7 after retry', 35)
                after = console.status()
                assert after['config_apply']['state'] == 'applied' and after['config_apply']['error'] is None, after['config_apply']
                assert after['config_apply']['active_revision'] != initial['config_apply']['active_revision']
                assert after['runtime_id'] == initial['runtime_id']
                assert registration_error(VK_F6) == 0 and registration_error(VK_F7) == 1409
                expect(page.get_by_text(either('设置已生效','Settings in effect')).first).to_be_visible(timeout=12000)
                page.screenshot(path=str(evidence/'retried.png'))
                receipt = {'desktop_sha256':digest, 'old_chord':OLD, 'requested_chord':NEW,
                           'ui_error':error, 'original_revision':initial['config_apply']['active_revision'],
                           'revision_after_refusal':failed['config_apply']['active_revision'],
                           'revision_after_retry':after['config_apply']['active_revision'],
                           'active_after_refusal':active_chord(failed), 'active_after_retry':active_chord(after),
                           'old_free_after_retry':True, 'new_owned_after_retry':True, 'same_runtime':True}
                (evidence/'result.json').write_text(json.dumps(receipt,indent=2),encoding='utf-8')
                print('Retry succeeded in the same installed runtime: F6 released, F7 registered', flush=True)
        finally:
            if held:
                user32.UnregisterHotKey(None, 101)
    finally:
        powershell(f"Get-Process -Name voiceless,voiceless-desktop -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{destination}\\*' }} | Stop-Process -Force", env)
        subprocess.run([destination/'unins000.exe', *INSTALL],env=env,check=True,timeout=120)


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'Windows':
        raise SystemExit('Requires the disposable Windows test desktop')
    evidence = Path(os.environ['RUNNER_TEMP'])/'shortcut-evidence'
    evidence.mkdir()
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as directory:
        check(Path(directory), Path(sys.argv[1]).resolve(), evidence)
