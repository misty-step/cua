"""Generated native send/cleanup contracts; the outer runner enforces device-free isolation.

Usage: python3 run.py /absolute/cua-driver /absolute/new-scratch-directory
Optional: --consumer-export /absolute/export-directory
No real sensor/compositor or complete installed Hermes acceptance is claimed.
"""
from __future__ import annotations

import argparse
import ast
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import signal
import struct
import subprocess
import sys
import time
import types
from typing import Any


class JsonProcess:
    def __init__(self, argv, env, name, owned):
        self.log = open(f'/work/{name}.stderr', 'wb')
        self.proc = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        stdin, stdout = self.proc.stdin, self.proc.stdout
        assert stdin is not None and stdout is not None
        self.stdin, self.stdout = stdin, stdout
        owned.append(self)
        self.background = Path(argv[0]).name in {'sleep', 'Xvfb', 'dbus-daemon'}
        self.buffer = b''
        self.messages = []

    def send(self, value):
        data = value if isinstance(value, str) else json.dumps(value)
        self.stdin.write((data + '\n').encode())

    def receive(self, predicate=lambda _: True, timeout=5):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if b'\n' not in self.buffer:
                if not select.select([self.stdout], [], [], max(0, end - time.monotonic()))[0]:
                    break
                data = os.read(self.stdout.fileno(), 65536)
                if not data:
                    raise RuntimeError(f'child {self.proc.pid} exited {self.proc.poll()}; {self.log.name}')
                self.buffer += data
                continue
            line, self.buffer = self.buffer.split(b'\n', 1)
            value = json.loads(line)
            self.messages.append(value)
            if predicate(value):
                return value
        raise TimeoutError(f'child {self.proc.pid}: {self.log.name}')

    def request(self, identity, method, params=None):
        value = dict(jsonrpc='2.0', id=identity, method=method)
        if params is not None:
            value['params'] = params
        self.send(value)
        return self.receive(lambda r: r.get('id') == identity)

    def tool(self, identity, name, args):
        return self.request(identity, 'tools/call', dict(name=name, arguments=args))

    def state(self):
        self.send('state')
        return self.receive(lambda r: 'idle_ms' in r)

    def command(self, command, key):
        self.send(command)
        return self.receive(lambda r: key in r)

    def stop(self):
        if self.proc.poll() is None:
            if self.background:
                self.proc.terminate()
            try:
                self.stdin.close()
                self.proc.wait(timeout=3)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=3)
        self.log.close()


def write_private(path, value):
    temporary = path.with_suffix('.next')
    temporary.write_text(json.dumps(value))
    temporary.chmod(0o600)
    temporary.replace(path)


def stable(before, after):
    assert after['sibling_events'] == 0 and after['root_mask'] == 0
    assert (before['root_xy'], before['focus']) == (after['root_xy'], after['focus'])
    assert after['idle_valid'] and after['idle_ms'] >= before['idle_ms']
    assert after['held'] == 0


def exported_backend(driver):
    """Exercise exact exported capture/input/result helpers against real MCP I/O.

    This wire adapter/config facade is a fixture, NOT the installed SDK/session,
    plugin middleware or complete Hermes tool handler.
    """
    import logging
    for package in ['tools', 'tools.computer_use']:
        module = types.ModuleType(package)
        module.__path__ = []
        sys.modules[package] = module

    def load(name, filename):
        spec = importlib.util.spec_from_file_location('tools.computer_use.' + name, '/consumer/' + filename)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        return module

    backend = load('backend', 'backend.py')
    parse = load('cua_backend_parse', 'cua_backend_parse.py')
    input_module = load('cua_backend_input', 'cua_backend_input.py')
    capture = load('cua_backend_capture', 'cua_backend_capture.py')
    facade = types.ModuleType('tools.computer_use.cua_backend')
    setattr(facade, '_cua_configured_ax_max_elements', lambda: 200)
    sys.modules[facade.__name__] = facade
    inventory = driver.request(20, 'tools/list')['result']['tools']
    tools = {tool['name']: tool for tool in inventory}

    class NativeWireAdapter:
        capabilities_discovered = True

        def __init__(self):
            self.identity = 30
            self.calls = []

        def _has_tool(self, name):
            return name in tools

        def supports_input_property(self, name, key):
            return key in tools.get(name, {}).get('inputSchema', {}).get('properties', {})

        def supports_capability(self, capability, *, tool):
            return capability in tools.get(tool, {}).get('capabilities', [])

        def call_tool(self, name, args, timeout=30):
            self.identity += 1
            self.calls.append(dict(name=name, args=dict(args)))
            value = driver.tool(self.identity, name, args)['result']
            result = types.SimpleNamespace(content=[types.SimpleNamespace(**p) for p in value['content']],
                structuredContent=value.get('structuredContent'), isError=value.get('isError', False))
            return parse._extract_tool_result(result)

        def _call_tool_via_cli(self, *args):
            raise AssertionError('metadata arming must not trigger CLI fallback')

    tree = ast.parse(Path('/consumer/cua_backend.py').read_text())
    cls = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == 'CuaDriverBackend')
    names = {'_clear_active_target', '_set_active_target', '_action'}
    functions = [node for node in cls.body if isinstance(node, ast.FunctionDef) and node.name in names]
    assert len(functions) == len(names)
    namespace = dict(ActionResult=backend.ActionResult, _action_result_from=parse._action_result_from,
                     logger=logging.getLogger('generated-consumer-contract'))
    module = ast.Module(body=[ast.ImportFrom(module='__future__', names=[ast.alias(name='annotations')], level=0), *functions], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), '/consumer/cua_backend.py', 'exec'), namespace)
    fixture_class = type('ExportedBackendHelpers', (capture._CaptureMixin, input_module._InputMixin),
                         {name: namespace[name] for name in names})
    instance = fixture_class()
    instance._session = NativeWireAdapter()
    instance._session_id = 'generated-consumer'
    instance._clear_active_target()
    return instance


