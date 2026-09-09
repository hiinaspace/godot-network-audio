#!/usr/bin/env python3
"""Control isolated tests from cc-0; all workload and analysis run on test pods."""
import argparse
import concurrent.futures
import json
import pathlib
import shlex
import subprocess
import time

ROOT = '/work/projects/godot-network-audio'
SSH = ['ssh', '-o', 'BatchMode=yes', '-o', 'ServerAliveInterval=10', '-o', 'ServerAliveCountMax=3',
       '-i', str(pathlib.Path.home() / '.ssh/gna-sim'), '-o', 'IdentitiesOnly=yes']


def remote(host, command, **kwargs):
    return subprocess.run(SSH + ['claude@' + host, command], check=True, text=True,
                          capture_output=True, **kwargs).stdout


def clock_bounds(host):
    samples = []
    command = shlex.join(['python3', '-u', '-c',
                          'import sys,time\nfor line in sys.stdin: print(time.time_ns() // 1000, flush=True)'])
    process = subprocess.Popen(SSH + ['claude@' + host, command], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, text=True)
    try:
        for _ in range(8):
            start = time.time_ns() // 1000
            process.stdin.write('sample\n')
            process.stdin.flush()
            value = int(process.stdout.readline())
            end = time.time_ns() // 1000
            samples.append({'low': value - end, 'high': value - start})
    finally:
        process.stdin.close()
        process.wait(timeout=10)
    return {'low': max(x['low'] for x in samples), 'high': min(x['high'] for x in samples),
            'samples': samples}


