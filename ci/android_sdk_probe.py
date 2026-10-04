"""Manual SDK-only buffered-drain experiment; never an application acceptance gate."""

import concurrent.futures
import hashlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import threading
import time

SERIAL = 'emulator-5558'
BUDGETS = (1_782_870, 1_599_914)  # Actual run37206449101 final-second WRTE byte totals.
MAX_PAYLOAD = 4068  # AOSP android-15.0.0_r1 system/logging/liblog/include/log/log.h.
# Android15 external/toybox/toys/android/log.c warns/fails at argument bytes >=1024.
# One ASCII argument needs no joining separator; stpcpy adds its trailing NUL.
CLI_MAX_MESSAGE = 1024 - 1
COMMAND_TIMEOUT = 120  # Empirical diagnostic budget, not a success criterion.
READY_TIMEOUT = 30  # Empirical observer/daemon startup budget.
CALLS = 'read,write,readv,writev,recvfrom,sendto,recvmsg,sendmsg,close,shutdown'


class Cancellation(InterruptedError):
    """Terminal operator signal, distinct from worker errors/cancelled futures."""

    def __init__(self, signum: int):
        super().__init__(f'forge signal {signum}')
        self.signum = signum


def message_limit(tag: str) -> int:
    return min(MAX_PAYLOAD - len(tag.encode()) - 3, CLI_MAX_MESSAGE)


def properties(text: str) -> dict[str, str]:
    result = {}
    for line in text.splitlines():
        if line and not line.startswith('#'):
            key, separator, value = line.partition('=')
            if not separator or key in result:
                raise ValueError('malformed/duplicate SDK property')
            result[key] = value
    return result


def verify_inputs(adb: str, emulator: str, image: str, emulator_properties: str) -> None:
    if re.findall(r'^Version (.+)$', adb, re.MULTILINE) != ['37.0.1-15733141']:
        raise ValueError('wrong platform-tools version/build')
    if re.findall(r'Android emulator version ([^\n]+)', emulator) != ['37.2.12.0 (build_id 16428233) (CL:N/A)']:
        raise ValueError('wrong emulator version/build')
    for actual, expected in [
        (properties(emulator_properties), {'Pkg.Revision': '37.2.12', 'Pkg.BuildId': '16428233'}),
        (properties(image), {'Pkg.Revision': '9', 'AndroidVersion.ApiLevel': '35',
                             'AndroidVersion.ExtensionLevel': '13', 'SystemImage.Abi': 'x86_64',
                             'SystemImage.TagId': 'google_apis'}),
    ]:
        if any(actual.get(key) != value for key, value in expected.items()):
            raise ValueError('wrong/missing SDK package identity')


def messages(tag: str, budget: int) -> list[str]:
    # priority byte + NUL-terminated tag + NUL-terminated message consume liblog's payload.
    limit = message_limit(tag)
    records = []
    remaining = budget
    while remaining:
        prefix = f'PUBLIC_{tag}_{len(records):06d}:'
        size = min(limit, remaining)
        if size < len(prefix):
            raise ValueError('fixed workload cannot fit its sequence identifier')
        records.append(prefix + 'x' * (size - len(prefix)))
        remaining -= min(limit, remaining)
    return records


def buffer_mib() -> int:
    # Same capacity for boundary preflight and both cases, derived from the larger case.
    tags = [f'SDKPROBE2_{index}' for index in range(len(BUDGETS))]
    total = sum(len(record) + 28 + len(tag) + 3 for tag, budget in zip(tags, BUDGETS)
                for record in messages(tag, budget))
    return (2 * total + (1 << 20) - 1) // (1 << 20)


def validate_capture(output: bytes, expected: list[str]) -> dict[str, object]:
    records = output.decode('ascii').splitlines()
    records = [line for line in records if line != '--------- beginning of main']
    if records != expected:
        raise ValueError('missing, duplicate, reordered, truncated or foreign public log record')
    payload = ''.join(records).encode()
    return {'records': len(records), 'message_bytes': len(payload),
            'message_sha256': hashlib.sha256(payload).hexdigest(),
            'wire_output_sha256': hashlib.sha256(output).hexdigest()}


