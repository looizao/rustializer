"""Serializable differential observations, run unchanged against either engine."""
import copy
import datetime
from decimal import Decimal
import gc
import inspect
import json
import pickle
import sys
import uuid
import weakref

if len(sys.argv) > 1 and sys.argv[1] == 'native':
    import rustializer
    rustializer.activate()

from django.conf import settings
settings.configure(
    SECRET_KEY='differential', USE_I18N=False, USE_TZ=True,
    INSTALLED_APPS=['django.contrib.contenttypes'],
    DATABASES={'default': {'ENGINE': 'django.db.backends.sqlite3', 'NAME': ':memory:'}},
    DEFAULT_AUTO_FIELD='django.db.models.AutoField',
)
import django
django.setup()
from django.db import connection, models
from django.test.utils import CaptureQueriesContext
from rest_framework import serializers
from rest_framework.exceptions import ErrorDetail, ValidationError
from rest_framework.utils.model_meta import RelationInfo


def observe(value):
    kind = f'{type(value).__module__}.{type(value).__qualname__}'
    if isinstance(value, ErrorDetail):
        return [kind, str(value), value.code]
    if isinstance(value, dict):
        return [kind, [[observe(k), observe(v)] for k, v in value.items()]]
    if isinstance(value, (list, tuple)):
        return [kind, [observe(item) for item in value]]
    if isinstance(value, (datetime.date, datetime.time, Decimal, uuid.UUID)):
        return [kind, str(value)]
    if isinstance(value, bytes):
        return [kind, value.hex()]
    return [kind, value]


results = {}
events = []


class HookField(serializers.CharField):
    def bind(self, field_name, parent):
        events.append(['bind', field_name])
        super().bind(field_name, parent)

    def to_internal_value(self, data):
        events.append(['internal', data])
        return super().to_internal_value(data)

    def to_representation(self, value):
        events.append(['representation', value])
        return super().to_representation(value)


class Common(serializers.Serializer):
    text = HookField(max_length=8)
    number = serializers.IntegerField(min_value=0, required=False, default=3)
    nullable = serializers.CharField(allow_null=True, required=False)
    hidden = serializers.HiddenField(default='secret')
    amount = serializers.DecimalField(max_digits=8, decimal_places=2, required=False)

    def validate_text(self, value):
        events.append(['validate_text', value])
        if value == 'bad':
            raise serializers.ValidationError('bad text', code='custom')
        return value.upper()

    def validate(self, attrs):
        events.append(['validate', dict(attrs)])
        return attrs


for index, data in enumerate((
    {'text': ' hi ', 'number': '4', 'nullable': None, 'amount': '1.234'},
    {'text': 'bad', 'number': '-1'}, {}, {'text': 'far too long'},
)):
    events.clear()
    item = Common(data=data)
    valid = item.is_valid()
    results[f'validation-{index}'] = {
        'valid': valid, 'values': observe(item.validated_data), 'errors': observe(item.errors),
        'data': observe(item.data), 'events': observe(copy.deepcopy(events)),
        'repr': repr(item),
    }
    if not valid:
        try:
            item.is_valid(raise_exception=True)
        except ValidationError as error:
            results[f'error-{index}'] = [observe(error.detail), observe(error.get_codes()), observe(error.get_full_details())]

partial = Common(data={}, partial=True)
partial.is_valid()
results['partial'] = [observe(partial.validated_data), observe(partial.errors)]
many = Common(data=[{'text': 'one'}, {'text': 'two'}], many=True)
many.is_valid()
results['many'] = [observe(many.data), observe(many.validated_data), observe(many.errors)]

s = Common({'text': 'abc', 'number': 2})
s.fields.pop('text')
s.fields['extra'] = serializers.CharField(source='text')
results['mutation'] = [observe(s.data), s.fields['extra'].field_name, s.fields['extra'].parent is s]


class Left(serializers.Serializer):
    shared = serializers.CharField(label='left')
    left = serializers.IntegerField()


class Right(serializers.Serializer):
    shared = serializers.CharField(label='right')
    right = serializers.IntegerField()


class Combined(Left, Right):
    left = None
    last = serializers.CharField()


combined = Combined()
results['inheritance'] = [list(combined.fields), combined.fields['shared'].label]

field = HookField(label={'nested': [1]}, validators=[lambda value: None])
cloned = copy.deepcopy(field)
results['deepcopy'] = [type(cloned).__name__, cloned.label == field.label,
                       cloned.label is not field.label, cloned._kwargs['validators'] is field._kwargs['validators']]
