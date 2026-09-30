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

# Readable fields are lazy generators over one live values iterator. Hooks can
# replace the container or change a later field's visibility between yields.
readable_events = []
class ReadableFlag:
    def __init__(self, field):
        self.field = field
    def __bool__(self):
        readable_events.append(['truth', self.field.name, self.field.hidden])
        return self.field.hidden
class ReadableField:
    def __init__(self, name, hidden=False):
        self.name, self.hidden = name, hidden
    @property
    def write_only(self):
        readable_events.append(['flag', self.name])
        return ReadableFlag(self)
class ReadableIterator:
    def __init__(self, fields):
        self.items = iter(fields)
    def __iter__(self):
        return self
    def __getattribute__(self, name):
        if name == '__next__':
            raise AssertionError('iteration must use the iterator slot')
        return object.__getattribute__(self, name)
    def __next__(self):
        readable_events.append(['next'])
        return next(self.items)
class ReadableContainer:
    def __init__(self, fields):
        self.items = fields
    def values(self):
        readable_events.append(['values'])
        return ReadableIterator(self.items)
class ReadableSerializer(serializers.Serializer):
    @property
    def fields(self):
        readable_events.append(['fields'])
        return self.container

readable_owner = ReadableSerializer()
first, second, third = [ReadableField(name) for name in ('first', 'second', 'third')]
readable_owner.container = ReadableContainer([first, second, third])
readable_stream = readable_owner._readable_fields
results['readable-lazy-creation'] = list(readable_events)
readable_names = [next(readable_stream).name]
second.hidden = True
readable_owner.container = ReadableContainer([ReadableField('replacement')])
readable_names.append(readable_stream.send(123).name)
readable_names.extend(field.name for field in readable_stream)
results['readable-live-iteration'] = [readable_names, list(readable_events)]

results['readable-generator-protocol'] = []
for operation in ('send-before-start', 'send', 'close-before-start', 'close', 'throw'):
    readable_events.clear()
    readable_owner.container = ReadableContainer([ReadableField('one'), ReadableField('two')])
    readable_stream = readable_owner._readable_fields
    observed = []
    failure = RuntimeError('readable injection')
    try:
        if operation == 'send-before-start':
            readable_stream.send(1)
        elif operation == 'close-before-start':
            observed.append(readable_stream.close())
            next(readable_stream)
        else:
            observed.append(next(readable_stream).name)
            if operation == 'send':
                observed.append(readable_stream.send(123).name)
            elif operation == 'close':
                observed.append(readable_stream.close())
                next(readable_stream)
            else:
                readable_stream.throw(failure)
    except Exception as error:
        observed.extend([type(error).__name__, str(error), error is failure])
    results['readable-generator-protocol'].append([operation, observed, list(readable_events)])

class ReentrantReadableField(ReadableField):
    @property
    def write_only(self):
        for operation in ('next', 'close', 'throw'):
            try:
                if operation == 'next':
                    next(readable_stream)
                elif operation == 'close':
                    readable_stream.close()
                else:
                    readable_stream.throw(RuntimeError('reentrant'))
            except Exception as error:
                readable_events.append([operation, type(error).__name__, str(error)])
        return False
readable_events.clear()
readable_owner.container = ReadableContainer([ReentrantReadableField('reentrant')])
readable_stream = readable_owner._readable_fields
results['readable-reentry'] = [next(readable_stream).name, list(readable_events)]
readable_stream.close()

class FailingReadableField(ReadableField):
    @property
    def write_only(self):
        raise StopIteration('flag failure')
readable_owner.container = ReadableContainer([FailingReadableField('failure')])
readable_stream = readable_owner._readable_fields
try:
    next(readable_stream)
except Exception as error:
    results['readable-stop-iteration'] = [type(error).__name__, str(error),
        type(error.__cause__).__name__, str(error.__cause__), error.__suppress_context__]
results['readable-closed-after-failure'] = list(readable_stream)

readable_owner = serializers.Serializer()
readable_owner.fields['one'] = serializers.CharField()
readable_owner.fields['two'] = serializers.CharField()
readable_stream = readable_owner._readable_fields
next(readable_stream)
readable_owner.fields.pop('two')
try:
    next(readable_stream)
except Exception as error:
    results['readable-container-size-mutation'] = [type(error).__name__, str(error)]

readable_owner = serializers.Serializer()
readable_owner.stream = readable_owner._readable_fields
owner_reference, stream_reference = weakref.ref(readable_owner), weakref.ref(readable_owner.stream)
del readable_owner
gc.collect()
results['readable-cycle-collection'] = [owner_reference() is None, stream_reference() is None]

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
for supplied in ((), (False,), (False, '4'), (False, '4', 'extra'),
                 [False, '4', 'extra'], {False: 1, '4': 2, 'extra': 3},
                 iter((False, '4', 'extra')), 7):
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