def tcp_fields(line: str) -> dict[str, object] | None:
    # Never preserve tcpdump's original line, options, hex, ASCII or application decoder text.
    if statistic := re.fullmatch(r'(\d+) packets (captured|received by filter|dropped by kernel)\s*', line):
        return {'statistic': statistic[2], 'packets': int(statistic[1])}
    match = re.fullmatch(
        r'(\d+\.\d+) IP (127\.0\.0\.1)\.(\d+) > (127\.0\.0\.1)\.(\d+): '
        r'Flags \[([SFRPAU.EW]*)\], (.*)length (\d+)\s*', line)
    if not match:
        return None
    stamp, source, source_port, destination, destination_port, flags, fields, length = match.groups()
    if 5559 not in (int(source_port), int(destination_port)):
        return None
    result = {'timestamp': stamp, 'source': source, 'source_port': int(source_port),
              'destination': destination, 'destination_port': int(destination_port),
              'flags': flags, 'length': int(length)}
    for name in ['seq', 'ack']:
        if value := re.search(rf'(?:^|, ){name} (\d+(?::\d+)?)(?:, |$)', fields):
            result[name] = value[1]
    return result


def syscall_fields(line: str, descriptors: set[int]) -> dict[str, object] | None:
    # strace --raw=all: pointers/integers only, not decoded buffers/structures or environments.
    match = re.fullmatch(r'(?:\[pid\s+(\d+)\]|(\d+))\s+(\d+\.\d+) '
                         r'([a-z0-9]+)\(([^()]*)\)\s+=\s+(0x[0-9a-f]+|-?\d+)(?: ([A-Z][A-Z0-9]+) \([^\n]*\))?(?: <(\d+\.\d+)>)?\s*', line)
    if not match:
        return None
    bracket_pid, pid, stamp, call, arguments, result, error, elapsed = match.groups()
    if call not in CALLS.split(','):
        return None
    values = arguments.split(', ')
    if not values or any(not re.fullmatch(r'0x[0-9a-f]+|NULL|-?\d+', value) for value in values):
        return None
    numbers = [0 if value == 'NULL' else int(value, 0) for value in values]
    if numbers[0] not in descriptors:
        return None
    return {'entry_timestamp': stamp, 'elapsed': elapsed, 'tid': int(pid or bracket_pid), 'call': call, 'fd': numbers[0],
            'arguments': numbers, 'result': int(result, 0), 'errno': error}


def socket_inventory(pid: int, proc: pathlib.Path = pathlib.Path('/proc')) -> list[dict[str, object]]:
    endpoints = {}
    for line in (proc / 'net/tcp').read_text().splitlines()[1:]:
        fields = line.split()
        local, remote = fields[1], fields[2]
        if int(local.split(':')[1], 16) == 5559 or int(remote.split(':')[1], 16) == 5559:
            if local.split(':')[0] != '0100007F' or remote.split(':')[0] != '0100007F':
                continue
            endpoints[fields[9]] = {'local': local, 'remote': remote, 'uid': int(fields[7]), 'inode': fields[9], 'state': fields[3]}
    result = []
    for descriptor in (proc / str(pid) / 'fd').iterdir():
        try:
            target = os.readlink(descriptor)
        except FileNotFoundError:
            continue
        if match := re.fullmatch(r'socket:\[(\d+)\]', target):
            if match[1] in endpoints:
                item = dict(endpoints[match[1]], pid=pid, fd=int(descriptor.name))
                if item['uid'] != os.getuid():
                    raise ValueError('owned socket UID mismatch')
                result.append(item)
    if not result:
        raise ValueError('owned PID has no loopback5559 socket; cannot attach scoped observer')
    return result