def inside():
    import resource
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    work = Path('/work')
    for name in ['home', 'runtime']:
        (work / name).mkdir(mode=0o700, exist_ok=True)
    env = dict(PATH='/usr/bin:/bin', HOME='/work/home', XDG_RUNTIME_DIR='/work/runtime',
               XDG_CONFIG_HOME='/work/home/config', XDG_CACHE_HOME='/work/home/cache',
               XDG_DATA_HOME='/work/home/data', TMPDIR='/tmp', DISPLAY=':71',
               XAUTHORITY='/work/auth', DBUS_SESSION_BUS_ADDRESS='unix:path=/work/bus',
               CUA_DRIVER_RS_TELEMETRY_ENABLED='0', CUA_DRIVER_RS_UPDATE_CHECK='false')
    cookie = os.urandom(16)
    record = struct.pack('>H', 65535)
    for data in [b'', b'71', b'MIT-MAGIC-COOKIE-1', cookie]:
        record += struct.pack('>H', len(data)) + data
    (work / 'auth').write_bytes(record)
    (work / 'auth').chmod(0o600)
    owned = []
    rows = []
    try:
        xvfb = JsonProcess(['/usr/bin/Xvfb', ':71', '-screen', '0', '640x480x24',
                            '-nolisten', 'tcp', '-noreset', '-auth', '/work/auth',
                            '-extension', 'GLX'], env, 'xvfb', owned)
        bus = JsonProcess(['/usr/bin/dbus-daemon', '--session', '--nofork',
                           '--address=unix:path=/work/bus'], env, 'bus', owned)
        deadline = time.monotonic() + 5
        while not Path('/tmp/.X11-unix/X71').exists() or not Path('/work/bus').exists():
            assert xvfb.proc.poll() is None and bus.proc.poll() is None
            assert time.monotonic() < deadline
            time.sleep(0.01)

        def begin(name, existing=None):
            if existing is None:
                fixture = JsonProcess(['/work/fixture'], env, 'fixture-' + name, owned)
                target = fixture.receive()
            else:
                fixture, target = existing
            producer = JsonProcess(['/usr/bin/sleep', '300'], env, 'producer-' + name, owned)
            verdict: dict[str, Any] = dict(schema=1, producer_pid=producer.proc.pid, at=time.time(),
                           since=time.time() - 700, input_mode='all-input', present=False,
                           drivable=True, reasons=['idle'], degraded=[],
                           evidence=dict(locked=False, meeting=False, idle_seconds=700))
            write_private(work / 'verdict.json', verdict)
            policy = dict(schema=1, verdict_path='/work/verdict.json', display=':71',
                          producer_pid=producer.proc.pid, pid=target['pid'],
                          window_id=target['target'], max_age_ms=15000)
            write_private(work / 'policy.json', policy)
            gated = {**env, 'CUA_DRIVER_IDLE_CLICK_POLICY': '/work/policy.json'}
            manifest = json.loads(subprocess.check_output(['/native/cua-driver', 'manifest'], env=gated))
            assert manifest['idle_click']['enabled']
            argv = manifest['mcp_invocation']
            assert argv['args'] == ['mcp', '--idle-click-policy', '/work/policy.json', '--no-overlay']
            driver = JsonProcess([argv['command'], *argv['args']], gated, 'driver-' + name, owned)
            init = driver.request(1, 'initialize', dict(protocolVersion='2024-11-05', capabilities={},
                                  clientInfo=dict(name='private-native-contract', version='1')))
            assert 'error' not in init
            driver.send(dict(jsonrpc='2.0', method='notifications/initialized'))
            inventory = driver.request(2, 'tools/list')['result']
            assert {t['name'] for t in inventory['tools']} == {'click', 'start_session', 'end_session', 'list_windows', 'get_window_state'}
            assert 'input.pointer.click.x11_send_event_idle_v1' in inventory['tools'][0]['capabilities']
            assert not driver.tool(3, 'start_session', dict(session=name))['result'].get('isError')
            point: dict[str, Any] = dict(pid=target['pid'], window_id=target['target'], x=30, y=30,
                         button='left', count=1, session=name)
            return fixture, producer, target, driver, verdict, point, manifest

        def finish(name, fixture, producer, driver, before, receipt, expected):
            after = fixture.state()
            time.sleep(0.15)
            later = fixture.state()
            stable(before, after)
            stable(before, later)
            assert (after['press'], after['release'], after['toggled']) == expected
            assert (later['press'], later['release'], later['toggled']) == expected
            if receipt is not None:
                retired = driver.tool(90, 'click', dict(pid=1, window_id=1, x=0, y=0))['result']
                assert retired['isError']
                assert fixture.state()['press'] == expected[0]
            rows.append(dict(name=name, before=before, receipt=receipt, after=after,
                             later=later, child_pid=driver.proc.pid))
            driver.stop(); fixture.stop(); producer.stop()

        for number in range(6):
            name = f'useful-{number}'
            fixture, producer, target, driver, verdict, point, manifest = begin(name)
            before = fixture.state()
            point['button'] = ['left', 'middle', 'right'][number % 3]
            response = driver.tool(10, 'click', point)['result']
            assert not response['isError'] and response['structuredContent']['effect'] == 'unverifiable'
            assert response['structuredContent']['cleanup_complete']
            finish(name, fixture, producer, driver, before, response, (1, 1, 1))

        mutations = ['present', 'stale', 'future', 'locked', 'unknown', 'malformed', 'missing',
                     'discount', 'degraded', 'pid', 'window', 'double', 'foreground', 'modifiers',
                     'capture_after', 'producer-lost', 'unmapped', 'symlink', 'public-verdict', 'backwards']
        for name in mutations:
            fixture, producer, target, driver, verdict, point, manifest = begin(name)
            before = fixture.state()
            if name == 'backwards':
                assert not driver.tool(4, 'get_window_state', dict(pid=target['pid'], window_id=target['target']))['result'].get('isError')
                verdict['at'] -= 1
            if name == 'present': verdict.update(present=True, drivable=False, reasons=['input'])
            if name == 'stale': verdict['at'] -= 60
            if name == 'future': verdict['at'] += 60
            if name == 'locked': verdict['evidence']['locked'] = True
            if name == 'unknown': verdict['evidence']['locked'] = None
            if name == 'discount': verdict.pop('input_mode')
            if name == 'degraded': verdict['degraded'] = ['input']
            if name == 'pid': point['pid'] = producer.proc.pid
            if name == 'window': point['window_id'] = target['sibling']
            if name == 'double': point['count'] = 2
            if name == 'foreground': point['delivery_mode'] = 'foreground'
            if name == 'modifiers': point['modifiers'] = ['ctrl']
            if name == 'capture_after': point['capture_after'] = True
            write_private(work / 'verdict.json', verdict)
            if name == 'malformed': (work / 'verdict.json').write_text('{')
            if name == 'missing': (work / 'verdict.json').unlink()
            if name == 'producer-lost': producer.proc.terminate(); producer.proc.wait(timeout=3)
            if name == 'unmapped': fixture.command('unmap', 'unmapped')
            if name == 'symlink':
                (work / 'verdict.json').rename(work / 'saved-verdict.json')
                (work / 'verdict.json').symlink_to('/work/saved-verdict.json')
            if name == 'public-verdict': (work / 'verdict.json').chmod(0o644)
            response = driver.tool(10, 'click', point)['result']
            assert response['isError'] and response['structuredContent']['effect'] == 'refused'
            # Remap only this private generated fixture so its pixels can be sampled.
            if name == 'unmapped':
                fixture.command('map', 'mapped')
            finish(name, fixture, producer, driver, before, response, (0, 0, 0))

        cancellations = ['return-before-press', 'cancel-before-press', 'return-between', 'cancel-between',
                     'end-between', 'busy-between', 'regrant-between', 'eof-between', 'term-between',
                     'interrupt-between', 'destroy-between', 'reuse-between', 'broken-output-between']
        for name in cancellations:
            fixture, producer, target, driver, verdict, point, manifest = begin(name)
            before = fixture.state()
            if name.endswith('before-press'):
                fixture.command('grab', 'grabbed')
            driver.send(dict(jsonrpc='2.0', id=10, method='tools/call', params=dict(name='click', arguments=point)))
            if name.endswith('before-press'):
                # Hold the real X server while the native worker waits for resolution.
                assert not select.select([driver.stdout], [], [], 0.05)[0]
                if name.startswith('return'):
                    verdict.update(at=time.time(), present=True, drivable=False, reasons=['input'])
                    write_private(work / 'verdict.json', verdict)
                else:
                    driver.send(dict(jsonrpc='2.0', method='notifications/cancelled', params=dict(requestId=10)))
                    assert driver.request(8, 'ping')['result'] == {}
                fixture.command('ungrab', 'grabbed')
            else:
                fixture.receive(lambda r: r.get('event') == 'press')
                if name == 'return-between':
                    verdict.update(at=time.time(), present=True, drivable=False, reasons=['input'])
                    write_private(work / 'verdict.json', verdict)
                if name == 'regrant-between':
                    verdict.update(at=time.time(), since=time.time() - 1)
                    write_private(work / 'verdict.json', verdict)
                if name == 'cancel-between':
                    driver.send(dict(jsonrpc='2.0', method='notifications/cancelled', params=dict(requestId=10)))
                if name == 'end-between':
                    driver.send(dict(jsonrpc='2.0', id=11, method='tools/call', params=dict(name='end_session', arguments={})))
                if name == 'busy-between':
                    driver.send(dict(jsonrpc='2.0', id=11, method='tools/call', params=dict(name='click', arguments=point)))
                    driver.send(dict(jsonrpc='2.0', method='notifications/cancelled', params=dict(requestId=10)))
                if name == 'eof-between': driver.stdin.close()
                if name == 'term-between': driver.proc.send_signal(signal.SIGTERM)
                if name == 'interrupt-between': driver.proc.send_signal(signal.SIGINT)
                if name == 'broken-output-between': driver.stdout.close()
                if name == 'destroy-between': fixture.command('destroy', 'destroyed')
                if name == 'reuse-between': fixture.command('reuse', 'reused')
            receipt = None
            if name in {'eof-between', 'term-between', 'interrupt-between', 'broken-output-between'}:
                driver.proc.wait(timeout=3)
            else:
                receipt = driver.receive(lambda r: r.get('id') == 10)['result']
                assert receipt['isError']
                if name == 'end-between':
                    end = driver.receive(lambda r: r.get('id') == 11)['result']
                    assert end['structuredContent']['cleanup_complete']
                if name == 'busy-between':
                    busy = [r for r in driver.messages if r.get('id') == 11]
                    if not busy: busy = [driver.receive(lambda r: r.get('id') == 11)]
                    assert busy[0]['result']['structuredContent']['code'] == 'idle_click_busy'
                if name in {'destroy-between', 'reuse-between'}:
                    assert receipt['structuredContent']['code'] == 'idle_click_cleanup_uncertain'
                    end = driver.tool(12, 'end_session', {})['result']
                    assert end['structuredContent']['cleanup_complete'] is False
                else:
                    assert receipt['structuredContent']['cleanup_complete']
            expected = (0, 0, 0) if name.endswith('before-press') else (1, 1, 1)
            if name in {'destroy-between', 'reuse-between'}: expected = (1, 0, 0)
            finish(name, fixture, producer, driver, before, receipt, expected)

        fixture, producer, target, driver, verdict, point, manifest = begin('reversible')
        before = fixture.state()
        first = driver.tool(10, 'click', point)['result']
        assert not first['isError']
        assert fixture.state()['pixel'] != before['pixel']
        driver.stop(); producer.stop()
        fixture, producer, target, driver, verdict, point, manifest = begin('explicit-undo', (fixture, target))
        second = driver.tool(10, 'click', point)['result']
        assert not second['isError']
        assert fixture.state()['pixel'] == before['pixel']
        finish('reversible', fixture, producer, driver, before, second, (2, 2, 0))

        # Exact exported consumer helpers are run, not reimplemented. Only their
        # environment/config facade is a generated test harness, not installed Hermes.
        if Path('/consumer/cua_backend_driver.py').exists():
            spec = importlib.util.spec_from_file_location('consumer_driver', '/consumer/cua_backend_driver.py')
            assert spec is not None and spec.loader is not None
            consumer = importlib.util.module_from_spec(spec); spec.loader.exec_module(consumer)
            fixture, producer, target, driver, verdict, point, manifest = begin('consumer')
            gated = {**env, 'CUA_DRIVER_IDLE_CLICK_POLICY': '/work/policy.json'}
            consumer._cb = lambda: types.SimpleNamespace(
                _run_driver=lambda command, *args, **kw: subprocess.run([command, *args], env=gated,
                    capture_output=True, text=True, timeout=kw.get('timeout', 6)), _cua_no_overlay=lambda: True)
            assert consumer._manifest_contract_reason(manifest) == ''
            command, args = consumer._resolve_mcp_invocation('/native/cua-driver')
            assert args[:4] == ['mcp', '--idle-click-policy', '/work/policy.json', '--no-overlay']
            assert command == '/native/cua-driver'
            before = fixture.state()
            helpers = exported_backend(driver)
            armed = helpers.capture(mode='ax', pid=target['pid'], window_id=target['target'])
            assert armed.png_b64 is None and armed.elements == []
            assert (helpers._active_pid, helpers._active_window_id) == (target['pid'], target['target'])
            action = helpers.click(x=30, y=30, button='left', delivery_mode='background', bring_to_front=False)
            assert action.ok and action.effect == 'unverifiable'
            after = fixture.state()
            stable(before, after)
            assert (after['press'], after['release'], after['toggled']) == (1, 1, 1)
            assert helpers._session.calls[-1]['args'] == dict(pid=target['pid'], window_id=target['target'],
                x=30, y=30, button='left', session='generated-consumer')
            (work / 'consumer-compatibility.json').write_text(json.dumps(dict(manifest=manifest,
                resolved_command=command, resolved_args=args, manifest_contract_reason='',
                exact_exported_helper_native_click=True, helper_calls=helpers._session.calls,
                before=before, after=after, effect=action.effect,
                installed_handler_proven=False), indent=2))
            driver.stop(); fixture.stop(); producer.stop()
    finally:
        for child in reversed(owned): child.stop()
        (work / 'results.json').write_text(json.dumps(rows, indent=2))
        (work / 'children.json').write_text(json.dumps([dict(pid=p.proc.pid, exit=p.proc.returncode) for p in owned]))
    assert len(rows) == 6 + len(mutations) + len(cancellations) + 1, len(rows)
    assert all(p.proc.returncode is not None for p in owned)
    summary = dict(contracts=len(rows), passed=True, child_count=len(owned), all_children_waited=True,
                   hyprland_ext_idle_proven=False, physical_return_proven=False,
                   installed_hermes_handler_proven=False)
    (work / 'summary.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary))


def main():
    if sys.argv[1:] == ['--inside']:
        inside(); return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('scratch', type=Path)

    parser.add_argument('--consumer-export', type=Path)
    args = parser.parse_args()
    work = args.scratch.resolve(); work.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = Path(__file__).resolve().parent
    shutil.copy2(source / 'fixture.c', work / 'fixture.c')
    shutil.copy2(Path(__file__), work / 'run.py')
    flags = subprocess.check_output(['pkg-config', '--cflags', '--libs', 'x11', 'xscrnsaver', 'x11-xcb', 'xcb'], text=True).split()
    subprocess.run(['cc', '-Wall', '-Wextra', '-Werror', '-O1', str(work / 'fixture.c'), '-o', str(work / 'fixture'), *flags], check=True)
    command = ['bwrap', '--unshare-all', '--die-with-parent', '--ro-bind', '/usr', '/usr',
               '--symlink', 'usr/bin', '/bin', '--symlink', 'usr/bin', '/sbin',
               '--symlink', 'usr/lib', '/lib', '--symlink', 'usr/lib', '/lib64',
               '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/run', '--tmpfs', '/tmp',
               '--bind', str(work), '/work', '--dir', '/native',
               '--ro-bind', str(args.binary.resolve()), '/native/cua-driver', '--chdir', '/work',
               '--clearenv', '--setenv', 'PATH', '/usr/bin:/bin']
    if args.consumer_export:
        command += ['--ro-bind', str(args.consumer_export.resolve()), '/consumer']
    command += ['/usr/bin/python3', '/work/run.py', '--inside']
    subprocess.run(command, check=True)


if __name__ == '__main__':
    main()