for key, value in [('default', serializers.CreateOnlyDefault(7)), ('record', RelationInfo(1, 2, 3, 4, 5, 6)),
                   ('field', serializers.IntegerField(min_value=2)), ('detail', ErrorDetail('missing', 'required'))]:
    restored = pickle.loads(pickle.dumps(value))
    if key in ('default', 'field'):
        results[f'pickle-{key}'] = [type(restored).__name__, observe(restored.__dict__ if key == 'default' else restored._kwargs)]
    else:
        results[f'pickle-{key}'] = observe(restored)

for key, cls in [('dict', type(s.data)), ('list', type(many.data))]:
    value = s.data if key == 'dict' else many.data
    results[f'pickle-return-{key}'] = observe(pickle.loads(pickle.dumps(value)))

cycle = Common()
ref = weakref.ref(cycle)
cycle.saved = cycle.to_representation
cycle.cycle = cycle
_ = cycle.fields
cycle = None
gc.collect()
results['gc'] = ref() is None

failure = RuntimeError('application callback')
seen = []


class FailureField(serializers.Field):
    def to_representation(self, value):
        seen.append(value)
        raise failure


class FailureSerializer(serializers.Serializer):
    item = FailureField()


try:
    FailureSerializer({'item': 1}).data
except RuntimeError as error:
    results['exception-identity'] = [error is failure, seen]


class Thing(models.Model):
    name = models.CharField(max_length=20, unique=True)
    count = models.IntegerField(default=0)
    class Meta:
        app_label = 'differential'


class ThingSerializer(serializers.ModelSerializer):
    class Meta:
        model = Thing
        fields = '__all__'

    def create(self, validated_data):
        events.append(['create', dict(validated_data)])
        return super().create(validated_data)

    def update(self, instance, validated_data):
        events.append(['update', dict(validated_data)])
        return super().update(instance, validated_data)


with connection.schema_editor() as editor:
    editor.create_model(Thing)
events.clear()
with CaptureQueriesContext(connection) as queries:
    creator = ThingSerializer(data={'name': 'one', 'count': 1})
    assert creator.is_valid(), creator.errors
    instance = creator.save(count=2)
    updater = ThingSerializer(instance, data={'count': 4}, partial=True)
    assert updater.is_valid(), updater.errors
    updater.save()
    duplicate = ThingSerializer(data={'name': 'one'})
    duplicate.is_valid()
results['database'] = {
    'rows': list(Thing.objects.values_list('id', 'name', 'count')),
    'errors': observe(duplicate.errors), 'events': observe(events),
    'query-types': [q['sql'].split()[0] for q in queries],
    'data': observe(updater.data), 'fields': repr(ThingSerializer()),
}

results['signatures'] = {
    name: str(inspect.signature(getattr(serializers.Field, name)))
    for name in ('bind', '__init__', '__new__', 'run_validation', 'fail')
}
results['binding-errors'] = []
for method, args, kwargs in (
    (serializers.Field.bind, (), {}),
    (serializers.Field.bind, (field,), {}),
    (field.bind, (), {'unexpected': True}),
    (field.bind, ('name', s, 3), {}),
    (field.bind, ('name', s), {'field_name': 'duplicate'}),
    (field.run_validation, (1, 2), {}),
):
    try:
        method(*args, **kwargs)
    except Exception as error:
        results['binding-errors'].append([type(error).__name__, str(error)])

# Public callable behavior, including saved methods and mutable defaults.
from rest_framework.utils import timezone as drf_timezone, json as drf_json
results['annotated-signature'] = str(inspect.signature(drf_timezone.datetime_ambiguous))
results['wrapped-signature'] = [str(inspect.signature(drf_json.dumps)),
    str(inspect.signature(drf_json.dumps, follow_wrapped=False))]
method_field = serializers.IntegerField()
saved_method = method_field.run_validation
results['bound-methods'] = [saved_method == method_field.run_validation,
    saved_method.__func__ is serializers.Field.run_validation,
    pickle.loads(pickle.dumps(serializers.Field.bind)) is serializers.Field.bind]
original_defaults = serializers.Field.run_validation.__defaults__
try:
    serializers.Field.run_validation.__defaults__ = (5,)
    results['mutable-defaults'] = [saved_method(), str(inspect.signature(saved_method))]
finally:
    serializers.Field.run_validation.__defaults__ = original_defaults
original_required = serializers.Field.__init__.__kwdefaults__['required']
try:
    serializers.Field.__init__.__kwdefaults__['required'] = False
    results['mutable-keyword-defaults'] = serializers.IntegerField().required
finally:
    serializers.Field.__init__.__kwdefaults__['required'] = original_required

