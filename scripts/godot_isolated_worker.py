#!/usr/bin/env python3
"""One bounded receiver or loadgen worker; invoked over SSH by the controller."""
import hashlib
import json
import os
import pathlib
import signal
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path('/work/projects/godot-network-audio')


def snapshot():
    result = {'unix_usec': time.time_ns() // 1000, 'mono_usec': time.monotonic_ns() // 1000}
    for name in ('cpu.stat', 'cpu.pressure', 'memory.pressure', 'cpu.max'):
        result[name] = pathlib.Path('/sys/fs/cgroup', name).read_text()
    return result


def main():
    role, directory, peers, active, fixed, spatial = sys.argv[1:]
    out = pathlib.Path(directory)
    out.mkdir(parents=True, exist_ok=False)
    children = []
    handles = []
    module = None
    env = os.environ.copy()
    before = snapshot()
    manifest = {'role': role, 'host': socket.gethostname(), 'before': before,
                'peers': int(peers), 'active': int(active), 'fixed': fixed, 'spatial': spatial}

    def launch(command, log, extra=None):
        handle = (out / log).open('w')
        handles.append(handle)
        child = subprocess.Popen(command, cwd=ROOT, env=extra or env,
                                 stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
        children.append(child)
        return child

    def interrupted(*_):
        raise InterruptedError('worker interrupted')

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        if role == 'receiver':
            binary = pathlib.Path('/work/.tools/godot-4.7.2/Godot_v4.7.2-stable_linux.x86_64')
            version = subprocess.check_output([str(binary), '--version'], text=True).strip()
            if not version.startswith('4.7.2.'):
                raise RuntimeError(version)
            manifest.update(executable=str(binary), version=version,
                            sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
            extension = ROOT / 'example_iroh/addons/godot_network_audio/bin/godot_network_audio.so'
            manifest['extension_sha256'] = hashlib.sha256(extension.read_bytes()).hexdigest()
            sink = 'gna_isolated_' + str(os.getpid())
            module = subprocess.check_output(['pactl', 'load-module', 'module-null-sink',
                                             'sink_name=' + sink, 'rate=48000'], text=True).strip()
            manifest['pulse_info'] = subprocess.check_output(['pactl', 'info'], text=True)
            launch(['ffmpeg', '-hide_banner', '-loglevel', 'warning', '-nostdin', '-y',
                    '-f', 'pulse', '-i', sink + '.monitor', '-t', '90', '-ac', '2', '-ar', '48000',
                    str(out / 'mixed_output.wav')], 'output_capture.log')
            ip = socket.gethostbyname('gna-sim-peer')
            env.update(PULSE_SINK=sink, GNA_IROH_ROLE='receiver', GNA_IROH_BIND_ADDR=ip + ':42000',
                       GNA_IROH_ENDPOINT_INFO_PATH=str(out / 'receiver_endpoint.json'),
                       GNA_DEMO_OUTPUT_DEVICE=sink, GNA_DEMO_SPATIALIZE=spatial,
                       GNA_DEMO_PRINT_STATS='0', GNA_DEMO_QUIT_SECONDS='90',
                       GNA_DEMO_QUIT_FILE=str(out / 'done'),
                       GNA_DEMO_TRACE_JSONL=str(out / 'receiver_trace.jsonl'),
                       GNA_DEMO_EVENT_JSONL=str(out / 'receiver_events.jsonl'))
            process = launch(['/usr/bin/time', '-v', '-o', str(out / 'receiver_time.txt'), str(binary),
                              '--display-driver', 'headless', '--audio-driver', 'PulseAudio',
                              '--path', str(ROOT / 'example_iroh'), '--scene', 'res://main.tscn'],
                             'receiver.log')
            # /usr/bin/time owns the Godot child; sample the actual process.
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                ids = pathlib.Path(f'/proc/{process.pid}/task/{process.pid}/children').read_text().split()
                if ids:
                    launch(['python3', str(ROOT / 'scripts/sample_process_resources.py'), ids[0],
                            str(out / 'receiver_resources.jsonl')], 'sampler.log')
                    break
                time.sleep(.02)
            manifest['bind'] = ip + ':42000'
        else:
            endpoint = sys.stdin.read()
            (out / 'receiver_endpoint.json').write_text(endpoint)
            env.update(GNA_CHURN_BIND_IP=socket.gethostbyname('gna-loadgen-peer'),
                       GNA_CHURN_BASE_PORT='42001', GNA_CHURN_FIXED=fixed)
            binary = ROOT / 'target/release/godot_voice_churn'
            manifest.update(executable=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                            bind_ip=env['GNA_CHURN_BIND_IP'], relay_enabled=False)
            process = launch([str(binary), str(out / 'receiver_endpoint.json'),
                              str(out / 'loadgen_events.jsonl'), peers, active, '3'], 'loadgen.json')
        manifest['git_head'] = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
        manifest['git_status'] = subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True)
        (out / 'manifest.json').write_text(json.dumps(manifest, indent=2))
        code = process.wait(timeout=100)
        if code:
            raise RuntimeError(f'{role} exited {code}')
    finally:
        manifest['after'] = snapshot()
        for child in reversed(children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGINT)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        for handle in handles:
            handle.close()
        if module:
            subprocess.run(['pactl', 'unload-module', module], check=False)
        (out / 'manifest.json').write_text(json.dumps(manifest, indent=2))


if __name__ == '__main__':
    main()
