"""Exercise live microphone selection using the installed Mac daemon and its actual web console."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import uuid

from playwright.sync_api import expect, sync_playwright
from acceptance import Daemon, Evidence, Guest, private_roots, wait_for


def exercise(root, evidence, guest, daemon, config):
    def audio():
        return daemon.api('desktop/audio')[1]

    def expect_audio(description, predicate):
        def match():
            value = audio()
            return value if predicate(value) else None
        value = wait_for(match, description)
        evidence.note(description, value)
        print(description, json.dumps({k:value[k] for k in ('selected','effective','system_default','error')}), flush=True)
        return value

    def save(page):
        page.get_by_role('button', name=re.compile(r'^(Save|保存)$')).click()
        page.get_by_role('dialog').get_by_role('button',name=re.compile(r'^(Apply|应用)$')).click()

    baseline = expect_audio('original default input', lambda a: a['selected'] is None and a['effective'] and a['effective'] == a['system_default'])
    original = baseline['system_default']
    original_uid = original.split(':', 1)[1]
    new_uid = 'dev.voiceless.acceptance.' + str(uuid.uuid4())
    temporary = 'coreaudio:' + new_uid
    server = subprocess.check_output(['pgrep','-x','coreaudiod'],text=True).splitlines()
    runtime_id = daemon.api('status')[1]['runtime_id']
    log = (root/'aggregate.log').open('w')
    aggregate = subprocess.Popen([guest.audio,'temporary-input',original_uid,new_uid],stdin=subprocess.PIPE,stdout=log,stderr=log)
    try:
        def created():
            if aggregate.poll() is not None:
                raise RuntimeError((root/'aggregate.log').read_text())
            return any(d['id'] == temporary for d in audio()['microphones'])
        wait_for(created,'new aggregate to appear in the installed console')
        with sync_playwright() as manager:
            browser = manager.chromium.launch(headless=True)
            try:
                page = browser.new_page(locale='en-US')
                page.goto(daemon.origin + '/#hotkey')
                choice = page.get_by_role('combobox', name=re.compile(r'^(Input device|输入设备)$'))
                expect(choice).to_have_value('')
                guest.default_input(new_uid)
                expect_audio('follows a new system default', lambda a: a['selected'] is None and a['effective'] == a['system_default'] == temporary)
                choice.select_option(temporary)
                save(page)
                expect_audio('explicitly selected the aggregate', lambda a: a['selected'] == a['effective'] == temporary)
                guest.default_input(original_uid)
                expect_audio('explicit input survives a default switch', lambda a: a['system_default'] == original and a['selected'] == a['effective'] == temporary)
                aggregate.stdin.close()
                if aggregate.wait(timeout=20) != 0:
                    raise RuntimeError((root/'aggregate.log').read_text())
                lost = expect_audio('only selected input disconnected', lambda a: a['selected'] == temporary and a['effective'] is None and a['system_default'] == original and temporary not in [d['id'] for d in a['microphones']] and temporary in (a['error'] or ''))
                expect(choice).to_have_value(temporary)
                expect(choice.locator('option:checked')).to_contain_text(re.compile('not connected|未连接'))
                expect(page.get_by_text(lost['error'],exact=True)).to_be_visible(timeout=10000)
                page.screenshot(path=str(evidence.directory/'disconnected.png'))
                choice.select_option('')
                save(page)
                restored = expect_audio('explicitly followed the original default', lambda a: a['selected'] is None and a['effective'] == a['system_default'] == original and a['error'] is None)
                expect(choice).to_have_value('')
                expect(page.get_by_text(lost['error'],exact=True)).to_have_count(0,timeout=10000)
                assert 'source' not in tomllib.loads(config.read_text()).get('audio',{})
                assert daemon.api('status')[1]['runtime_id'] == runtime_id
                assert subprocess.check_output(['pgrep','-x','coreaudiod'],text=True).splitlines() == server
                page.screenshot(path=str(evidence.directory/'recovered.png'))
                return {'default':original,'removed':temporary,'recovered':restored['effective'],'same_runtime':True,'same_coreaudiod':True}
            finally:
                browser.close()
    finally:
        try:
            guest.default_input(original_uid)
        finally:
            if not aggregate.stdin.closed:
                aggregate.stdin.close()
            aggregate.wait(timeout=20)
            log.close()


def main():
    root, directory = map(Path,sys.argv[1:3])
    evidence = Evidence(directory)
    try:
        guest = Guest(Path(__file__).resolve().parent,evidence)
        env = private_roots(root/'user',root/'home')
        config = Path(env['XDG_CONFIG_HOME'])/'voiceless/config.toml'
        config.parent.mkdir(parents=True)
        config.write_text('[audio]\ncues=false\n[console]\nport=0\n')
        config.chmod(0o600)
        cli = root/'home/.local/bin/voiceless'
        version = subprocess.check_output([cli,'--version'],env=env,text=True).strip()
        assert version == 'voiceless 0.2.0', version
        daemon = Daemon(cli,config,env,root/'daemon.log',evidence)
        try:
            evidence.check('installed microphone selector handles a single input disconnect and explicit recovery',True,
                           lambda: exercise(root,evidence,guest,daemon,config))
        finally:
            daemon.stop()
    finally:
        evidence.write()
    return int(bool(evidence.failed()))


if __name__ == '__main__':
    sys.exit(main())
