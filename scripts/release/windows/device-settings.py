"""Check model device choices through the installed desktop application's WebView2."""
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
    original = b'[local]\nmodel = "qwen3-asr-1.7b-gguf"\nprovider = "vulkan"\n'
    config.write_bytes(original)
    model = 'sense-voice-int8'
    destination = root / 'desktop'
    version = json.loads((REPO / 'desktop/tauri.conf.json').read_text())['version']
    install(candidates, 'desktop', version, destination, env)
    try:
        subprocess.run([destination / 'voiceless.exe', '--config', config, 'model', 'install', model],
                       env=env, stdout=subprocess.DEVNULL, check=True, timeout=420)
        shortcut = Path(os.environ['APPDATA']) / SHORTCUT
        powershell(f"(Start-Process -FilePath '{shortcut}' -PassThru).Id", env)
        console = Console(env['XDG_DATA_HOME'])
        wait_for(console.ready, 'installed desktop runtime', 120)
        catalog = {m['id']: m for m in console.call('api/models')}
        assert catalog[model]['installed'] and catalog[model]['devices'] == ['cpu']
        assert catalog['qwen3-asr-1.7b-gguf']['devices'] == []
        with sync_playwright() as manager:
            page = attach(manager, console)
            page.goto(console.url + '#engine')
            expect(page.locator('meta[name=voiceless-platform]')).to_have_attribute('content', 'windows')
            devices = page.get_by_role('group', name=either('设备', 'Device'))
            vulkan = devices.get_by_role('button', name='Vulkan', exact=True)
            expect(vulkan).to_be_disabled()
            expect(vulkan).to_have_attribute('aria-pressed', 'true')
            assert config.read_bytes() == original, 'opening the engine page rewrote the saved device'
            picker = page.get_by_role('combobox', name=either('模型', 'Model'))
            picker.select_option(model)
            buttons = devices.get_by_role('button')
            expect(buttons).to_have_count(2)
            default = devices.get_by_role('button', name=either('默认', 'Default'))
            cpu = devices.get_by_role('button', name='CPU', exact=True)
            expect(default).to_have_attribute('aria-pressed', 'true')
            expect(cpu).to_be_enabled()
            choices = buttons.all_text_contents()
            save = page.locator('.savebar').get_by_role('button', name=either('保存', 'Save'))
            def saved():
                return tomllib.loads(config.read_text(encoding='utf-8'))['local']
            save.click()
            wait_for(lambda: saved()['model'] == model and saved().get('provider') is None, 'model default saved', 20)
            page.reload()
            expect(picker).to_have_value(model)
            cpu.click()
            save.click()
            wait_for(lambda: saved().get('provider') == 'cpu', 'explicit CPU saved', 20)
            page.reload()
            expect(cpu).to_have_attribute('aria-pressed', 'true')
            page.screenshot(path=str(evidence / 'devices.png'))
            result = dict(version=version, model=model, choices=choices, prior_vulkan_preserved=True, reopened=saved()['provider'])
            (evidence / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
            print('NATIVE_DEVICES', json.dumps(result), flush=True)
    finally:
        powershell(f"Get-Process -Name voiceless, voiceless-desktop -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{destination}\\*' }} | Stop-Process -Force", env)
        subprocess.run([destination / 'unins000.exe', *INSTALL], env=env, check=True, timeout=150)


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'Windows':
        raise SystemExit('Requires the disposable Windows desktop')
    evidence = Path(os.environ['RUNNER_TEMP']) / 'device-evidence'
    evidence.mkdir()
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as temp:
        verify(Path(temp).resolve(), Path(sys.argv[1]).resolve(), evidence)
