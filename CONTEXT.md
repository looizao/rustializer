# Rustializer

Rustializer targets existing Python/Django applications using Django REST Framework serializers.

## Language

**Rustializer**:
A Rust-backed replacement for Django REST Framework's serialization engine intended for use in existing Python/Django applications.
_Avoid_: Rust-native serialization framework

**DRF**:
Django REST Framework, whose serializer behavior provides the reference for Rustializer's compatibility contract.

**Drop-in replacement**:
A replacement that preserves existing serializer definitions, imports, and application-facing serializer behavior after installation and one explicit activation step.
_Avoid_: API-inspired alternative, serializer migration

**Activation**:
The explicit step by which an existing Django application enables Rustializer for its DRF serializers.

**Representation**:
The outgoing values produced by a serializer from an object or collection, before a renderer encodes them into a response format.
_Avoid_: JSON encoding

**Input validation**:
The conversion and checking of incoming values against a serializer's rules, producing validated values or structured validation errors.