from rest_framework.fields import is_simple_callable
with patch.object(inspect, 'isfunction', return_value=False) as function_hook, \
        patch.object(inspect, 'ismethod', return_value=False) as method_hook:
    result = is_simple_callable(serializers.Field.bind)
    results['inspection-callback-hooks'] = [result, function_hook.call_count, method_hook.call_count]

metadata_function = serializers.Field.bind
results['function-metadata-mutation'] = []
class MetadataName(str):
    pass
for attribute in ('__name__', '__qualname__', '__module__', '__doc__'):
    original = getattr(metadata_function, attribute)
    value = MetadataName('changed') if attribute in ('__name__', '__qualname__') else [1, 2]
    try:
        setattr(metadata_function, attribute, value)
        results['function-metadata-mutation'].append([attribute, getattr(metadata_function, attribute) is value])
        setattr(metadata_function, attribute, None)
        results['function-metadata-mutation'].append([attribute, getattr(metadata_function, attribute)])
    except Exception as error:
        results['function-metadata-mutation'].append([attribute, type(error).__name__, str(error)])
    finally:
        setattr(metadata_function, attribute, original)
globals_function = serializers.CharField.to_representation
original_str = globals_function.__globals__.get('str')
had_str = 'str' in globals_function.__globals__
try:
    globals_function.__globals__['str'] = lambda value: 'overridden'
    results['function-global-mutation'] = serializers.CharField().to_representation(5)
finally:
    if had_str:
        globals_function.__globals__['str'] = original_str
    else:
        del globals_function.__globals__['str']

record = RelationInfo(1, 2, True, None, False, False)
results['record-metadata'] = {name: [str(inspect.signature(getattr(RelationInfo, name))),
    getattr(RelationInfo, name).__module__, getattr(RelationInfo, name).__doc__]
    for name in ('__new__', '_make', '_replace', '__repr__', '_asdict', '__getnewargs__')}
results['record-replace-api'] = hasattr(record, '__replace__')
if hasattr(copy, 'replace'):
    results['record-copy-replace'] = observe(copy.replace(record, to_many=False))
results['record-new-keyword'] = observe(RelationInfo.__new__(_cls=RelationInfo,
    **dict(zip(RelationInfo._fields, record))))
try:
    record._replace(self=record)
except Exception as error:
    results['record-positional-self'] = [type(error).__name__, str(error)]
original_fields = RelationInfo._fields
try:
    RelationInfo._fields = tuple('changed' for _ in original_fields)
    results['record-field-name-mutation'] = [repr(record), observe(record._replace(model_field=7)), record._asdict()]
finally:
    RelationInfo._fields = original_fields
record_events = []
class HookRecord(RelationInfo):
    @classmethod
    def _make(cls, iterable):
        record_events.append(type(iterable).__name__)
        return super()._make(iterable)
results['record-iterator-hook'] = [observe(HookRecord(*record)._replace(model_field=7)), record_events]

results['function-metadata-deletion'] = []
for attribute in ('__name__', '__qualname__', '__module__', '__doc__', '__defaults__',
                  '__kwdefaults__', '__annotations__', '__globals__', '__dict__'):
    original = getattr(metadata_function, attribute)
    try:
        delattr(metadata_function, attribute)
        results['function-metadata-deletion'].append([attribute, getattr(metadata_function, attribute)])
    except Exception as error:
        results['function-metadata-deletion'].append([attribute, type(error).__name__, str(error)])
    finally:
        if attribute not in ('__globals__', '__dict__'):
            setattr(metadata_function, attribute, original)

metadata_events = []
class MetadataFinalizer:
    def __del__(self):
        metadata_function.__doc__ = 'set by finalizer'
        metadata_events.append('finalized')
original_doc = metadata_function.__doc__
try:
    metadata_function.__doc__ = MetadataFinalizer()
    metadata_function.__doc__ = 'replacement'
    results['metadata-finalizer-reentry'] = [metadata_function.__doc__, metadata_events]
finally:
    metadata_function.__doc__ = original_doc

metadata_events.clear()
invoked_function = serializers.CharField.to_representation
original_doc = invoked_function.__doc__
had_str = 'str' in invoked_function.__globals__
original_str = invoked_function.__globals__.get('str')
class InvokedMetadataFinalizer:
    def __del__(self):
        metadata_events.append('finalized')
def replacement_str(value):
    invoked_function.__doc__ = 'replacement'
    metadata_events.append('callback')
    return str(value)
try:
    invoked_function.__doc__ = InvokedMetadataFinalizer()
    invoked_function.__globals__['str'] = replacement_str
    result = serializers.CharField().to_representation(7)
    results['metadata-finalizer-during-call'] = [result, list(metadata_events)]
finally:
    invoked_function.__doc__ = original_doc
    if had_str:
        invoked_function.__globals__['str'] = original_str
    else:
        del invoked_function.__globals__['str']

print(json.dumps(results, sort_keys=True, default=str))
