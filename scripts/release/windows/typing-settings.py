"""Verify the paste and copy settings in the installed app's own WebView2 window."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

from playwright.sync_api import expect, sync_playwright
from practice import CDP_PORT, INSTALL, REPO, SHORTCUT, Console, attach, either, install, powershell, wait_for


def verify(root, candidates, evidence):
    env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'),
               XDG_CACHE_HOME=str(root / 'cache'), XDG_STATE_HOME=str(root / 'state'),
               WEBVIEW2_USER_DATA_FOLDER=str(root / 'webview'),
               WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=f'--remote-debugging-address=127.0.0.1 --remote-debugging-port={CDP_PORT}')
    config = root / 'config/voiceless/config.toml'
    config.parent.mkdir(parents=True)
    old = b'[inject]\nprefer = "fcitx5"\n[audio]\ncues = false\n'
    config.write_bytes(old)
    version = json.loads((REPO / 'desktop/tauri.conf.json').read_text())['version']
    destination = root / 'desktop'
    install(candidates, 'desktop', version, destination, env)
    shortcut = Path(os.environ['APPDATA']) / SHORTCUT
    try:
        target = powershell(f"(New-Object -ComObject WScript.Shell).CreateShortcut('{shortcut}').TargetPath", env)
        assert Path(target).resolve() == (destination / 'voiceless-desktop.exe').resolve(), target
        powershell(f"(Start-Process -FilePath '{shortcut}' -PassThru).Id", env)
        console = Console(env['XDG_DATA_HOME'])
        wait_for(console.ready, 'installed desktop runtime', 120)
        with sync_playwright() as manager:
            page = attach(manager, console)
            page.set_default_timeout(20000)
            page.goto(console.url + '#hotkey')
            meta = page.locator('meta[name=voiceless-platform]')
            expect(meta).to_have_attribute('content', 'windows')
            group = page.locator('section.group').filter(has=page.get_by_role('heading', name=either('上屏', 'Type it')))
            paste = group.get_by_role('button', name=either('剪贴板', 'Clipboard'))
            copy = group.get_by_role('button', name=either('复制到剪贴板', 'Copy to clipboard'))
            expect(paste).to_have_attribute('aria-pressed', 'true')
            expect(copy).to_have_attribute('aria-pressed', 'false')
            expect(group.get_by_role('button')).to_have_count(2)
            expect(group.locator('details, input')).to_have_count(0)
            expect(page.get_by_text(either('录音状态显示在 Voiceless 窗口中', 'The Voiceless window shows the recording status.'))).to_be_visible()
            assert config.read_bytes() == old, 'opening settings rewrote the imported preference'
            page.keyboard.press('Control+k')
            search = page.locator('#dialog input[type=search]')
            search.fill('wtype')
            expect(page.get_by_text(either('没有匹配的设置', 'No matching setting'))).to_be_visible()
            dialog = page.get_by_role('dialog')
            dialog.get_by_role('button', name=either('关闭', 'Close')).click()
            expect(dialog).to_have_count(0)
            save = page.locator('.savebar').get_by_role('button', name=either('保存', 'Save'))
            for tile, preference in [(copy, 'copy'), (paste, 'clipboard')]:
                tile.click()
                save.click()
                wait_for(lambda: tomllib.loads(config.read_text(encoding='utf-8'))['inject']['prefer'] == preference,
                         'the chosen delivery method to reach the configuration', 20)
                page.reload()
                expect(tile).to_have_attribute('aria-pressed', 'true')
                assert tomllib.loads(config.read_text(encoding='utf-8'))['audio']['cues'] is False
            page.screenshot(path=str(evidence / 'typing.png'))
            result = {'platform':meta.get_attribute('content'), 'desktop_version':version,
                      'methods':group.get_by_role('button').all_inner_texts(), 'preserved_on_open':True,
                      'saved_and_reopened':['copy', 'clipboard'], 'linux_tool_search_results':0}
            (evidence / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
            print('NATIVE_TYPING', json.dumps(result, ensure_ascii=False), flush=True)
    finally:
        powershell(f"Get-Process -Name voiceless, voiceless-desktop -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{destination}\\*' }} | Stop-Process -Force", env)
        subprocess.run([str(destination / 'unins000.exe'), *INSTALL], env=env, check=True, timeout=150)


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'Windows':
        raise SystemExit('Requires the disposable Windows desktop')
    evidence = Path(os.environ['RUNNER_TEMP']) / 'typing-evidence'
    evidence.mkdir()
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as temp:
        verify(Path(temp).resolve(), Path(sys.argv[1]).resolve(), evidence)
