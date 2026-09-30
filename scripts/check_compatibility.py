"""Verify one built ABI3 wheel against the pinned, unmodified DRF matrices."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess
import tarfile
import tempfile
import time
import zipfile

REFERENCES = {
    '3.17.2': 'ad309f3e18aa5591db87a8fc959dc4564c000324',
    '3.18.1': 'b578eab1cad040414b758131af1e17aa000b51e2',
}
MATRIX = {
    '3.17.2': {'3.10': ['4.2', '5.1', '5.2'], '3.11': ['4.2', '5.1', '5.2'],
               '3.12': ['4.2', '5.1', '5.2', '6.0'], '3.13': ['5.1', '5.2', '6.0'],
               '3.14': ['5.2', '6.0']},
    '3.18.1': {'3.10': ['5.2'], '3.11': ['5.2'], '3.12': ['5.2', '6.0', '6.1'],
               '3.13': ['5.2', '6.0', '6.1'], '3.14': ['5.2', '6.0', '6.1'], '3.15': ['6.1']},
}
PROBE = '''
import hashlib,json,platform,sys,sysconfig
from pathlib import Path
import django,rest_framework,rustializer
from rustializer import _native
rustializer.activate()
assert sys.implementation.name == 'cpython'
assert not sysconfig.get_config_var('Py_GIL_DISABLED')
print(json.dumps({'python':platform.python_version(),'django':django.get_version(),
    'drf':rest_framework.VERSION,'reference':_native._reference_commit,
    'platform':platform.system(),'architecture':platform.machine(),
    'binary_sha256':hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest(),
    'extension':_native.__file__, 'drf_package':rest_framework.__file__}))
'''


def run(command, log, *, cwd=None, env=None):
    start = time.monotonic()
    with log.open('w') as output:
        result = subprocess.run([str(arg) for arg in command], cwd=cwd, env=env,
                                stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        tail = '\n'.join(log.read_text(errors='replace').splitlines()[-25:])[-6000:]
        raise RuntimeError(f'{command[0]} exited {result.returncode}; {log}\n{tail}')
    return time.monotonic() - start


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('wheel', type=Path)
    parser.add_argument('--checkout', type=Path)
    parser.add_argument('--python', nargs='+')
    parser.add_argument('--drf', nargs='+', choices=REFERENCES, default=list(REFERENCES))
    parser.add_argument('--django', nargs='+')
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--work-directory', type=Path)
    parser.add_argument('--uv', default='uv')
    parser.add_argument('--architecture', choices=('x86_64', 'aarch64'))
    parser.add_argument('--psycopg', choices=('binary', 'python'), default='binary',
                        help='use pure psycopg with a native system libpq when binary wheels are unavailable')
    parser.add_argument('--no-baseline', action='store_true')
    options = parser.parse_args()
    wheel = options.wheel.resolve(strict=True)
    with zipfile.ZipFile(wheel) as archive:
        extensions = [n for n in archive.namelist() if n.endswith(('.so', '.pyd'))]
        if len(extensions) != 1 or '-abi3-' not in wheel.name:
            raise ValueError('expected a single-extension ABI3 wheel')
        digest = hashlib.sha256(archive.read(extensions[0])).hexdigest()
    tests = Path(__file__).resolve().parents[1] / 'tests'
    with tempfile.TemporaryDirectory(prefix='rustializer-matrix-') as temporary:
        work = (options.work_directory or Path(temporary)).resolve()
        work.mkdir(parents=True, exist_ok=True)
        checkout = options.checkout
        if checkout is None:
            checkout = work / 'reference-git'
            run(['git', 'clone', '--filter=blob:none', '--no-checkout',
                 'https://github.com/encode/django-rest-framework.git', checkout], work / 'clone.log')
        references = {}
        reference_wheels = {}
        for version in options.drf:
            source = work / f'drf-{version}'
            if not source.exists():
                source.mkdir()
                archive = work / f'drf-{version}.tar'
                run(['git', '-C', checkout, 'archive', REFERENCES[version], '-o', archive], work / f'archive-{version}.log')
                with tarfile.open(archive) as content:
                    content.extractall(source, filter='data')
            suite = work / f'installed-wheel-suite-{version}'
            suite.mkdir(exist_ok=True)
            shutil.copytree(source / 'tests', suite / 'tests', dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
            shutil.copy2(source / 'pyproject.toml', suite / 'pyproject.toml')
            references[version] = (source, suite)
            distribution = work / f'reference-wheel-{version}'
            distribution.mkdir(exist_ok=True)
            run([options.uv, 'build', source, '--wheel', '--out-dir', distribution],
                work / f'build-reference-{version}.log')
            reference_wheels[version], = distribution.glob('*.whl')
        plugin = work / 'activation'
        plugin.mkdir(exist_ok=True)
        (plugin / 'rustializer_activation.py').write_text('import rustializer\nrustializer.activate()\n')
        combinations = [(drf, python, django) for drf in options.drf
                        for python, versions in MATRIX[drf].items()
                        for django in versions
                        if (options.python is None or python in options.python)
                        and (options.django is None or django in options.django)]
        if not combinations:
            raise ValueError('no agreed runtime combinations selected')

        def check(combination):
            drf, version, django = combination
            key = f'drf-{drf}-py{version}-django{django}'
            directory = work / key
            directory.mkdir(exist_ok=True)
            virtualenv = directory / 'env'
            environment = dict(os.environ, PYTHONMALLOC='debug', PYTHONNOUSERSITE='1',
                               PYTHONDONTWRITEBYTECODE='1', UV_LINK_MODE='copy')
            environment.pop('PYTHONPATH', None)
            environment.pop('PYTHONHOME', None)
            # Upstream tests must use their own disposable SQLite databases.
            environment.pop('DATABASE_URL', None)
            request = version
            if options.architecture:
                import platform
                system = {'Linux': 'linux', 'Darwin': 'macos', 'Windows': 'windows'}[platform.system()]
                libc = 'gnu' if system == 'linux' else 'none'
                request = f'cpython-{version}-{system}-{options.architecture}-{libc}'
            run([options.uv, 'venv', '--python', request, virtualenv], directory / 'venv.log', env=environment)
            python = virtualenv / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
            source, suite = references[drf]
            major, minor = map(int, django.split('.'))
            upper = f'{major}.{minor + 1}'
            optional = ['--group', f'{source / "pyproject.toml"}:optional']
            if options.psycopg == 'python':
                import tomllib
                groups = tomllib.loads((source / 'pyproject.toml').read_text())['dependency-groups']
                # Keep upstream source/tests untouched. Only select the documented
                # pure installation of psycopg, with the same version constraint.
                optional = [requirement.replace('psycopg[binary]', 'psycopg')
                            for requirement in groups['optional']]
                environment['PSYCOPG_IMPL'] = 'python'
            run([options.uv, 'pip', 'install', '--python', python,
                 '--group', f'{source / "pyproject.toml"}:test', *optional,
                 f'Django>={django},<{upper}', 'pillow', reference_wheels[drf], wheel], directory / 'install.log', env=environment)
            probe = subprocess.run([str(python), '-I', '-X', 'dev', '-c', PROBE], env=environment,
                                   check=True, text=True, capture_output=True)
            metadata = json.loads(probe.stdout)
            metadata['psycopg_installation'] = options.psycopg
            if not Path(metadata['drf_package']).is_relative_to(virtualenv):
                raise RuntimeError(f'expected installed DRF wheel, got: {metadata}')
            if metadata['binary_sha256'] != digest or metadata['reference'] != REFERENCES[drf]:
                raise RuntimeError(f'binary or source reference mismatch: {metadata}')
            if not metadata['python'].startswith(version + '.') or not metadata['django'].startswith(django + '.'):
                raise RuntimeError(f'runtime mismatch: {metadata}')
            actual_arch = {'ARM64': 'aarch64', 'arm64': 'aarch64', 'AMD64': 'x86_64'}.get(metadata['architecture'], metadata['architecture'])
            if options.architecture and actual_arch != options.architecture:
                raise RuntimeError(f'architecture mismatch: {metadata}')
            if metadata['drf'] != drf:
                raise RuntimeError(f'DRF package mismatch: {metadata}')
            run([python, '-I', '-X', 'dev', '-m', 'unittest', 'discover', '-s', tests, '-v'],
                directory / 'integration.log', cwd=directory, env=environment)
            if not options.no_baseline:
                metadata['baseline_seconds'] = run([python, '-m', 'pytest', '-q', '--tb=short'],
                    directory / 'baseline.log', cwd=suite, env=environment)
            activated = dict(environment, PYTHONPATH=str(plugin))
            metadata['activated_seconds'] = run([python, '-m', 'pytest', '-p', 'rustializer_activation', '-q', '--tb=short'],
                directory / 'activated.log', cwd=suite, env=activated)
            metadata['result'] = (directory / 'activated.log').read_text().splitlines()[-1]
            metadata['logs'] = str(directory)
            (directory / 'runtime.json').write_text(json.dumps(metadata, indent=2) + '\n')
            return metadata

        failures = []
        results = []
        with ThreadPoolExecutor(max_workers=options.jobs) as pool:
            futures = {pool.submit(check, combination): combination for combination in combinations}
            for future in as_completed(futures):
                try:
                    result = future.result()
                    results.append(result)
                    print(json.dumps(result), flush=True)
                except Exception as error:
                    failures.append(futures[future])
                    print(json.dumps({'failed': futures[future], 'error': str(error)}), flush=True)
        print(json.dumps({'passed': len(results), 'failed': len(failures), 'selected': len(combinations),
                          'binary_sha256': digest}), flush=True)
        if failures:
            raise SystemExit(1)


if __name__ == '__main__':
    main()
