import concurrent.futures
import fcntl
import hashlib
import importlib.util
import json
import os
import pathlib
import select
import shutil
import signal
import subprocess
import sys
import threading
import time
import tempfile
import types
import unittest
import unittest.mock

spec = importlib.util.spec_from_file_location('probe', pathlib.Path(__file__).with_name('android_sdk_probe.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

ADB = 'Android Debug Bridge version 1.0.41\nVersion 37.0.1-15733141\n'
EMU = 'Android emulator version 37.2.12.0 (build_id 16428233) (CL:N/A)\n'
IMAGE = 'Pkg.Revision=9\nAndroidVersion.ApiLevel=35\nAndroidVersion.ExtensionLevel=13\nSystemImage.Abi=x86_64\nSystemImage.TagId=google_apis\n'
EMU_PROPERTIES = 'Pkg.Revision=37.2.12\nPkg.BuildId=16428233\n'

# Public pipe fixture, not SDK code. Client0 waits for client1's finite output completion.
# A serial communicate(client0) cannot progress while client1's stdout is full/undrained.
PIPE_FIXTURE = '''
import fcntl
import json
import os
import pathlib
import sys
role, payload, errors, acknowledgment, checkpoint, exit_code = sys.argv[1:]
wire = pathlib.Path(payload).read_bytes()
error_wire = pathlib.Path(errors).read_bytes()
print("SDK_PROBE_CAPTURE_READY", flush=True)
sys.stdin.readline()
if role == '0':
    if os.read(int(acknowledgment), 1) != b'D':
        sys.exit(1)
    sys.stdout.buffer.write(wire)
else:
    flags = fcntl.fcntl(1, fcntl.F_GETFL)
    fcntl.fcntl(1, fcntl.F_SETFL, flags | os.O_NONBLOCK)
    sent = 0
    while sent < len(wire):
        try:
            sent += os.write(1, wire[sent:])
        except BlockingIOError:
            break
    fcntl.fcntl(1, fcntl.F_SETFL, flags)
    snapshot = {'capacity': fcntl.fcntl(1, fcntl.F_GETPIPE_SZ), 'sent': sent,
                'total': len(wire), 'blocked': sent < len(wire)}
    os.write(int(checkpoint), (json.dumps(snapshot) + '\\n').encode())
    sys.stdout.buffer.write(wire[sent:])
sys.stdout.buffer.flush()
sys.stderr.buffer.write(error_wire)
sys.stderr.buffer.flush()
if role == '1':
    os.write(int(acknowledgment), b'D')
sys.exit(int(exit_code))
'''


# Controller/descendants are owned pure fixtures: no SDK/device/privileged actions.
CANCELLATION_FIXTURE = '''
import concurrent.futures
import importlib.util
import json
import os
import signal
import sys
import threading
import types
import unittest.mock
spec = importlib.util.spec_from_file_location('tests', sys.argv[1])
tests = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tests)
p = tests.probe
base = p.Probe
mode, release_fd = sys.argv[2], int(sys.argv[3])
instances, shutdowns, kills = [], [], []
def emit(event, **fields):
    print(json.dumps(dict(event=event, **fields)), flush=True)

# Rejected009c shape: generic Exception swallows the installed signal exception.
def old_collect(self, clients, expected):
    barrier = threading.Barrier(len(clients) + 1)
    def collect(index):
        barrier.wait(p.READY_TIMEOUT)
        return self._collect_capture(*clients[index], expected[index])
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(clients)) as executor:
        futures = [executor.submit(collect, index) for index in range(len(clients))]
        barrier.wait(p.READY_TIMEOUT)
        outcomes = []
        for (name, process, _), future in zip(clients, futures):
            try:
                outcomes.append(future.result())
            except Exception as error:
                outcome = {'pid': process.pid, 'verified': False, 'collector_error': type(error).__name__}
                self.save(f'{name}-result.json', json.dumps(outcome))
                outcomes.append(outcome)
                if isinstance(error, p.Cancellation):
                    emit('swallowed', alive=[child.returncode is None for child in self.captures])
        return outcomes

class Fixture(tests.MockProbe):
    def __init__(self, *args):
        super().__init__(*args)
        self.captures, self.waiting, self.active, self.attempts = [], [], [], []
        instances.append(self)
    def setup(self):
        return types.SimpleNamespace(pid=123), types.SimpleNamespace(pid=124)
    def boundary(self):
        pass  # The separate Toybox-model controls test this prerequisite.
    def case(self, count, *args):
        self.attempts.append(count)
        self.save('attempts.json', json.dumps(self.attempts))
        if count == 2:
            emit('second-case')
            return
        # Stress two concurrent capturers in the first attempted pure-fixture case.
        return super().case(2, *args)
    def observe(self, *args):
        process = super().observe(*args)
        self.observer_counts[args[0]]['accepted'] = 0  # Cannot mask pending cancellation.
        return process
    def postmortem(self, *args):
        pass
    def start(self, name, command, **kwargs):
        if '-capture-' not in name:
            return super().start(name, command, **kwargs)
        child = 'import os,sys; print("SDK_PROBE_CAPTURE_READY",flush=True); sys.stdin.readline(); os.write(1,b"PUBLIC_CANCEL_PARTIAL\\\\n"); os.write(2,b"PUBLIC_CANCEL_ERROR\\\\n"); os.close(1); os.close(2); os.read(int(sys.argv[1]),1)'
        process = base.start(self, name, [sys.executable, '-c', child, str(release_fd)], pass_fds=(release_fd,), **kwargs)
        ready, active = threading.Event(), threading.Event()
        self.captures.append(process)
        self.waiting.append(ready)
        self.active.append(active)
        original_try_wait = process._try_wait
        def try_wait(flags):
            if flags == 0:
                ready.set()  # CPython holds _waitpid_lock before this blocking waitpid.
            return original_try_wait(flags)
        process._try_wait = try_wait
        original_communicate = process.communicate
        def communicate(input=None, timeout=None):
            active.set()
            try:
                # Pure-fixture stress: real stdlib blocking wait holds its mutex.
                # Production remains120; no elapsed timeout is a pass criterion.
                return original_communicate(input=input, timeout=None)
            finally:
                active.clear()
        process.communicate = communicate
        return process
    def stop(self, name, process):
        if '-capture-' in name:
            if any(active.is_set() for active in self.active):
                raise AssertionError('concurrent actor stop while communicate owns pipes/wait')
            return base.stop(self, name, process)
        return super().stop(name, process)
if mode == 'old':
    Fixture.collect_captures = old_collect
original_result = concurrent.futures.Future.result
announced = False
def result(self, *args, **kwargs):
    global announced
    if not announced and threading.current_thread() is threading.main_thread():
        instance = instances[0]
        for ready in instance.waiting:
            if not ready.wait(p.READY_TIMEOUT):
                raise RuntimeError('fixture blocking wait event missing')
        announced = True
        emit('waiting', captures=[child.pid for child in instance.captures],
             locks=[child._waitpid_lock.locked() for child in instance.captures])
    return original_result(self, *args, **kwargs)
concurrent.futures.Future.result = result
original_executor = concurrent.futures.ThreadPoolExecutor
class Executor(original_executor):
    def shutdown(self, *args, **kwargs):
        shutdowns.append(1)
        emit('shutdown', calls=len(shutdowns), kills=len(kills))
        return super().shutdown(*args, **kwargs)
concurrent.futures.ThreadPoolExecutor = Executor
original_kill = p.subprocess.Popen.kill
def kill(self):
    original_kill(self)
    kills.append(self.pid)
    emit('kill', signal=signal.SIGKILL, pid=self.pid)
p.subprocess.Popen.kill = kill
p.Probe = Fixture
status = 0
with unittest.mock.patch.object(p, 'socket_inventory', return_value=[{'fd': 21}]):
    try:
        p.main()
    except SystemExit as error:
        status = error.code
    except BaseException as error:
        status = 1
        emit('error', type=type(error).__name__)
instance = instances[0]
emit('finished', status=status, attempts=instance.attempts, captures=[child.pid for child in instance.captures],
     exits=[child.returncode for child in instance.captures], shutdowns=len(shutdowns), kills=kills)
sys.exit(status)
'''


def toybox_log_argument(message):
    # android-15.0.0_r1 toys/android/log.c: one argument, no separator; >=1024 warns.
    # lib/lib.c error_msg sets exitval1 even when exactly1024 bytes are emitted intact.
    return {'message': message[:1024], 'exit': int(len(message.encode()) >= 1024),
            'stderr': b'log: log cut at 1024 bytes\n' if len(message.encode()) >= 1024 else b''}


class MockProbe(probe.Probe):
    """Pure subprocess fixtures: never execute SDK, sudo, tcpdump, strace or attach to /proc."""
    def __init__(self, output, sdk, corrupt=False):
        super().__init__(output, sdk)
        self.corrupt = corrupt
        self.events = []
        self.expected = {}
        self.retired = []
        self.observer_commands = []

    def owned_executables(self, adb, emulator):
        return {'adb': {'pid': adb.pid}, 'qemu': {'pid': emulator.pid}}

    def device(self, name, *arguments, **kwargs):
        self.events.append(name)
        if name.endswith('-producer'):
            for line in kwargs['input_bytes'].decode().splitlines()[1:]:
                tag = line.split(' ')[4]
                result = toybox_log_argument(line.partition("'")[2][:-1])
                self.expected.setdefault(tag, []).append(result['message'])
                self.save(name + '.exit', str(result['exit']))
                (self.output / (name + '.stderr')).write_bytes(result['stderr'])
                if result['exit']:
                    raise RuntimeError(f'{name} exited {result["exit"]}')
        if name == 'boundary-capture':
            return ('\n'.join(self.expected['SDKBOUNDARY']) + '\n').encode()
        return (name.removesuffix('-ack') + '-complete').encode() if name.endswith('-ack') else b'1234.5678'

    def observe(self, name, command, parser, readiness):
        self.events.append(name)
        self.observer_commands.append(command)
        self.observer_counts[name] = {'accepted': 1, 'discarded': 0}
        return self.start(name, [sys.executable, '-c', 'import sys; sys.stdin.read()'], stdin=subprocess.PIPE)

    def start(self, name, command, **kwargs):
        if '-capture-' in name:
            tag = command[-2].removesuffix(':I')
            records = self.expected[tag]
            if self.corrupt is True and tag.endswith('_0'):
                records = records[:-1]
            public = '\n'.join(records) + '\n'
            fixture = self.private / (name + '.txt')
            fixture.write_text(public)
            command = [sys.executable, '-c', 'import pathlib,sys; print("SDK_PROBE_CAPTURE_READY", flush=True); sys.stdin.readline(); sys.stdout.write(pathlib.Path(sys.argv[1]).read_text()); sys.exit(int(sys.argv[2]))', str(fixture), '255' if self.corrupt == 'exit255' and tag.endswith('_0') else '0']
        return super().start(name, command, **kwargs)

    def stop(self, name, process):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=5)
        self.save(name + '.exit', str(process.returncode))
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None and not stream.closed:
                stream.close()
        self.retired.append(process.pid)


class MockSetup(probe.Probe):
    def __init__(self, output, sdk, failure):
        super().__init__(output, sdk)
        self.failure = failure
        self.boot_command = None

    def run(self, name, command, input_bytes=None, timeout=probe.COMMAND_TIMEOUT):
        if name in ['strace-help', 'strace-version', 'tcpdump-help', 'tcpdump-version']:
            return b'mock tool metadata'
        match name:
            case 'adb-version':
                return ADB.encode()
            case 'emulator-version':
                return EMU.encode()
            case 'create-avd':
                return b'created owned mock AVD'
            case 'boot-event':
                self.boot_command = command
                if self.failure == 'boot':
                    raise subprocess.CalledProcessError(124, command)
                return b'Finished processing BOOT_COMPLETED for u0'
            case 'boot-property':
                return b'0' if self.failure == 'property' else b'1'
            case 'package-manager':
                return b'' if self.failure == 'package' else b'package:/system/framework/framework-res.apk'
        raise AssertionError('unexpected mock command')

    def start(self, name, command, **kwargs):
        if name == 'adb-server':
            descriptor = int(command[-1])
            ack = b'BAD' if self.failure == 'ack' else b'OK\n'
            command = [sys.executable, '-c', 'import os,signal,sys; os.write(int(sys.argv[1]), bytes.fromhex(sys.argv[2])); signal.pause()', str(descriptor), ack.hex()]
        elif name == 'emulator':
            command = [sys.executable, '-c', 'import signal; signal.pause()']
        else:
            raise AssertionError('unexpected mock actor')
        return super().start(name, command, **kwargs)


class ProbeTest(unittest.TestCase):
    def test_actual_input_identity_and_foreign_or_missing_rejection(self):
        probe.verify_inputs(ADB, EMU, IMAGE, EMU_PROPERTIES)
        for index, foreign in [(0, ADB.replace('37.0.1', '35.0.2')), (0, ADB.replace('15733141', '15733142')),
                               (1, EMU.replace('16428233', '16428234')), (2, IMAGE.replace('Pkg.Revision=9', 'Pkg.Revision=8')),
                               (2, IMAGE.replace('x86_64', 'arm64-v8a')), (2, IMAGE.replace('ExtensionLevel=13', 'ExtensionLevel=12')),
                               (2, IMAGE.replace('ApiLevel=35', 'ApiLevel=24')), (3, '')]:
            with self.subTest(index=index, foreign=foreign):
                values = [ADB, EMU, IMAGE, EMU_PROPERTIES]
                values[index] = foreign
                with self.assertRaises(ValueError):
                    probe.verify_inputs(*values)
        with self.assertRaises(ValueError):
            probe.properties('Pkg.Revision=9\nPkg.Revision=9\n')

    def test_exact_public_workload_all_records_bytes_and_hashes(self):
        for tag, budget in zip(['SDKPROBE2_0', 'SDKPROBE2_1'], probe.BUDGETS):
            expected = probe.messages(tag, budget)
            self.assertEqual(sum(map(len, expected)), budget)
            self.assertTrue(all(len(message.encode()) + len(tag) + 3 <= probe.MAX_PAYLOAD for message in expected))
            self.assertEqual(max(map(len, expected)), 1023)
            self.assertEqual(len(expected), 1743 if budget == probe.BUDGETS[0] else 1564)
            self.assertTrue(all(toybox_log_argument(message)['exit'] == 0 for message in expected))
            output = ('--------- beginning of main\n' + '\n'.join(expected) + '\n').encode()
            result = probe.validate_capture(output, expected)
            self.assertEqual(result['message_bytes'], budget)
            self.assertEqual(result['records'], len(expected))
            for bad in [expected[:-1], expected + [expected[0]], expected[::-1], ['PUBLIC_ACK_ONLY'],
                        [expected[0][:-1]] + expected[1:], expected + ['PRIVATE_KEY_SENTINEL']]:
                with self.assertRaises(ValueError):
                    probe.validate_capture(('\n'.join(bad) + '\n').encode(), expected)

    def test_field_only_tcp_and_raw_syscall_privacy(self):
        sentinel = 'DO_NOT_ARCHIVE_PRIVATE_KEY_PASSWORD'
        row = probe.tcp_fields('1234.567 IP 127.0.0.1.44444 > 127.0.0.1.5559: Flags [P.], seq 1:99, ack 2, win 1, '
                               f'options [{sentinel}], length 98')
        self.assertEqual(row['seq'], '1:99')
        self.assertNotIn(sentinel, json.dumps(row))
        self.assertEqual(row['length'], 98)
        for line in [f'\t0x0000: {sentinel}', f'1234.567 IP 127.0.0.1.44444 > 127.0.0.1.5037: Flags [.], length 1',
                     f'1234.567 IP 192.168.1.1.44444 > 127.0.0.1.5559: Flags [.], length 1']:
            self.assertIsNone(probe.tcp_fields(line))
        raw = '[pid 123] 1234.567 read(0x15, 0x7abcdef, 0x1000) = -1 ECONNRESET (Connection reset by peer)'
        row = probe.syscall_fields(raw, {21})
        self.assertEqual(row['errno'], 'ECONNRESET')
        self.assertEqual(row['arguments'], [21, 0x7abcdef, 4096])
        timed = probe.syscall_fields('123 1234.567 read(0x15, 0x7abcdef, 0x1000) = 0x0 <0.000025>', {21})
        self.assertEqual(timed['elapsed'], '0.000025')
        self.assertEqual(timed['result'], 0)
        self.assertIsNone(probe.syscall_fields(raw, {22}))
        self.assertIsNone(probe.syscall_fields(f'[pid 123] 1234.567 read(21, "{sentinel}", 1) = 1', {21}))
        self.assertIsNone(probe.syscall_fields(f'[pid 123] 1234.567 execve("{sentinel}", [], []) = 0', {21}))

    def test_socket_owner_uid_port_and_fd_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'net').mkdir()
            (root / '123/fd').mkdir(parents=True)
            (root / 'net/tcp').write_text(f'header\n0: 0100007F:ABCD 0100007F:15B7 01 0:0 0:0 0 {os.getuid()} 0 999\n')
            (root / '123/fd/21').symlink_to('socket:[999]')
            inventory = probe.socket_inventory(123, root)
            self.assertEqual(inventory[0]['fd'], 21)
            self.assertEqual(inventory[0]['inode'], '999')
            with unittest.mock.patch.object(probe.os, 'getuid', return_value=os.getuid() + 1):
                with self.assertRaises(ValueError):
                    probe.socket_inventory(123, root)
            (root / 'net/tcp').write_text('header\n')
            with self.assertRaises(ValueError):
                probe.socket_inventory(123, root)

    def test_two_finite_mock_cases_and_failure_actor_retirement(self):
        for corrupt in [False, True, 'exit255']:
            with self.subTest(corrupt=corrupt), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                    instance = MockProbe(root / 'public', root / 'sdk', corrupt)
                owner = types.SimpleNamespace(pid=123)
                emulator = types.SimpleNamespace(pid=124)
                inventory = [{'pid': 123, 'fd': 21, 'uid': os.getuid(), 'inode': '999', 'local': '0100007F:ABCD', 'remote': '0100007F:15B7'}]
                try:
                    with unittest.mock.patch.object(probe, 'socket_inventory', return_value=inventory):
                        for count in (1, 2):
                            if corrupt:
                                with self.assertRaises(ValueError):
                                    instance.case(count, owner, emulator)
                                result = json.loads((root / 'public' / f'case-{count}-result.json').read_text())
                                if corrupt == 'exit255' and count == 2:
                                    self.assertEqual([client['exit'] for client in result['clients']], [255, 0])
                                    self.assertTrue(result['clients'][1]['verified'])
                            else:
                                instance.case(count, owner, emulator)
                                result = json.loads((root / 'public' / f'case-{count}-result.json').read_text())
                                self.assertEqual(len(result['clients']), count)
                                self.assertFalse(result['application_acceptance'])
                                self.assertTrue(result['traffic_verified'])
                            self.assertTrue(all(process.poll() is not None for _, process in instance.actors))
                finally:
                    instance.close()
                self.assertEqual(sum('-capture-' in name for name, _ in instance.actors), 3)
                self.assertEqual([command[-1] for command in instance.observer_commands if 'strace' in command], ['123', '123'])
                for _, process in instance.actors:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(process.pid, 0)

    def test_pipe_capacity_serial_red_concurrent_green_and_exit255_peer(self):
        for mode in ['serial', 'concurrent', 'exit255']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                    instance = MockProbe(root / 'public', root / 'sdk')
                acknowledgment_read, acknowledgment_write = os.pipe()
                checkpoint_read, checkpoint_write = os.pipe()
                clients = []
                expected = [probe.messages(f'SDKPROBE2_{index}', budget) for index, budget in enumerate(probe.BUDGETS)]
                wires = [('\n'.join(records) + '\n').encode() for records in expected]
                old_thread = None
                old_result = []
                old_done = threading.Event()
                try:
                    for index in range(2):
                        payload = root / f'payload-{index}'
                        payload.write_bytes(wires[index])
                        errors = root / f'errors-{index}'
                        # Both stdout and stderr exceed their actual pipe capacities; no timing ratio.
                        errors.write_bytes(wires[index])
                        ack_fd = acknowledgment_read if index == 0 else acknowledgment_write
                        process = probe.Probe.start(instance, f'pipe-{index}', [sys.executable, '-c', PIPE_FIXTURE,
                            str(index), str(payload), str(errors), str(ack_fd), str(checkpoint_write),
                            '255' if mode == 'exit255' and index == 0 else '0'],
                            pass_fds=(ack_fd, checkpoint_write), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                        self.assertEqual(process.stdout.readline(), b'SDK_PROBE_CAPTURE_READY\n')
                        self.assertGreater(len(wires[index]), fcntl.fcntl(process.stdout.fileno(), fcntl.F_GETPIPE_SZ))
                        self.assertGreater(len(wires[index]), fcntl.fcntl(process.stderr.fileno(), fcntl.F_GETPIPE_SZ))
                        clients.append((f'pipe-{index}', process, time.time_ns()))
                    os.close(acknowledgment_read)
                    os.close(acknowledgment_write)
                    os.close(checkpoint_write)
                    if mode == 'serial':
                        def old_collect():
                            old_result.append(clients[0][1].communicate(input=b'capture\n', timeout=probe.COMMAND_TIMEOUT))
                            old_done.set()
                        old_thread = threading.Thread(target=old_collect)
                        old_thread.start()
                        clients[1][1].stdin.write(b'capture\n')
                        clients[1][1].stdin.close()
                        clients[1][1].stdin = None
                        self.assertTrue(select.select([checkpoint_read], [], [], probe.READY_TIMEOUT)[0])
                        checkpoint = json.loads(os.read(checkpoint_read, 4096))
                        self.assertTrue(checkpoint['blocked'])
                        self.assertLess(checkpoint['sent'], checkpoint['total'])
                        self.assertFalse(old_done.is_set())
                        self.assertIsNone(clients[1][1].poll())
                        # The full-pipe event proves the dependency stall; no sleep/timeout-to-pass.
                        for _, process, _ in clients:
                            os.killpg(process.pid, signal.SIGTERM)
                        old_thread.join(probe.READY_TIMEOUT)
                        self.assertFalse(old_thread.is_alive())
                        self.assertEqual(old_result[0][0], b'')
                        second_output, second_error = clients[1][1].communicate(timeout=probe.READY_TIMEOUT)
                        (instance.output / 'pipe-0.stdout').write_bytes(old_result[0][0])
                        (instance.output / 'pipe-0.stderr').write_bytes(old_result[0][1])
                        (instance.output / 'pipe-1.stdout').write_bytes(second_output)
                        (instance.output / 'pipe-1.stderr').write_bytes(second_error)
                        self.assertEqual(second_output, wires[1][:checkpoint['sent']])
                        instance.save('checkpoint.json', json.dumps(checkpoint))
                    else:
                        outcomes = instance.collect_captures(clients, expected)
                        self.assertEqual([outcome['exit'] for outcome in outcomes], [255 if mode == 'exit255' else 0, 0])
                        self.assertEqual([outcome['verified'] for outcome in outcomes], [mode != 'exit255', True])
                        for index, outcome in enumerate(outcomes):
                            self.assertEqual((instance.output / f'pipe-{index}.stdout').read_bytes(), wires[index])
                            self.assertEqual((instance.output / f'pipe-{index}.stderr').read_bytes(), wires[index])
                            self.assertEqual(outcome['wire_output_sha256'], hashlib.sha256(wires[index]).hexdigest())
                        self.assertTrue(select.select([checkpoint_read], [], [], probe.READY_TIMEOUT)[0])
                        instance.save('checkpoint.json', os.read(checkpoint_read, 4096).decode().strip())
                finally:
                    for _, process, _ in clients:
                        if process.poll() is None:
                            os.killpg(process.pid, signal.SIGTERM)
                    if old_thread is not None:
                        old_thread.join(probe.READY_TIMEOUT)
                    instance.close()
                    for descriptor in (acknowledgment_read, acknowledgment_write, checkpoint_read, checkpoint_write):
                        try:
                            os.close(descriptor)
                        except OSError:
                            pass
                for _, process, _ in clients:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(process.pid, 0)
                # Optional test-only receipt destination; never interpreted by the SDK helper.
                if destination := os.environ.get('SDK_PROBE_TEST_RECEIPTS'):
                    shutil.copytree(instance.output, pathlib.Path(destination) / mode)
                    (pathlib.Path(destination) / mode / 'fixture.py').write_text(PIPE_FIXTURE)

    def test_public_cli_boundary_preflight_exact_and_negative(self):
        for failure in ['', 'truncated', 'marker']:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                    instance = MockProbe(root / 'public', root / 'sdk')
                original = instance.device
                def boundary_device(name, *arguments, **kwargs):
                    output = original(name, *arguments, **kwargs)
                    if name == 'boundary-capture':
                        if failure == 'truncated':
                            return output[:-2] + b'\n'
                        if failure == 'marker':
                            return b'PUBLIC_ACK_ONLY\n'
                    return output
                try:
                    with unittest.mock.patch.object(instance, 'device', side_effect=boundary_device):
                        if failure:
                            with self.assertRaisesRegex(ValueError, 'CLI boundary preflight failed'):
                                instance.boundary()
                        else:
                            instance.boundary()
                            result = json.loads((instance.output / 'boundary-result.json').read_text())
                            self.assertEqual(result['message_bytes'], 1023)
                            self.assertEqual(result['records'], 1)
                            self.assertTrue(result['verified'])
                            self.assertFalse(result['concurrency_case'])
                    self.assertFalse(any(name.startswith('case-') for name in instance.events))
                    self.assertEqual(instance.events.count('boundary-producer'), 1)
                finally:
                    instance.close()

    def test_boundary_failure_stops_main_before_concurrency_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            environment = {'RUNNER_TEMP': directory, 'ANDROID_HOME': str(root / 'sdk'), 'GITHUB_ACTIONS': 'true',
                'GITHUB_EVENT_NAME': 'workflow_dispatch', 'GITHUB_REPOSITORY': 'JafarAbdi/wezterm', 'GITHUB_SHA': 'public-mock-source'}
            with unittest.mock.patch.dict(os.environ, environment):
                instance = MockProbe(root / 'public', root / 'sdk')
                with unittest.mock.patch.object(probe, 'Probe', return_value=instance), \
                     unittest.mock.patch.object(instance, 'setup', return_value=(None, None)), \
                     unittest.mock.patch.object(instance, 'boundary', side_effect=ValueError('public CLI boundary preflight failed')), \
                     unittest.mock.patch.object(instance, 'case') as cases, \
                     unittest.mock.patch.object(probe.signal, 'signal'):
                    with self.assertRaises(ValueError):
                        probe.main()
                    cases.assert_not_called()
            self.assertEqual(json.loads((instance.output / 'retirement.json').read_text())['errors'], [])

    def test_toybox_single_argument_cap_and_legacy4054_rejected(self):
        self.assertEqual(probe.CLI_MAX_MESSAGE, 1023)
        self.assertEqual(probe.message_limit('SDKBOUNDARY'), 1023)
        self.assertEqual(probe.message_limit('t' * (probe.MAX_PAYLOAD - 103)), 100)
        for size in [1023, 1024, 4054]:
            result = toybox_log_argument('x' * size)
            self.assertEqual(len(result['message']), min(size, 1024))
            self.assertEqual(result['exit'], int(size >= 1024))
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                instance = MockProbe(root / 'public', root / 'sdk')
            try:
                with unittest.mock.patch.object(probe, 'message_limit', return_value=4054):
                    with self.assertRaisesRegex(RuntimeError, 'boundary-producer exited 1'):
                        instance.boundary()
                self.assertEqual(len(instance.expected['SDKBOUNDARY'][0]), 1024)
                self.assertEqual((instance.output / 'boundary-producer.exit').read_text().strip(), '1')
                self.assertEqual((instance.output / 'boundary-producer.stderr').read_bytes(), b'log: log cut at 1024 bytes\n')
                self.assertNotIn('boundary-capture', instance.events)
                self.assertFalse(any(name.startswith('case-') for name in instance.events))
            finally:
                instance.close()
            if destination := os.environ.get('SDK_PROBE_TEST_RECEIPTS'):
                shutil.copytree(instance.output, pathlib.Path(destination) / 'legacy-cli4054')

    def test_worker_errors_and_cancelled_future_are_not_operator_cancellation(self):
        for error in [concurrent.futures.CancelledError(), InterruptedError('worker IO error')]:
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                    instance = MockProbe(root / 'public', root / 'sdk')
                clients = []
                try:
                    for index in range(2):
                        process = probe.Probe.start(instance, f'error-{index}', [sys.executable, '-c', 'import sys; sys.stdin.read()'],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                        clients.append((f'error-{index}', process, time.time_ns()))
                    def collect(name, process, started, expected):
                        process.communicate(input=b'capture\n', timeout=probe.COMMAND_TIMEOUT)
                        if name.endswith('0'):
                            raise error
                        return {'verified': True, 'exit': process.returncode}
                    with unittest.mock.patch.object(instance, '_collect_capture', side_effect=collect), \
                         unittest.mock.patch.object(probe.subprocess.Popen, 'kill') as signals:
                        outcomes = instance.collect_captures(clients, [[], []])
                        signals.assert_not_called()
                    self.assertFalse(outcomes[0]['verified'])
                    self.assertEqual(outcomes[0]['collector_error'], type(error).__name__)
                    self.assertTrue(outcomes[1]['verified'])
                finally:
                    instance.close()

    def test_real_owned_signal_cancellation_red_green_and_wait_mutex(self):
        for signum in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
            for mode in ['old', 'fixed']:
                with self.subTest(signal=signum, mode=mode), tempfile.TemporaryDirectory() as directory:
                    root = pathlib.Path(directory)
                    release_read, release_write = os.pipe()
                    environment = dict(os.environ, RUNNER_TEMP=directory, ANDROID_HOME=str(root / 'sdk'),
                        GITHUB_ACTIONS='true', GITHUB_EVENT_NAME='workflow_dispatch',
                        GITHUB_REPOSITORY='JafarAbdi/wezterm', GITHUB_SHA='PUBLIC_SIGNAL_FIXTURE')
                    controller = subprocess.Popen([sys.executable, '-c', CANCELLATION_FIXTURE, __file__, mode, str(release_read)],
                        env=environment, pass_fds=(release_read,), start_new_session=True,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
                    os.close(release_read)
                    rows, captures = [], []
                    finished = False
                    def read_event():
                        self.assertTrue(select.select([controller.stdout], [], [], probe.READY_TIMEOUT)[0], 'fixture event deadline')
                        row = json.loads(controller.stdout.readline())
                        rows.append(row)
                        return row
                    try:
                        row = read_event()
                        self.assertEqual(row['event'], 'waiting')
                        self.assertEqual(row['locks'], [True, True])
                        captures = row['captures']
                        # Signal only the owned controller, never this test/session.
                        os.kill(controller.pid, signum)
                        if mode == 'old':
                            row = read_event()
                            self.assertEqual(row, {'event': 'swallowed', 'alive': [True, True]})
                            # Event proves lost cancellation/retained captures. Release this red
                            # fixture only so it can demonstrate the erroneous next-case attempt.
                            os.write(release_write, b'RR')
                        while (row := read_event())['event'] != 'finished':
                            pass  # Event stream, not status/sleep polling.
                        finished = True
                        self.assertEqual(controller.wait(timeout=probe.READY_TIMEOUT), 1 if mode == 'old' else 128 + signum)
                        self.assertEqual(row['attempts'], [1, 2] if mode == 'old' else [1])
                        self.assertEqual(row['shutdowns'], 1)
                        self.assertEqual(row['exits'], [0, 0] if mode == 'old' else [-signal.SIGKILL, -signal.SIGKILL])
                        shutdown = next(item for item in rows if item['event'] == 'shutdown')
                        self.assertEqual(shutdown['kills'], 0 if mode == 'old' else 2)
                        self.assertEqual(len(row['kills']), 0 if mode == 'old' else 2)
                        retirement = json.loads((root / 'android-sdk-probe-public/retirement.json').read_text())
                        self.assertEqual(retirement, {'errors': [], 'actors': 4})
                        for pid in row['captures']:
                            with self.assertRaises(ProcessLookupError):
                                os.kill(pid, 0)
                    finally:
                        if not finished:
                            for pid in captures:
                                try:
                                    os.kill(pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                        if controller.poll() is None:
                            controller.kill()
                        controller.wait(timeout=probe.READY_TIMEOUT)
                        os.close(release_write)
                        output, error = controller.communicate(timeout=probe.READY_TIMEOUT)
                    self.assertEqual(output, b'')
                    self.assertEqual(error, b'')
                    if destination := os.environ.get('SDK_PROBE_TEST_RECEIPTS'):
                        receipts = pathlib.Path(destination) / f'signal-{signum}-{mode}'
                        shutil.copytree(root / 'android-sdk-probe-public', receipts)
                        (receipts / 'controller.py').write_text(CANCELLATION_FIXTURE)
                        (receipts / 'events.json').write_text(json.dumps(rows, indent=2) + '\n')
                        (receipts / 'controller.exit').write_text(str(controller.returncode) + '\n')

    def test_owned_exec_image_is_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                instance = probe.Probe(root / 'public', root / 'sdk')
            paths = [root / 'sdk/platform-tools/adb', root / 'sdk/emulator/qemu/linux-x86_64/qemu-system-x86_64-headless']
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('public mock SDK executable')
            owner = types.SimpleNamespace(pid=123)
            try:
                with unittest.mock.patch.object(probe.os, 'readlink', side_effect=list(map(str, paths))):
                    self.assertEqual(set(instance.owned_executables(owner, owner)), {'adb', 'qemu'})
                with unittest.mock.patch.object(probe.os, 'readlink', return_value='/NOT_AN_OWNED_SDK_EXECUTABLE'):
                    with self.assertRaises(ValueError):
                        instance.owned_executables(owner, owner)
            finally:
                instance.close()

    def test_startup_ack_boot_and_fail_closed_gate_controls(self):
        for failure in ['', 'ack', 'boot', 'property', 'package']:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                sdk = root / 'sdk'
                for relative, content in [('platform-tools/adb', 'mock binary'), ('emulator/emulator', 'mock binary'),
                    ('emulator/source.properties', EMU_PROPERTIES), ('system-images/android-35/google_apis/x86_64/source.properties', IMAGE)]:
                    target = sdk / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(content)
                with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory, 'ADB_TRACE': 'transport', 'ADB_VENDOR_KEYS': '/FORBIDDEN_SIGNING_SENTINEL'}):
                    instance = MockSetup(root / 'public', sdk, failure)
                self.assertEqual(instance.environment['ADB_TRACE'], '')
                self.assertEqual(instance.environment['ADB_EMU'], '0')
                self.assertNotIn('ADB_VENDOR_KEYS', instance.environment)
                try:
                    if failure:
                        with self.assertRaises((RuntimeError, subprocess.CalledProcessError)):
                            instance.setup()
                    else:
                        instance.setup()
                        self.assertEqual(instance.boot_command[:2], ['timeout', '600'])
                        self.assertIn('Sending BOOT_COMPLETE user #0|Finished processing BOOT_COMPLETED for u0', instance.boot_command[4])
                        self.assertEqual(instance.boot_command[-1], 'emulator-5558')
                finally:
                    instance.close()
                self.assertTrue(all(process.poll() is not None for _, process in instance.actors))

    def test_observer_absence_fails_not_marker_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            with unittest.mock.patch.dict(os.environ, {'RUNNER_TEMP': directory}):
                instance = MockProbe(root / 'public', root / 'sdk')
            original = instance.observe
            def empty(*args):
                process = original(*args)
                instance.observer_counts[args[0]]['accepted'] = 0
                return process
            owner = types.SimpleNamespace(pid=123)
            inventory = [{'fd': 21}]
            try:
                with unittest.mock.patch.object(probe, 'socket_inventory', return_value=inventory), unittest.mock.patch.object(instance, 'observe', side_effect=empty):
                    with self.assertRaises(RuntimeError):
                        instance.case(1, owner, owner)
            finally:
                instance.close()
            self.assertTrue(all(process.poll() is not None for _, process in instance.actors))

    def test_local_execution_guard(self):
        with unittest.mock.patch.dict(os.environ, {'GITHUB_ACTIONS': 'false'}):
            with self.assertRaises(RuntimeError):
                probe.main()


if __name__ == '__main__':
    unittest.main(verbosity=2)