def run(index, fixed, spatial, trace=False, norewinds=False, cycles=1):
    label = f'{"fixed" if fixed else "churn"}-{spatial}d-{time.time_ns()}-{index}'
    directory = '/tmp/gna-isolated-' + label
    durable = ROOT + '/target/godot-gate/isolated/' + label
    peers = 7 if fixed else 31
    command = shlex.join(['python3', ROOT + '/scripts/godot_isolated_worker.py', 'receiver',
                          directory, str(peers), '7', str(int(fixed)), str(int(spatial == 3)),
                          *(['--trace'] if trace else []),
                          *(['--norewinds'] if norewinds else [])])
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        before = dict(zip(('receiver', 'loadgen'), pool.map(clock_bounds, ('gna-sim', 'gna-loadgen'))))
    receiver = subprocess.Popen(SSH + ['claude@gna-sim', command], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
    try:
        endpoint = None
        for _ in range(100):
            if receiver.poll() is not None:
                raise RuntimeError(receiver.communicate())
            endpoint = remote('gna-sim', f'test ! -s {directory}/receiver_endpoint.json || cat {directory}/receiver_endpoint.json')
            if endpoint:
                break
            time.sleep(.1)
        if not endpoint:
            raise RuntimeError('receiver endpoint unavailable')
        # Keep the receiver alive while fresh fleets repeatedly join and leave.
        # This distinguishes bounded cleanup from process-exit reclamation.
        artifacts = {'loadgen_events.jsonl': '', 'loadgen_events.jsonl.cadence.json': [],
                     'loadgen.json': '', 'manifest.json': []}
        for cycle in range(cycles):
            fleet_directory = directory + f'-fleet-{cycle}'
            command = shlex.join(['python3', ROOT + '/scripts/godot_isolated_worker.py', 'loadgen',
                                  fleet_directory, str(peers), '7', str(int(fixed)), str(int(spatial == 3))])
            remote('gna-loadgen', command, input=endpoint, timeout=110)
            for name in artifacts:
                content = remote('gna-loadgen', f'cat {fleet_directory}/{name}')
                if name.endswith('.cadence.json'):
                    artifacts[name].extend(json.loads(content))
                elif name == 'manifest.json':
                    artifacts[name].append(json.loads(content))
                else:
                    artifacts[name] += content
            # Preserve each worker's complete artifacts until the run is durable.
        # Give final disconnect callbacks and deferred deletion checks time to run.
        remote('gna-sim', 'sleep 0.1')
        remote('gna-sim', f'touch {directory}/done')
        stdout, stderr = receiver.communicate(timeout=110)
        if receiver.returncode:
            raise RuntimeError((stdout, stderr))
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            after = dict(zip(('receiver', 'loadgen'), pool.map(clock_bounds, ('gna-sim', 'gna-loadgen'))))
        # Transfer the small sender artifacts through the controller; audio and
        # full receiver traces never pass through cc-0.
        for name, content in artifacts.items():
            target = 'loadgen_manifest.json' if name == 'manifest.json' else name
            if not isinstance(content, str):
                content = json.dumps(content)
            remote('gna-sim', f'tee {directory}/{target} >/dev/null', input=content)
        remote('gna-sim', f'tee {directory}/clock_bounds.json >/dev/null',
               input=json.dumps({'before': before, 'after': after}, indent=2))
        # Cross-host wall subtraction has a measured bound; publish it alongside
        # sender cadence and receiver-local trace gaps, not as exact latency.
        summary = remote('gna-sim', f'python3 {ROOT}/scripts/summarize_godot_voice_churn.py {directory} {peers} 7 {17 * cycles}')
        data = json.loads(summary)
        data['fleet_cycles'] = cycles
        lo = min(v['loadgen']['low'] - v['receiver']['high'] for v in (before, after))
        hi = max(v['loadgen']['high'] - v['receiver']['low'] for v in (before, after))
        data['sender_minus_receiver_clock_offset_us_bounds'] = [lo, hi]
        data['cross_host_timing_note'] = 'Legacy first-output/disconnect/tail fields use unadjusted wall clocks; use corrected bounds or receiver-local timing.'
        data['first_output_max_ms_corrected_bounds'] = [data['first_output_latency_ms_max'] + lo / 1000,
                                                       data['first_output_latency_ms_max'] + hi / 1000]
        cadence = json.loads(remote('gna-sim', f'cat {directory}/loadgen_events.jsonl.cadence.json'))
        data['sender_deadline_lateness_us_max'] = max(cadence)
        data['sender_deadlines_over_20ms'] = sum(x > 20000 for x in cadence)
        checked_fields = ('receive_reclamation_failures', 'receive_reclamation_missing',
                          'final_pending_reclamation_checks', 'receiver_error_lines',
                          'queue_dropped_packets', 'missing_first_output', 'missing_scheduled_disconnect')
        data['validation_failures'] = [field for field in checked_fields if data.get(field, 0)]
        if data.get('receive_reclamation_checks') != data['receiver_disconnect_events']:
            data['validation_failures'].append('reclamation_count_mismatch')
        remote('gna-sim', f'tee {directory}/summary.json >/dev/null', input=json.dumps(data, indent=2))
        remote('gna-sim', f'mkdir -p {durable} && cp -a {directory}/. {durable}/')
        print(json.dumps({'run': label, 'result': data}), flush=True)
        cleanup = [('gna-sim', directory)] + [
            ('gna-loadgen', directory + f'-fleet-{cycle}') for cycle in range(cycles)
        ]
        for host, scratch in cleanup:
            # Exact, generated run directory; durable receiver copy already exists.
            remote(host, shlex.join(['python3', '-c', 'import shutil,sys; shutil.rmtree(sys.argv[1])', scratch]))
        if data['validation_failures']:
            raise RuntimeError(f"Isolated gate failed: {data['validation_failures']}; artifacts: {durable}")
    finally:
        # Worker timeouts also bound remote lifetime if control connectivity fails.
        subprocess.run(SSH + ['claude@gna-sim', f'test ! -d {directory} || touch {directory}/done'], capture_output=True)
        if receiver.poll() is None:
            try:
                receiver.communicate(timeout=110)
            except subprocess.TimeoutExpired:
                receiver.terminate()


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--repeats', type=int, default=1)
    parser.add_argument('--fixed', action='store_true')
    parser.add_argument('--spatial', type=int, choices=(2, 3), default=3)
    parser.add_argument('--trace', action='store_true', help='Trace receiver waits with strace')
    parser.add_argument('--norewinds', action='store_true', help='Use a PulseAudio null sink with 50 ms maximum latency and no rewinds')
    parser.add_argument('--cycles', type=int, choices=range(1, 4), default=1,
                        help='Fresh fleet cycles within one receiver lifetime (bounded by its 90 s timeout)')
    args = parser.parse_args()
    for index in range(args.repeats):
        run(index, args.fixed, args.spatial, args.trace, args.norewinds, args.cycles)