class Probe:
    def __init__(self, output: pathlib.Path, sdk: pathlib.Path):
        self.output = output
        self.sdk = sdk
        self.private = pathlib.Path(os.environ['RUNNER_TEMP']) / 'android-sdk-probe-private'
        self.private.mkdir(mode=0o700)
        self.output.mkdir(mode=0o700)
        for name in ['home', 'tmp', 'avd', 'user']:
            (self.private / name).mkdir(mode=0o700)
        inherited = {key: value for key, value in os.environ.items() if not key.startswith('ADB_') and key not in
                     ['ADBHOST', 'ANDROID_ADB_SERVER_ADDRESS', 'ANDROID_ADB_SERVER_PORT', 'ANDROID_SERIAL',
                      'ANDROID_SDK_HOME', 'ANDROID_EMULATOR_HOME']}
        self.environment = dict(inherited, HOME=str(self.private / 'home'), TMPDIR=str(self.private / 'tmp'),
                                ANDROID_AVD_HOME=str(self.private / 'avd'), ANDROID_USER_HOME=str(self.private / 'user'),
                                ADB_TRACE='', ADB_SERVER_SOCKET='tcp:127.0.0.1:5037',
                                ADB_EMU='0', ADB_USB='0', ADB_MDNS='0', ADB_REJECT_KILL_SERVER='1')
        self.actors = []
        self.readers = []
        self.errors = []
        self.observer_counts = {}
        self.observer_pids = {}
        self.server = None
        self.adb = str(sdk / 'platform-tools/adb')

    def start(self, name: str, command: list[str], **kwargs) -> subprocess.Popen:
        process = subprocess.Popen(command, env=self.environment, start_new_session=True, **kwargs)
        self.actors.append((name, process))
        self.save(f'{name}.pid', str(process.pid))
        return process

    def save(self, name: str, value: str) -> None:
        (self.output / name).write_text(value + '\n')

    def run(self, name: str, command: list[str], input_bytes: bytes | None = None,
            timeout: int = COMMAND_TIMEOUT) -> bytes:
        process = self.start(name, command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            stdout, stderr = process.communicate(input_bytes, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self.stop(name, process)
            (self.output / f'{name}.stdout').write_bytes(error.output or b'')
            (self.output / f'{name}.stderr').write_bytes(error.stderr or b'')
            raise
        self.save(f'{name}.exit', str(process.returncode))
        (self.output / f'{name}.stdout').write_bytes(stdout)
        (self.output / f'{name}.stderr').write_bytes(stderr)
        if process.returncode:
            raise RuntimeError(f'{name} exited {process.returncode}')
        return stdout

    def device(self, name: str, *arguments: str, **kwargs) -> bytes:
        if self.server is None or self.server.poll() is not None:
            raise RuntimeError('owned daemon absent; do not auto-start a replacement')
        return self.run(name, [self.adb, '-s', SERIAL, *arguments], **kwargs)

    def stop(self, name: str, process: subprocess.Popen) -> None:
        if process.poll() is None:
            # Diagnostic observers run as root; signal only their published exec PID, never shared adb.
            if name.startswith(('tcp-', 'strace-')):
                if name not in self.observer_pids:
                    # Startup failed before the exec-PID event; terminate the owned sudo wrapper.
                    process.terminate()
                else:
                    subprocess.run(['sudo', '-n', 'kill', '-INT', '--', str(self.observer_pids[name])], check=True)
            else:
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=READY_TIMEOUT)
            except subprocess.TimeoutExpired:
                if name.startswith(('tcp-', 'strace-')):
                    subprocess.run(['sudo', '-n', 'kill', '-KILL', '--', str(self.observer_pids[name])], check=True)
                else:
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        self.save(f'{name}.exit', str(process.returncode))
        if not name.startswith(('tcp-', 'strace-')):
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None and not stream.closed:
                    stream.close()

    def observe(self, name: str, command: list[str], parser, readiness) -> subprocess.Popen:
        ready = threading.Event()
        # Root wrapper publishes the eventual tool exec PID, not merely sudo's monitor PID.
        command = command[:2] + ['bash', '-c', 'printf "SDK_PROBE_EXEC_PID=%s\\n" "$$" >&2; exec "$@"', '_'] + command[2:]
        process = self.start(name, command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        counts = {'accepted': 0, 'discarded': 0}
        self.observer_counts[name] = counts

        def read(stream, channel):
            closed = set()
            with (self.output / f'{name}.{channel}.jsonl').open('w') as output:
                for line in stream:
                    if match := re.fullmatch(r'SDK_PROBE_EXEC_PID=(\d+)\n', line):
                        self.observer_pids[name] = int(match[1])
                        self.save(f'{name}.exec-pid', match[1])
                    if readiness(line):
                        ready.set()
                    row = parser(line.rstrip('\n'))
                    if row and row.get('fd') not in closed:
                        output.write(json.dumps(row) + '\n')
                        if 'statistic' not in row:
                            counts['accepted'] += 1
                        if row.get('call') == 'close' and row['result'] == 0:
                            closed.add(row['fd'])
                    else:
                        counts['discarded'] += 1
            stream.close()

        for channel, stream in [('stdout', process.stdout), ('stderr', process.stderr)]:
            thread = threading.Thread(target=read, args=(stream, channel))
            thread.start()
            self.readers.append(thread)
        if not ready.wait(READY_TIMEOUT) or process.poll() is not None:
            raise RuntimeError(f'{name} observer not ready')
        return process

    def setup(self) -> tuple[subprocess.Popen, subprocess.Popen]:
        adb_version = self.run('adb-version', [self.adb, 'version']).decode()
        emulator = str(self.sdk / 'emulator/emulator')
        emulator_version = self.run('emulator-version', [emulator, '-version']).decode()
        for tool in ['tcpdump', 'strace']:
            self.run(f'{tool}-help', [tool, '--help'])
            self.run(f'{tool}-version', [tool, '--version'])
        image_path = self.sdk / 'system-images/android-35/google_apis/x86_64/source.properties'
        emulator_path = self.sdk / 'emulator/source.properties'
        self.save('image-source.properties', image_path.read_text())
        self.save('emulator-source.properties', emulator_path.read_text())
        self.save('input-sha256.json', json.dumps({str(path.relative_to(self.sdk)): hashlib.sha256(path.read_bytes()).hexdigest()
                  for path in [pathlib.Path(self.adb), pathlib.Path(emulator), image_path, emulator_path]}))
        verify_inputs(adb_version, emulator_version, image_path.read_text(), emulator_path.read_text())
        self.save('owned-environment.json', json.dumps({key: self.environment[key] for key in
                  ['ADB_TRACE', 'ADB_SERVER_SOCKET', 'ADB_EMU', 'ADB_USB', 'ADB_MDNS', 'ADB_REJECT_KILL_SERVER']}))
        # Explicit IPv4 client socket is treated as remote by adb: it cannot auto-start a replacement.
        # Discovery is disabled. The owned emulator's host:emulator:5559 notification registers5558.
        self.run('create-avd', ['avdmanager', 'create', 'avd', '--name', 'sdk-probe-35', '--path',
                              str(self.private / 'avd/sdk-probe-35.avd'), '--package',
                              'system-images;android-35;google_apis;x86_64'], b'no\n')
        read_fd, write_fd = os.pipe()
        ready = threading.Event()
        acknowledgment = []
        try:
            # SDK's own ACK pipe makes this foreground-owned child reapable, with no global kill-server.
            adb = self.start('adb-server', [self.adb, '-L', 'tcp:localhost:5037', 'fork-server', 'server',
                                           '--reply-fd', str(write_fd)], pass_fds=(write_fd,),
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        finally:
            os.close(write_fd)

        def read_ack():
            try:
                acknowledgment.append(os.read(read_fd, 3))
            finally:
                os.close(read_fd)
                ready.set()

        thread = threading.Thread(target=read_ack)
        thread.start()
        self.readers.append(thread)
        if not ready.wait(READY_TIMEOUT) or acknowledgment != [b'OK\n'] or adb.poll() is not None:
            raise RuntimeError('owned ADB startup ACK failed')
        self.save('adb-startup-ack.txt', 'OK')
        self.server = adb
        emu = self.start('emulator', [emulator, '-avd', 'sdk-probe-35', '-no-window', '-no-audio', '-no-snapshot',
                                     '-no-boot-anim', '-gpu', 'swiftshader', '-port', '5558', '-cores', '2', '-memory', '2560'],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        barrier = 'adb -s "$1" wait-for-device && exec adb -s "$1" logcat -b system -v brief -s ActivityManager:D UserController:D -e "Sending BOOT_COMPLETE user #0|Finished processing BOOT_COMPLETED for u0" -m 1'
        self.environment['PATH'] = str(self.sdk / 'platform-tools') + ':' + self.environment['PATH']
        self.run('boot-event', ['timeout', '600', 'bash', '-c', barrier, '_', SERIAL], timeout=600 + READY_TIMEOUT)
        if self.device('boot-property', 'shell', 'getprop', 'sys.boot_completed').strip() != b'1':
            raise RuntimeError('boot property not1')
        if b'package:/system/framework/framework-res.apk' not in self.device('package-manager', 'shell', 'pm', 'path', 'android'):
            raise RuntimeError('PackageManager not ready')
        return adb, emu

    def owned_executables(self, adb: subprocess.Popen, emulator: subprocess.Popen) -> dict[str, object]:
        executables = {}
        for label, process in [('adb', adb), ('qemu', emulator)]:
            path = pathlib.Path(os.readlink(f'/proc/{process.pid}/exe'))
            expected_path = pathlib.Path(self.adb) if label == 'adb' else self.sdk / 'emulator/qemu/linux-x86_64/qemu-system-x86_64-headless'
            if path.resolve() != expected_path.resolve():
                raise ValueError('owned SDK exec PID/image mismatch')
            executables[label] = {'pid': process.pid, 'executable': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
        return executables

    def boundary(self) -> None:
        # liblog's payload limit alone does not prove the CLI accepts this argv length.
        tag = 'SDKBOUNDARY'  # Same11-byte tag length as the two-case workload.
        expected = messages(tag, message_limit(tag))
        producer = f"set -eu\nlog -p i -t {tag} '{expected[0]}'\n"
        self.save('boundary-producer.sh', producer)
        self.device('boundary-buffer', 'logcat', '-b', 'main', '-G', f'{buffer_mib()}M')
        self.device('boundary-clear', 'logcat', '-b', 'main', '-c')
        self.device('boundary-producer', 'shell', 'sh', input_bytes=producer.encode())
        output = self.device('boundary-capture', 'logcat', '-b', 'main', '-d', '-v', 'raw', '-s', f'{tag}:I', '*:S')
        try:
            result = validate_capture(output, expected)
        except ValueError as error:
            self.save('boundary-result.json', json.dumps({'verified': False, 'reason': 'CLI maximum-length public record readback mismatch'}))
            raise ValueError('public CLI boundary preflight failed; no concurrency cases started') from error
        self.save('boundary-result.json', json.dumps(dict(result, verified=True, concurrency_case=False,
            source_sha256=hashlib.sha256(producer.encode()).hexdigest())))

    def collect_captures(self, clients: list[tuple[str, subprocess.Popen, int]], expected: list[list[str]]) -> list[dict[str, object]]:
        # Each worker owns one communicate(), which drains that client's stdout AND stderr.
        # All workers rendezvous before any worker releases a capture gate.
        barrier = threading.Barrier(len(clients) + 1)

        def collect(index):
            barrier.wait(READY_TIMEOUT)
            return self._collect_capture(*clients[index], expected[index])

        with concurrent.futures.ThreadPoolExecutor(max_workers=len(clients)) as executor:
            try:
                futures = [executor.submit(collect, index) for index in range(len(clients))]
                barrier.wait(READY_TIMEOUT)
                outcomes = []
                for (name, process, _), future in zip(clients, futures):
                    try:
                        outcomes.append(future.result())
                    except Cancellation:
                        raise
                    except Exception as error:
                        # Worker errors/cancelled futures must not erase the peer or act as operator signals.
                        outcome = {'pid': process.pid, 'verified': False, 'collector_error': type(error).__name__}
                        self.save(f'{name}-result.json', json.dumps(outcome))
                        outcomes.append(outcome)
                return outcomes
            except Cancellation:
                # Kill owned exec capturers BEFORE executor exit joins workers. Popen.kill()
                # uses a nonblocking poll, even if a worker holds its waitpid mutex. Do not
                # concurrently stop()/wait()/close streams; workers own drainage/reaping.
                for name, process, _ in clients:
                    try:
                        process.kill()
                    except OSError as error:
                        self.errors.append(f'{name}: cancellation kill {type(error).__name__}')
                barrier.abort()
                raise

    def _collect_capture(self, name: str, process: subprocess.Popen, started: int, expected: list[str]) -> dict[str, object]:
        try:
            output, error = process.communicate(input=b'capture\n', timeout=COMMAND_TIMEOUT)
        except subprocess.TimeoutExpired as error:
            self.stop(name, process)
            partial = error.output or b''
            (self.output / f'{name}.stdout').write_bytes(partial)
            (self.output / f'{name}.stderr').write_bytes(error.stderr or b'')
            outcome = {'pid': process.pid, 'timeout': True, 'exit': process.returncode, 'verified': False,
                'wire_output_bytes': len(partial), 'wire_output_sha256': hashlib.sha256(partial).hexdigest()}
        else:
            self.save(f'{name}.exit', str(process.returncode))
            (self.output / f'{name}.stdout').write_bytes(output)
            (self.output / f'{name}.stderr').write_bytes(error)
            outcome = {'pid': process.pid, 'started_ns': started, 'collected_ns': time.time_ns(),
                       'exit': process.returncode, 'verified': False,
                       'wire_output_bytes': len(output), 'wire_output_sha256': hashlib.sha256(output).hexdigest()}
            try:
                outcome.update(validate_capture(output, expected))
                outcome['verified'] = process.returncode == 0
            except ValueError:
                outcome['record_error'] = True
        self.save(f'{name}-result.json', json.dumps(outcome))
        return outcome

    def case(self, count: int, adb: subprocess.Popen, emulator: subprocess.Popen) -> None:
        name = f'case-{count}'
        tags = [f'SDKPROBE{count}_{index}' for index in range(count)]
        expected = [messages(tag, budget) for tag, budget in zip(tags, BUDGETS)]
        # Twofold empirical headroom; logger_entry is28 bytes plus priority/tag/message NULs.
        buffer_size = buffer_mib()
        self.device(f'{name}-buffer', 'logcat', '-b', 'main', '-G', f'{buffer_size}M')
        self.device(f'{name}-clear', 'logcat', '-b', 'main', '-c')
        producer = 'set -eu\n' + ''.join(f"log -p i -t {tag} '{record}'\n" for tag, records in zip(tags, expected) for record in records)
        self.save(f'{name}-producer.sh', producer)
        self.save(f'{name}-source.json', json.dumps({'budgets': BUDGETS[:count], 'liblog_max_payload': MAX_PAYLOAD,
                   'cli_max_message': CLI_MAX_MESSAGE, 'message_limit': [message_limit(tag) for tag in tags],
                   'buffer_mib': buffer_size, 'message_records': [len(records) for records in expected],
                   'source_sha256': hashlib.sha256(producer.encode()).hexdigest(), 'interpretation': 'finite prepopulated drain; not subscription timing'}))
        self.device(f'{name}-producer', 'shell', 'sh', input_bytes=producer.encode())
        sockets = [socket_inventory(process.pid) for process in [adb, emulator]]
        self.save(f'{name}-sockets.json', json.dumps(sockets))
        self.save(f'{name}-owned-executables.json', json.dumps(self.owned_executables(adb, emulator)))
        host = time.time_ns()
        guest = self.device(f'{name}-guest-clock', 'shell', 'date', '+%s.%N').decode().strip()
        if not re.fullmatch(r'\d+\.\d+', guest):
            raise ValueError('guest epoch clock marker unavailable')
        self.save(f'{name}-clocks.json', json.dumps({'host_before_ns': host, 'guest_epoch': guest, 'host_after_ns': time.time_ns(), 'aligned': False}))
        first_actor = len(self.actors)
        first_reader = len(self.readers)
        observers = []
        try:
            observers.append((f'tcp-{name}', self.observe(f'tcp-{name}', ['sudo', '-n', 'tcpdump', '-i', 'lo', '-nn', '-tt', '-S', '-l',
                                                                     'tcp port 5559 and host 127.0.0.1'], tcp_fields,
                                                     lambda line: 'listening on lo' in line)))
            # Do not ptrace QEMU/vCPU threads: printing filters do not avoid syscall stops.
            # QEMU remains endpoint/image validated, but its syscall errno is unobserved.
            descriptors = {item['fd'] for item in sockets[0]}
            observer_name = f'strace-{name}-{adb.pid}'
            observers.append((observer_name, self.observe(observer_name, ['sudo', '-n', 'strace', '-f', '-ttt', '-T', '-o', '/dev/stdout',
                '-e', f'trace={CALLS}', '-e', 'raw=all', '-e', 'status=successful,failed',
                '-e', 'trace-fds=' + ','.join(str(fd) for fd in sorted(descriptors)), '-p', str(adb.pid)],
                lambda line: syscall_fields(line, descriptors),
                lambda line: f'Process {adb.pid} attached' in line)))
            clients = []
            # Launch every owned wrapper before releasing either capture, without a sleep.
            for index, tag in enumerate(tags):
                client_name = f'{name}-capture-{index}'
                started = time.time_ns()
                process = self.start(client_name, ['bash', '-c', 'printf "SDK_PROBE_CAPTURE_READY\\n"; read -r go; exec "$@"', '_', self.adb, '-s', SERIAL,
                    'logcat', '-b', 'main', '-d', '-v', 'raw', '-s', f'{tag}:I', '*:S'],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                clients.append((client_name, process, started))
            for client_name, process, _ in clients:
                ready = threading.Event()
                acknowledgment = []

                def read_ready(stream=process.stdout, ack=acknowledgment, event=ready):
                    ack.append(stream.readline())
                    event.set()

                thread = threading.Thread(target=read_ready)
                thread.start()
                self.readers.append(thread)
                if not ready.wait(READY_TIMEOUT) or acknowledgment != [b'SDK_PROBE_CAPTURE_READY\n']:
                    raise RuntimeError('capture wrapper event missing')
                self.save(f'{client_name}.ready', 'SDK_PROBE_CAPTURE_READY')
            outcomes = self.collect_captures(clients, expected)
            verified = all(outcome['verified'] for outcome in outcomes)
            self.save(f'{name}-result.json', json.dumps({'clients': outcomes, 'traffic_verified': verified,
                'concurrency_dispatched': count == 2, 'application_acceptance': False, 'causal_fix': False}))
            ack = self.device(f'{name}-ack', 'shell', 'printf', f'{name}-complete')
            if ack != f'{name}-complete'.encode():
                raise RuntimeError('separate completion acknowledgment missing')
            if not verified:
                raise ValueError('one or more clients failed literal public record/byte validation')
        finally:
            cancellation = isinstance(sys.exception(), Cancellation)
            for actor_name, process in reversed(self.actors[first_actor:]):
                try:
                    self.stop(actor_name, process)
                except Exception as error:
                    self.errors.append(f'{actor_name}: {type(error).__name__}')
            for thread in self.readers[first_reader:]:
                thread.join(READY_TIMEOUT)
                if thread.is_alive():
                    self.errors.append('case observer reader not retired')
            if self.errors and not cancellation:
                raise RuntimeError('case actors not retired; stop experiment')
            for observer_name, _ in observers:
                counts = self.observer_counts[observer_name]
                self.save(f'{observer_name}-counts.json', json.dumps(counts))
                if counts['accepted'] == 0 and not cancellation:
                    raise RuntimeError(f'{observer_name} produced no validated endpoint/syscall evidence')

    def postmortem(self, count: int, adb: subprocess.Popen, emulator: subprocess.Popen) -> None:
        # Do not archive arbitrary guest/kernel text. Retain only owned lifecycle/OOM rows.
        commands = [
            ('adbd-lifecycle', [self.adb, '-s', SERIAL, 'logcat', '-b', 'all', '-d', '-v', 'threadtime', '-s', 'adbd:I', '*:S']),
            ('kernel-oom', ['sudo', '-n', 'dmesg', '--time-format', 'iso']),
        ]
        for name, command in commands:
            if name == 'adbd-lifecycle' and adb.poll() is not None:
                self.save(f'case-{count}-{name}.unavailable', 'owned ADB exited; no replacement startup')
                continue
            name = f'case-{count}-{name}'
            process = self.start(name, command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                output, _ = process.communicate(timeout=READY_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.stop(name, process)
                self.save(f'{name}.unavailable', 'empirical snapshot deadline; not a case completion or retry')
                continue
            self.save(f'{name}.exit', str(process.returncode))
            rows = []
            for line in output.decode(errors='replace').splitlines():
                if name.endswith('adbd-lifecycle'):
                    if match := re.fullmatch(r'(\d\d-\d\d \d\d:\d\d:\d\d\.\d+)\s+(\d+)\s+(\d+) [IDWEF] adbd\s*: '
                        r'host-(\d+): (read thread spawning|write thread spawning|already offline|offline|read failed: [A-Za-z ]+|connection terminated: (?:read|write) failed)', line):
                        rows.append({'timestamp': match[1], 'pid': int(match[2]), 'tid': int(match[3]), 'host': int(match[4]), 'event': match[5]})
                elif match := re.search(r'Killed process (\d+) \(([^)]+)\)', line):
                    if int(match[1]) in (adb.pid, emulator.pid):
                        stamp = re.match(r'(\d{4}-\d\d-\d\dT[0-9:.+\-]+)', line)
                        rows.append({'timestamp': stamp[1] if stamp else None, 'pid': int(match[1]), 'event': 'kernel killed owned process'})
            self.save(f'{name}.json', json.dumps(rows))

    def close(self) -> None:
        for name, process in reversed(self.actors):
            try:
                self.stop(name, process)
            except Exception as error:
                self.errors.append(f'{name}: {type(error).__name__}')
        for thread in self.readers:
            thread.join(READY_TIMEOUT)
            if thread.is_alive():
                self.errors.append('reader not retired')
        self.save('retirement.json', json.dumps({'errors': self.errors, 'actors': len(self.actors)}))
        if self.errors:
            raise RuntimeError('owned actor retirement incomplete')


def main() -> None:
    if (os.environ.get('GITHUB_ACTIONS') != 'true' or os.environ.get('GITHUB_EVENT_NAME') != 'workflow_dispatch'
            or os.environ.get('GITHUB_REPOSITORY') != 'JafarAbdi/wezterm'):
        raise RuntimeError('SDK probe is restricted to an authorized ephemeral forge VM')
    output = pathlib.Path(os.environ['RUNNER_TEMP']) / 'android-sdk-probe-public'
    probe = Probe(output, pathlib.Path(os.environ['ANDROID_HOME']))
    probe.save('source-identity.json', json.dumps({'repository': os.environ['GITHUB_REPOSITORY'],
               'sha': os.environ['GITHUB_SHA'], 'event': os.environ['GITHUB_EVENT_NAME'],
               'probe_sha256': hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
               'scope': 'SDK-only; no APK/native build or product acceptance'}))

    def interrupted(signum, frame):
        # First signal is terminal. Repeated signals must not interrupt owned retirement.
        for pending in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(pending, signal.SIG_IGN)
        raise Cancellation(signum)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    try:
        adb, emulator = probe.setup()
        probe.boundary()  # One public CLI-length preflight, not a third concurrency case.
        # Each case attempted once; a failed first case does not omit the second or become a retry.
        failures = []
        for count in (1, 2):
            try:
                probe.case(count, adb, emulator)
            except InterruptedError:
                raise
            except Exception as error:
                failures.append(f'case-{count}: {type(error).__name__}: {error}')
                if probe.errors:
                    raise RuntimeError('cannot start another case with unretired actors') from error
            # Preserve case1 lifecycle before case2 clears the owned main buffer.
            probe.postmortem(count, adb, emulator)
        probe.save('summary.json', json.dumps({'failures': failures, 'product_green': False, 'causal_fix': False}))
        if failures:
            raise RuntimeError('SDK experiment case failure; inspect literal receipts')
    except Cancellation as error:
        raise SystemExit(128 + error.signum) from error
    finally:
        probe.close()


if __name__ == '__main__':
    main()
