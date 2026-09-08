#!/usr/bin/env python3
"""Measure read-only TUI operations. Save timings, not cluster object data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import statistics
import subprocess
import tempfile
import time


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout


def stats(values):
    values = sorted(values)
    rank = (len(values) - 1) * 0.95
    lo = int(rank)
    return dict(n=len(values), median=statistics.median(values),
                minimum=values[0], maximum=values[-1],
                p95=values[lo] + (values[min(lo + 1, len(values) - 1)] - values[lo]) * (rank - lo))


def measure_commands(binaries):
    result = {}
    for operation in ['version', 'help']:
        samples = {name: [] for name in binaries}
        commands = {name: [binary] + (['version', '--short'] if name == 'k9s' and operation == 'version' else ['--' + operation]) for name, binary in binaries.items()}
        for command in commands.values():
            subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for trial in range(100):
            for name in (['sofka', 'k9s'] if trial % 2 == 0 else ['k9s', 'sofka']):
                start = time.perf_counter()
                subprocess.run(commands[name], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                samples[name].append((time.perf_counter() - start) * 1000)
        result[operation] = samples
    return result


def summarize(result):
    result['summary'] = {}
    for name in result['binaries']:
        rows = [r for r in result['runs'] if r['program'] == name]
        result['summary'][name] = {key: stats([r[key] for r in rows if key in r])
            for key in ['startup_ms', 'filter_ms', 'clear_ms', 'statefulsets_ms'] if any(key in r for r in rows)}
        memory = [statistics.median(r['rss_mib']) for r in rows if r.get('rss_mib')]
        if memory:
            result['summary'][name]['rss_mib'] = stats(memory)
        for operation, samples in result['commands'].items():
            result['summary'][name][operation + '_ms'] = stats(samples[name])


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--context', required=True)
    ap.add_argument('--sofka', default='target/release/sofka')
    ap.add_argument('--k9s', default=shutil.which('k9s'))
    ap.add_argument('--pairs', type=int, default=10)
    ap.add_argument('--output', required=True)
    ap.add_argument('--finish-commands', action='store_true',
                    help='Keep saved TUI trials and repeat only command measurements')
    args = ap.parse_args()
    if args.pairs < 1:
        ap.error('--pairs must be positive')
    binaries = {'sofka': str(Path(args.sofka).resolve()), 'k9s': str(Path(args.k9s).resolve())}
    kubectl = ['kubectl', '--context', args.context, '--request-timeout=30s']
    def objects(kind):
        return json.loads(run(kubectl + ['get', kind, '-A', '-o', 'json']))['items']
    if args.finish_commands:
        output = Path(args.output)
        result = json.loads(output.read_text())
        for name, binary in binaries.items():
            digest = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
            if digest != result['binaries'][name]['sha256']:
                raise ValueError('Build differs from saved TUI trials')
        result['commands'] = measure_commands(binaries)
        result['pods_after'] = len(objects('pods'))
        result['commands_repeated_after_cleanup_error'] = True
        summarize(result)
        output.write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result['summary'], indent=2))
        return
    pods = objects('pods')
    # Use a running pod with a unique name. Do not store its name in the result.
    names = [p['metadata']['name'] for p in pods]
    selected = next(p['metadata']['name'] for p in pods
                    if p.get('status', {}).get('phase') == 'Running'
                    and sum(p['metadata']['name'] in n for n in names) == 1)
    threshold = int(len(pods) * 0.95)
    sts_count = len(objects('statefulsets'))
    if threshold < 20 or sts_count == 0:
        raise RuntimeError('This test needs at least 22 pods and one StatefulSet')
    result = {'date_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'platform': platform.platform(),
              'commit': run(['git', 'rev-parse', 'HEAD']).strip(),
              'rust': run(['rustc', '--version']).strip(),
              'tmux': run(['tmux', '-V']).strip(),
              'pods_before': len(pods), 'pod_threshold': threshold,
              'statefulsets_before': sts_count, 'runs': [], 'commands': {}, 'binaries': {}}
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        output.write_text(json.dumps(result, indent=2) + '\n')
    for name, binary in binaries.items():
        data = Path(binary).read_bytes()
        entry = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
        # Nix launchers are scripts. Report the executable separately.
        if data.startswith(b'#!'):
            match = re.search(r'exec -a "\$0" "([^"]+)"', data.decode())
            if not match:
                raise RuntimeError('Unknown launcher: specify the executable with --k9s')
            executable = Path(match[1]).read_bytes()
            entry.update(executable_bytes=len(executable), executable_sha256=hashlib.sha256(executable).hexdigest())
        entry['version'] = run([binary] + (['--version'] if name == 'sofka' else ['version', '--short'])).strip()
        result['binaries'][name] = entry
    socket = 'sofka-benchmark-' + str(os.getpid())
    def tmux(*parts):
        return run(['tmux', '-L', socket, '-f', '/dev/null', *parts])
    def screen():
        return tmux('capture-pane', '-p', '-t', 'bench:0.0')
    def count(text, name, kind='pods'):
        pattern = (rf'\b{kind}\s+\[([\d,]+)' if name == 'sofka'
                   else rf'\b{kind}\(all\)\[([\d,]+)')
        found = re.search(pattern, text, re.I)
        return int(found[1].replace(',', '')) if found else -1
    def wait_for(predicate, start, timeout=60):
        while time.perf_counter() - start < timeout:
            text = screen()
            if predicate(text):
                return (time.perf_counter() - start) * 1000
            time.sleep(0.005)
        raise TimeoutError('Screen condition not reached')
    with tempfile.TemporaryDirectory(prefix='sofka-benchmark-') as root:
        base_env = {k: v for k, v in os.environ.items()
                    if not k.startswith(('SOFKA_', 'K9S_'))}
        base_env['TERM'] = 'xterm-256color'
        try:
            tmux('new-session', '-d', '-s', 'bench', '-x', '180', '-y', '50', 'sleep 3600')
            costs = []
            for _ in range(50):
                start = time.perf_counter()
                screen()
                costs.append((time.perf_counter() - start) * 1000)
            result['capture_ms'] = costs
            for pair in range(args.pairs):
                order = ['sofka', 'k9s'] if pair % 2 == 0 else ['k9s', 'sofka']
                for name in order:
                    env = base_env.copy()
                    for key in ['XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME']:
                        env[key] = str(Path(root) / f'{pair}-{name}' / key)
                        Path(env[key]).mkdir(parents=True)
                    command = [binaries[name], '--context', args.context, '--readonly', '-A']
                    command += ['pods'] if name == 'sofka' else ['-c', 'pods', '--splashless', '--logoless']
                    row = {'pair': pair + 1, 'program': name}
                    result['runs'].append(row)
                    start = time.perf_counter()
                    # env -i prevents personal program overrides from entering the child.
                    launch = ['env', '-i'] + [f'{k}={v}' for k, v in env.items()] + command
                    tmux('respawn-pane', '-k', '-t', 'bench:0.0', 'exec ' + shlex.join(launch))
                    try:
                        row['startup_ms'] = wait_for(lambda s: count(s, name) >= threshold
                            and len(re.findall(r'\b(?:Running|Succeeded|Completed|Pending)\b', s)) >= 20, start)
                        row['visible_pods'] = count(screen(), name)
                        # Sample each new process at the same time after readiness.
                        time.sleep(5)
                        pid = tmux('display-message', '-p', '-t', 'bench:0.0', '#{pane_pid}').strip()
                        row['rss_mib'] = []
                        for _ in range(5):
                            row['rss_mib'].append(float(run(['/bin/ps', '-o', 'rss=', '-p', pid])) / 1024)
                            time.sleep(0.25)
                        # Time the whole filter input, since a live filter can finish before Enter.
                        start = time.perf_counter()
                        tmux('send-keys', '-t', 'bench:0.0', '/')
                        tmux('send-keys', '-t', 'bench:0.0', '-l', selected)
                        tmux('send-keys', '-t', 'bench:0.0', 'Enter')
                        row['filter_ms'] = wait_for(lambda s: count(s, name) == 1
                            and any(selected in line and re.search(r'\bRunning\b', line) for line in s.splitlines()), start)
                        start = time.perf_counter()
                        tmux('send-keys', '-t', 'bench:0.0', 'Escape')
                        row['clear_ms'] = wait_for(lambda s: count(s, name) >= threshold, start)
                        # First resource open, timed from the initial colon.
                        start = time.perf_counter()
                        tmux('send-keys', '-t', 'bench:0.0', ':')
                        tmux('send-keys', '-t', 'bench:0.0', '-l', 'statefulsets')
                        tmux('send-keys', '-t', 'bench:0.0', 'Enter')
                        row['statefulsets_ms'] = wait_for(lambda s: count(s, name, 'statefulsets') >= sts_count, start)
                    except (TimeoutError, subprocess.CalledProcessError) as exc:
                        row['error'] = type(exc).__name__
                    finally:
                        tmux('send-keys', '-t', 'bench:0.0', 'C-c')
                        save()
                    print(json.dumps(row), flush=True)
            result['commands'] = measure_commands(binaries)
            result['pods_after'] = len(objects('pods'))
        finally:
            subprocess.run(['tmux', '-L', socket, 'kill-server'],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            save()
    summarize(result)
    save()
    print(json.dumps(result['summary'], indent=2))


if __name__ == '__main__':
    main()
