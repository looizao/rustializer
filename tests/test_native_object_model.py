"""Behavioral feasibility checks, not DRF compatibility certification."""

import copy
import gc
import inspect
import pickle
import unittest
import weakref
from unittest.mock import patch

from rustializer import _feasibility as native


class PickleField(native.Field):
    pass


class NativeObjectModelTests(unittest.TestCase):
    def test_native_methods_and_inherited_storage(self):
        self.assertEqual(type(native.Field.to_representation).__module__, native.__name__)
        self.assertFalse(hasattr(native.Field.to_representation, "__code__"))
        for cls in (native.Field, native.BaseSerializer, native.Serializer, native.ModelSerializer):
            self.assertEqual(cls.__basicsize__, native._NativeState.__basicsize__)
        self.assertEqual(
            list(inspect.signature(native.Serializer.to_representation).parameters),
            ["self", "instance"],
        )

    def test_instance_dictionary_is_live_and_replaceable(self):
        field = native.Field(label="original")
        field.__dict__["label"] = "changed"
        field.extra_state = {"nested": [1]}
        self.assertEqual(field.label, "changed")
        self.assertIs(vars(field)["extra_state"], field.extra_state)
        field.__dict__ = {"label": "replacement"}
        self.assertEqual(field.label, "replacement")

    def test_native_class_can_be_patched_and_restored(self):
        field = native.Field()
        original = native.Field.to_representation
        with patch.object(native.Field, "to_representation", lambda self, value: value + 1):
            self.assertEqual(field.to_representation(4), 5)
        self.assertIs(native.Field.to_representation, original)
        self.assertEqual(field.to_representation(4), 4)

    def test_creation_counter_orders_fields_and_collects_declarations(self):
        first, second = native.Field(), native.Field()

        class Example(native.Serializer):
            second_field = second
            first_field = first

        self.assertEqual(list(Example._declared_fields), ["first_field", "second_field"])
        self.assertNotIn("first_field", Example.__dict__)
        self.assertIs(type(Example), native.SerializerMetaclass)

    def test_inherited_precedence_and_non_field_override(self):
        class Left(native.Serializer):
            shared = native.Field(label="left")
            left = native.Field()

        class Right(native.Serializer):
            shared = native.Field(label="right")
            right = native.Field()

        class Combined(Left, Right):
            left = None
            last = native.Field()

        self.assertEqual(list(Combined._declared_fields), ["shared", "right", "last"])
        self.assertEqual(Combined._declared_fields["shared"].label, "left")

    def test_python_metaclass_override_calls_native_super(self):
        events = []

        class CustomMeta(native.SerializerMetaclass):
            def __new__(mcls, name, bases, namespace, **kwargs):
                events.append(name)
                namespace["marker"] = True
                return super().__new__(mcls, name, bases, namespace, **kwargs)

        class Example(native.Serializer, metaclass=CustomMeta):
            item = native.Field()

        self.assertEqual(events, ["Example"])
        self.assertTrue(Example.marker)
        self.assertIn("item", Example._declared_fields)
        self.assertIs(type(Example), CustomMeta)

    def test_classcell_init_subclass_and_set_name_are_preserved(self):
        events = []

        class Descriptor:
            def __set_name__(self, owner, name):
                events.append((owner.__name__, name))

        class Parent(native.Serializer):
            def __init_subclass__(cls, marker=None, **kwargs):
                events.append((cls.__name__, marker))
                super().__init_subclass__(**kwargs)

        class Child(Parent, marker="configured"):
            item = native.Field()
            descriptor = Descriptor()

            def to_representation(self, instance):
                return super().to_representation(instance)

        self.assertIn(("Child", "descriptor"), events)
        self.assertIn(("Child", "configured"), events)
        self.assertEqual(Child().to_representation({"item": 7}), {"item": 7})

    def test_deepcopy_reconstructs_python_subclass(self):
        payload = {"items": [1]}
        validators = [lambda value: value]
        field = PickleField(label=payload, validators=validators)
        field.runtime_only = "not a constructor argument"
        copied = copy.deepcopy(field)
        self.assertIs(type(copied), PickleField)
        self.assertEqual(copied.label, payload)
        self.assertIsNot(copied.label, payload)
        self.assertIs(copied.validators, validators)
        self.assertFalse(hasattr(copied, "runtime_only"))

    def test_bound_fields_are_independent_and_python_bind_override_runs(self):
        events = []

        class CustomField(native.Field):
            def bind(self, name, parent):
                events.append(name)
                super().bind(name, parent)

        class Example(native.Serializer):
            item = CustomField(label="original")

        first, second = Example(), Example()
        self.assertIsNot(first.fields["item"], second.fields["item"])
        self.assertIs(first.fields["item"].parent, first)
        first.fields["item"].label = "changed"
        self.assertEqual(second.fields["item"].label, "original")
        self.assertEqual(events, ["item", "item"])

    def test_live_field_changes_and_custom_field_dispatch(self):
        class Upper(native.Field):
            def to_representation(self, value):
                return super().to_representation(value).upper()

        class Example(native.Serializer):
            keep = native.Field()
            remove = native.Field()

        serializer = Example()
        serializer.fields.pop("remove")
        serializer.fields["keep"] = Upper()
        self.assertEqual(serializer.to_representation({"keep": "hello"}), {"keep": "HELLO"})

    def test_explicit_base_descriptor_call_accepts_python_subclass(self):
        class Example(native.ModelSerializer):
            item = native.Field()

            def to_representation(self, instance):
                return {"overridden": True}

        serializer = Example()
        self.assertEqual(
            native.ModelSerializer.to_representation(serializer, {"item": 8}), {"item": 8}
        )
        with self.assertRaises(AttributeError):
            native.Serializer.to_representation(object(), {"item": 8})

    def test_native_signature_and_keyword_receiver_match_python_methods(self):
        def reference(self, instance):
            pass

        class Example(native.Serializer):
            item = native.Field()

        serializer = Example()
        self.assertEqual(inspect.signature(native.Serializer.to_representation), inspect.signature(reference))
        self.assertEqual(str(inspect.signature(serializer.to_representation)), "(instance)")
        self.assertEqual(native.Serializer.to_representation(self=serializer, instance={"item": 7}), {"item": 7})
        self.assertEqual(serializer.to_representation(instance={"item": 8}), {"item": 8})

    def test_saved_native_bound_method_cycle_is_collected(self):
        serializer = native.Serializer()
        serializer.saved_method = serializer.to_representation
        reference = weakref.ref(serializer)
        del serializer
        gc.collect()
        self.assertIsNone(reference())

    def test_native_fixed_signature_binds_keywords_and_rejects_invalid_calls(self):
        field = native.Field()
        parent = native.Serializer()
        field.bind(parent=parent, field_name="item")
        self.assertEqual(field.field_name, "item")
        self.assertIs(field.parent, parent)
        self.assertEqual(field.to_representation(value=9), 9)
        invalid_calls = [
            lambda: field.to_representation(),
            lambda: field.to_representation(1, 2),
            lambda: field.to_representation(1, value=2),
            lambda: field.to_representation(1, unexpected=2),
            lambda: field.to_representation(self=field, value=1),
            lambda: native.Field.to_representation(field, self=field, value=1),
        ]
        for call in invalid_calls:
            with self.assertRaises(TypeError):
                call()

    def test_mayo_multiple_inheritance_and_cooperative_super(self):
        class AppointmentTimezoneMixin(native.Serializer):
            def to_representation(self, instance):
                self.events.append("timezone")
                return super().to_representation(instance)

        class DestinationAppointmentTimezoneMixin(AppointmentTimezoneMixin):
            pass

        class ResolvedLocationDisplayMixin(native.Serializer):
            def to_representation(self, instance):
                self.events.append("location")
                return super().to_representation(instance)

        class DestinationSerializationMixin(native.BaseSerializer):
            def to_representation(self, instance):
                previous = getattr(self, "_destination_instance", None)
                self._destination_instance = instance
                try:
                    self.events.append("destination")
                    return super().to_representation(instance)
                finally:
                    self._destination_instance = previous

        class DestinationModelSerializer(DestinationSerializationMixin, native.ModelSerializer):
            pass

        class AppointmentSerializer(
            DestinationAppointmentTimezoneMixin,
            ResolvedLocationDisplayMixin,
            DestinationModelSerializer,
        ):
            item = native.Field()

        serializer = AppointmentSerializer(events=[])
        self.assertEqual(serializer.to_representation({"item": 9}), {"item": 9})
        self.assertEqual(serializer.events, ["timezone", "location", "destination"])
        self.assertIsNone(serializer._destination_instance)
        self.assertIsInstance(serializer, DestinationSerializationMixin)

    def test_python_callback_can_reenter_same_native_serializer(self):
        class ReentrantField(native.Field):
            def to_representation(self, value):
                if value:
                    return self.parent.to_representation({"item": 0})["item"] + value
                return 0

        class Example(native.Serializer):
            item = ReentrantField()

        self.assertEqual(Example().to_representation({"item": 3}), {"item": 3})

    def test_callback_exception_identity_is_preserved_without_replay(self):
        failure = RuntimeError("callback failure")
        events = []

        class FailingField(native.Field):
            def to_representation(self, value):
                events.append(value)
                raise failure

        class Example(native.Serializer):
            item = FailingField()

        with self.assertRaises(RuntimeError) as caught:
            Example().to_representation({"item": 1})
        self.assertIs(caught.exception, failure)
        self.assertEqual(events, [1])

    def test_native_list_loop_calls_python_child_override_once_per_item(self):
        class Child(native.Serializer):
            item = native.Field()

            def to_representation(self, instance):
                self.events.append(instance["item"])
                return super().to_representation(instance)

        child = Child(events=[])
        serializer = native.ListSerializer(child=child)
        self.assertEqual(
            serializer.to_representation(iter([{"item": 1}, {"item": 2}])),
            [{"item": 1}, {"item": 2}],
        )
        self.assertEqual(child.events, [1, 2])

    def test_native_root_and_python_subclass_cycles_are_collectable(self):
        for cls in (native._NativeState, native.Field, native.Serializer):
            refs = []
            for _ in range(100):
                instance = cls()
                instance.cycle = instance
                refs.append(weakref.ref(instance))
            del instance
            gc.collect()
            self.assertTrue(all(ref() is None for ref in refs), cls.__name__)

    def test_parent_child_cycles_are_collectable(self):
        class Example(native.Serializer):
            item = native.Field()

        serializer = Example()
        parent = weakref.ref(serializer)
        child = weakref.ref(serializer.fields["item"])
        del serializer
        gc.collect()
        self.assertIsNone(parent())
        self.assertIsNone(child())

    def test_finalizer_can_resurrect_and_then_release_native_subclass(self):
        resurrected, events = [], []

        class Finalized(native.Field):
            def __del__(self):
                events.append("finalized")
                resurrected.append(self)

        instance = Finalized()
        instance.cycle = instance
        del instance
        gc.collect()
        self.assertEqual(events, ["finalized"])
        resurrected.clear()
        gc.collect()
        self.assertEqual(events, ["finalized"])

    def test_native_field_and_python_subclass_pickle_preserve_state(self):
        for cls in (native.Field, PickleField):
            original = cls(label="field")
            original.extra = [1, 2]
            restored = pickle.loads(pickle.dumps(original))
            self.assertIs(type(restored), cls)
            self.assertEqual(restored.__dict__, original.__dict__)

    def test_string_subclass_constructor_code_and_pickle(self):
        class CustomDetail(native.ErrorDetail):
            pass

        for cls in (native.ErrorDetail, CustomDetail):
            detail = cls("missing", code="required")
            self.assertIs(type(detail), cls)
            self.assertIsInstance(detail, str)
            self.assertEqual(str(detail), "missing")
            self.assertEqual(detail.code, "required")
        detail = native.ErrorDetail("missing", "required")
        restored = pickle.loads(pickle.dumps(detail))
        self.assertIs(type(restored), native.ErrorDetail)
        self.assertEqual(restored, detail)
        self.assertEqual(restored.code, "required")

    def test_dict_and_list_subclasses_drop_backlink_for_pickle(self):
        serializer = native.Serializer()
        for cls, value, base in (
            (native.ReturnDict, {"item": 1}, dict),
            (native.ReturnList, [1, 2], list),
        ):
            result = cls(value, serializer=serializer)
            self.assertIs(type(result), cls)
            self.assertIs(result.serializer, serializer)
            restored = pickle.loads(pickle.dumps(result))
            self.assertIs(type(restored), base)
            self.assertEqual(restored, value)
            result.cycle = result
            ref = weakref.ref(result)
            del result
            gc.collect()
            self.assertIsNone(ref())


if __name__ == "__main__":
    unittest.main()
