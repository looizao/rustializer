"""Measure installed reference/native engines in alternating fresh processes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time


def peak_rss():
    if os.name == 'nt':
        import ctypes
        from ctypes import wintypes
        class Counters(ctypes.Structure):
            _fields_ = [('cb', wintypes.DWORD), ('faults', wintypes.DWORD)] + [
                (name, ctypes.c_size_t) for name in (
                    'peak_working_set', 'working_set', 'peak_paged', 'paged',
                    'peak_nonpaged', 'nonpaged', 'pagefile', 'peak_pagefile')]
        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        kernel = ctypes.WinDLL('kernel32')
        kernel.GetCurrentProcess.restype = wintypes.HANDLE
        psapi = ctypes.WinDLL('psapi')
        psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]
        if not psapi.GetProcessMemoryInfo(kernel.GetCurrentProcess(), ctypes.byref(counters), counters.cb):
            raise ctypes.WinError()
        return counters.peak_working_set
    import resource
    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return value if sys.platform == 'darwin' else value * 1024


def worker(options):
    import gc
    start = time.perf_counter_ns()
    if options.engine == 'native':
        import rustializer
        rustializer.activate()
    from django.conf import settings
    settings.configure(USE_I18N=False, USE_TZ=True, INSTALLED_APPS=[],
                       DATABASES={'default': {'ENGINE': 'django.db.backends.sqlite3', 'NAME': ':memory:'}})
    import django
    django.setup()
    from django.db import models
    from rest_framework import serializers
    import rest_framework
    startup = time.perf_counter_ns() - start
    metadata = {'engine': options.engine, 'python': platform.python_version(),
                'django': django.get_version(), 'drf': rest_framework.VERSION,
                'platform': platform.system(), 'architecture': platform.machine(),
                'startup_ns': startup, 'startup_peak_rss_bytes': peak_rss()}
    if options.startup_only:
        return metadata

    class Item(serializers.Serializer):
        id = serializers.IntegerField(min_value=0)
        name = serializers.CharField(max_length=40)
        active = serializers.BooleanField()
        score = serializers.DecimalField(max_digits=8, decimal_places=2)
        tags = serializers.ListField(child=serializers.CharField())

    class Collection(serializers.Serializer):
        title = serializers.CharField()
        items = Item(many=True)

    class Model(models.Model):
        name = models.CharField(max_length=40, unique=True)
        active = models.BooleanField(default=True)
        count = models.IntegerField(default=0)
        class Meta:
            app_label = 'benchmark'

    class ModelSerializer(serializers.ModelSerializer):
        class Meta:
            model = Model
            fields = '__all__'

    row = {'id': 7, 'name': 'sample', 'active': True, 'score': '12.50', 'tags': ['a', 'b']}
    rows = [dict(row, id=i) for i in range(options.records)]
    warm = Item()
    nested = Collection()
    nested_data = {'title': 'batch', 'items': rows}
    invalid = dict(row, id=-1, name='x' * 50, score='invalid')

    def validation(data):
        item = Item(data=data)
        valid = item.is_valid()
        return [valid, item.validated_data, item.errors]

    cases = {
        'warm_representation': (lambda: warm.to_representation(row), 1),
        'fresh_representation': (lambda: Item(row).data, 1),
        'nested_many_representation': (lambda: nested.to_representation(nested_data), options.records),
        'valid_input': (lambda: validation(row), 1),
        'invalid_input': (lambda: validation(invalid), 1),
        'model_field_generation': (lambda: list(ModelSerializer().fields), 1),
    }
    observations = {}
    for name, (operation, units) in cases.items():
        for _ in range(5):
            result = operation()
        fingerprint = hashlib.sha256(json.dumps(result, sort_keys=True, default=str).encode()).hexdigest()
        gc.collect()
        elapsed = []
        cpu_start = time.process_time_ns()
        for _ in range(options.iterations):
            before = time.perf_counter_ns()
            operation()
            elapsed.append(time.perf_counter_ns() - before)
        observations[name] = {'latency_ns': elapsed, 'cpu_ns': time.process_time_ns() - cpu_start,
                              'units_per_operation': units, 'output_sha256': fingerprint}

    # Keep objects alive so retained memory cannot be hidden by cyclic collection.
    gc.collect()
    import tracemalloc
    tracemalloc.start()
    retained = [Item(row) for _ in range(options.memory_objects)]
    for item in retained:
        _ = item.fields
    current, peak = tracemalloc.get_traced_memory()
    metadata.update(cases=observations, peak_rss_bytes=peak_rss(),
                    retained_objects=len(retained), python_traced_current_bytes=current,
                    python_traced_peak_bytes=peak)
    return metadata


def percentile(values, fraction):
    values = sorted(values)
    position = (len(values) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(values) - 1)
    return values[lower] + (values[upper] - values[lower]) * (position - lower)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--python', default=sys.executable)
    parser.add_argument('--samples', type=int, default=5)
    parser.add_argument('--iterations', type=int, default=100)
    parser.add_argument('--records', type=int, default=50)
    parser.add_argument('--memory-objects', type=int, default=1000)
    parser.add_argument('--engines', nargs='+', choices=('reference', 'native'), default=['reference', 'native'])
    parser.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--engine', choices=('reference', 'native'), help=argparse.SUPPRESS)
    parser.add_argument('--startup-only', action='store_true', help=argparse.SUPPRESS)
    options = parser.parse_args()
    if min(options.samples, options.iterations, options.records, options.memory_objects) < 1:
        parser.error('sample sizes must be positive')
    if options.worker:
        print(json.dumps(worker(options)))
        return
    environment = dict(os.environ, PYTHONNOUSERSITE='1', PYTHONDONTWRITEBYTECODE='1')
    environment.pop('PYTHONPATH', None)
    environment.pop('PYTHONMALLOC', None)
    results = {engine: [] for engine in options.engines}
    startups = {engine: [] for engine in options.engines}
    for sample in range(options.samples):
        engines = options.engines if sample % 2 == 0 else list(reversed(options.engines))
        for engine in engines:
            command = [options.python, '-I', str(Path(__file__).resolve()), '--worker', '--engine', engine,
                       '--iterations', str(options.iterations), '--records', str(options.records),
                       '--memory-objects', str(options.memory_objects)]
            before = time.perf_counter_ns()
            startup = subprocess.run([*command, '--startup-only'], env=environment,
                                     capture_output=True, text=True, check=True)
            startups[engine].append(time.perf_counter_ns() - before)
            result = subprocess.run(command, env=environment, capture_output=True, text=True, check=True)
            results[engine].append(json.loads(result.stdout))
    summary = {}
    for engine, runs in results.items():
        first = runs[0]
        summary[engine] = {key: first[key] for key in ('python', 'django', 'drf', 'platform', 'architecture')}
        summary[engine].update(startup_process_p50_ms=statistics.median(startups[engine]) / 1e6,
            startup_import_p50_ms=statistics.median(r['startup_ns'] for r in runs) / 1e6,
            startup_peak_rss_p50_bytes=statistics.median(r['startup_peak_rss_bytes'] for r in runs),
            peak_rss_p50_bytes=statistics.median(r['peak_rss_bytes'] for r in runs),
            retained_objects=options.memory_objects,
            python_traced_current_p50_bytes=statistics.median(r['python_traced_current_bytes'] for r in runs),
            cases={})
        for name in first['cases']:
            values = [value for run in runs for value in run['cases'][name]['latency_ns']]
            fingerprints = {run['cases'][name]['output_sha256'] for run in runs}
            if len(fingerprints) != 1:
                raise RuntimeError(f'non-deterministic output for {engine}/{name}')
            units = first['cases'][name]['units_per_operation']
            summary[engine]['cases'][name] = {'p50_us': percentile(values, .5) / 1000,
                'p95_us': percentile(values, .95) / 1000, 'p99_us': percentile(values, .99) / 1000,
                'units_per_second': units * len(values) * 1e9 / sum(values),
                'cpu_us_per_operation': sum(r['cases'][name]['cpu_ns'] for r in runs) / len(values) / 1000,
                'output_sha256': fingerprints.pop()}
    if len(summary) == 2:
        for name in summary['reference']['cases']:
            if summary['reference']['cases'][name]['output_sha256'] != summary['native']['cases'][name]['output_sha256']:
                raise RuntimeError(f'reference/native output mismatch for {name}')
    print(json.dumps({'samples': options.samples, 'iterations_per_sample': options.iterations,
                      'records_per_many_call': options.records, 'results': summary}, indent=2))


if __name__ == '__main__':
    main()