handled = []
class HandledDecimal(serializers.DecimalField):
    def fail(self, key, **kwargs):
        current = sys.exc_info()[1]
        handled.append([key, type(current).__name__ if current else None])
        return super().fail(key, **kwargs)
try:
    HandledDecimal(max_digits=4, decimal_places=2).run_validation('invalid')
except ValidationError as error:
    results['handled-exception'] = [handled, type(error.__context__).__name__,
        error.__cause__ is None, error.__suppress_context__, sys.exc_info()[1] is error]

import warnings
from unittest.mock import patch
with patch.object(warnings, 'warn') as warning_hook:
    serializers.DecimalField(max_digits=4, decimal_places=2, min_value=1.0)
    results['warning-hook'] = [[str(call.args[0]), sorted(call.kwargs)]
        for call in warning_hook.call_args_list]

results['callable-mutation-errors'] = []
for name in ('__defaults__', '__kwdefaults__', '__annotations__'):
    try:
        setattr(serializers.Field.run_validation, name, [])
    except Exception as error:
        results['callable-mutation-errors'].append([name, type(error).__name__, str(error)])

import contextlib
original_exit = contextlib.suppress.__exit__
context_events = []
def exit_hook(self, typ, value, traceback):
    context_events.append([typ.__name__, sys.exc_info()[1] is value])
    return original_exit(self, typ, value, traceback)
with patch.object(contextlib.suppress, '__exit__', exit_hook):
    try:
        serializers.BooleanField().run_validation([])
    except ValidationError:
        pass
results['context-manager-exception'] = context_events

results['generator-protocol'] = []
for operation in ('send-before-start', 'send', 'close', 'throw'):
    stream = iter(Common({'text': 'abc', 'number': 2}))
    observed = []
    try:
        if operation == 'send-before-start':
            stream.send(1)
        else:
            observed.append(type(next(stream)).__name__)
            if operation == 'send':
                observed.append(type(stream.send(123)).__name__)
            elif operation == 'close':
                observed.append(stream.close())
                next(stream)
            else:
                stream.throw(RuntimeError('injected'))
    except Exception as error:
        observed.extend([type(error).__name__, str(error)])
    results['generator-protocol'].append([operation, observed])

from rest_framework.validators import UniqueValidator
truth_events = []
class TruthToken:
    def __init__(self, value):
        self.value = value
    def __bool__(self):
        truth_events.append(self.value)
        return self.value
class EqualityHook:
    def __init__(self, token):
        self.token = token
    def __eq__(self, other):
        return self.token
results['truthiness-callbacks'] = []
for attribute, value in (('lookup', True), ('message', True), ('message', False)):
    token = TruthToken(value)
    left, right = UniqueValidator([]), UniqueValidator([])
    setattr(left, attribute, EqualityHook(token))
    setattr(right, attribute, EqualityHook(token))
    truth_events.clear()
    result = left == right
    results['truthiness-callbacks'].append([attribute, value, result is token, list(truth_events)])

import operator
with patch.object(operator, 'add', side_effect=AssertionError('unrelated operator patch')):
    arithmetic_field = serializers.IntegerField()
    results['native-arithmetic-dispatch'] = arithmetic_field.run_validation('5')

mutation_events = []
class RenameField(serializers.IntegerField):
    def to_representation(self, value):
        mutation_events.append(['represent', self.field_name, value])
        self.field_name = 'renamed'
        return super().to_representation(value)
class RenameSerializer(serializers.Serializer):
    number = RenameField()
results['representation-key-mutation'] = [RenameSerializer({'number': 3}).data, mutation_events]

child_events = []
class NextChild(serializers.BaseSerializer):
    def to_representation(self, value):
        child_events.append(['next', value])
        return value + 10
class FirstChild(serializers.BaseSerializer):
    def to_representation(self, value):
        child_events.append(['first', value])
        self.parent.child = NextChild()
        return value
changing_list = serializers.ListSerializer(child=FirstChild())
results['list-child-mutation'] = [changing_list.to_representation([1, 2, 3]), child_events]

class UnpackField(serializers.IntegerField):
    def validate_empty_values(self, data):
        return data
results['validation-unpack'] = []
for supplied in ((), (False,), (False, '4'), (False, '4', 'extra'), 7):
    try:
        result = UnpackField().run_validation(supplied)
        results['validation-unpack'].append(['value', result])
    except Exception as error:
        results['validation-unpack'].append([type(error).__name__, str(error)])
unpack_events = []
def too_many_values():
    for index in range(5):
        unpack_events.append(index)
        yield index
try:
    UnpackField().run_validation(too_many_values())
except ValueError as error:
    results['unpack-iterator-effects'] = [str(error), unpack_events]

print(json.dumps(results, sort_keys=True, default=str))
