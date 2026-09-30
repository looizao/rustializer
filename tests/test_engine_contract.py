"""Fresh-process activation and built-package behavioral compatibility checks."""
import json
from pathlib import Path
import subprocess
import sys
import unittest


class EngineContractTests(unittest.TestCase):
    def run_code(self, code):
        result = subprocess.run([sys.executable, '-I', '-X', 'dev', '-c', code],
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def test_early_activation_and_native_class_methods(self):
        self.run_code('''
import inspect, types
import rustializer
rustializer.activate()
from django.conf import settings
settings.configure(USE_I18N=False)
from rest_framework import fields, serializers, validators, relations, exceptions
from rest_framework.utils import serializer_helpers, model_meta, formatting
for module in (fields, serializers, validators, relations, exceptions, serializer_helpers, model_meta, formatting):
    for value in vars(module).values():
        if not isinstance(value, type) or value.__module__ != module.__name__:
            continue
        for attribute in vars(value).values():
            if isinstance(attribute, (staticmethod, classmethod)):
                attribute = attribute.__func__
            if isinstance(attribute, property):
                assert not isinstance(attribute.fget, types.FunctionType), value
            assert not isinstance(attribute, types.FunctionType), value
assert not hasattr(serializers.Serializer.to_representation, '__code__')
rustializer.activate()
''')

    def test_activation_and_callbacks_under_aggressive_garbage_collection(self):
        self.run_code("""
import gc
# Native objects can be traversed while CPython allocates their instance dict.
gc.set_threshold(1, 1, 1)
import rustializer
rustializer.activate()
from django.conf import settings
settings.configure(USE_I18N=False)
from rest_framework import serializers
class Example(serializers.Serializer):
    value = serializers.IntegerField()
for _ in range(20):
    item = Example(data={'value': '2'})
    assert item.is_valid() and item.data == {'value': 2}
    item.cycle = item.run_validation
    gc.collect()
""")

    def test_late_activation_rejected(self):
        self.run_code('''
from rest_framework import serializers
original = serializers.Serializer
import rustializer
try:
    rustializer.activate()
except RuntimeError as error:
    assert 'already imported' in str(error)
else:
    raise AssertionError('late activation succeeded')
assert serializers.Serializer is original
''')

    def test_unsupported_version_rejected_without_patch(self):
        self.run_code('''
import rest_framework, sys
rest_framework.VERSION = '3.19.0'
import rustializer
try:
    rustializer.activate()
except ImportError as error:
    assert '3.19.0' in str(error)
else:
    raise AssertionError('unsupported version succeeded')
assert 'rest_framework.fields' not in sys.modules
''')

    def test_failed_activation_rolls_back_and_preserves_exception(self):
        self.run_code('''
import sys, django.utils.translation
import rustializer
original = django.utils.translation.gettext_lazy
failure = RuntimeError('dependency failure')
def fail(*args, **kwargs):
    raise failure
django.utils.translation.gettext_lazy = fail
try:
    rustializer.activate()
except RuntimeError as error:
    assert error is failure
else:
    raise AssertionError('activation did not fail')
assert 'rest_framework.fields' not in sys.modules
assert 'rest_framework.exceptions' not in sys.modules
django.utils.translation.gettext_lazy = original
rustializer.activate()
''')

    def test_reload_and_cache_eviction_keep_native_engine(self):
        self.run_code('''
import rustializer
rustializer.activate()
import importlib, sys
from rest_framework import fields
importlib.reload(fields)
assert not hasattr(fields.Field.bind, '__code__')
sys.modules.pop('rest_framework.fields')
fields = importlib.import_module('rest_framework.fields')
assert not hasattr(fields.Field.bind, '__code__')
''')

    def test_differential_values_types_errors_hooks_and_database_effects(self):
        script = Path(__file__).with_name('engine_scenarios.py')
        observations = []
        for engine in ('reference', 'native'):
            result = subprocess.run([sys.executable, '-I', '-X', 'dev', str(script), engine],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            observations.append(json.loads(result.stdout))
        for scenario, reference in observations[0].items():
            with self.subTest(scenario=scenario):
                self.assertEqual(observations[1][scenario], reference)


if __name__ == '__main__':
    unittest.main()
