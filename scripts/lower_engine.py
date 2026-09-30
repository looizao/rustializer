"""Lower the two pinned reference engines into data for the native Rust runtime."""
import argparse
import ast
import hashlib
import json
from pathlib import Path
import subprocess

REFERENCES = {
    '3.17.2': 'ad309f3e18aa5591db87a8fc959dc4564c000324',
    '3.18.1': 'b578eab1cad040414b758131af1e17aa000b51e2',
}
MODULES = [
    'utils/html', 'utils/representation', 'utils/json', 'utils/formatting',
    'utils/humanize_datetime', 'utils/timezone', 'utils/serializer_helpers',
    'exceptions', 'validators', 'fields', 'relations',
    'utils/model_meta', 'utils/field_mapping', 'serializers',
]


def lower(value):
    if isinstance(value, ast.AST):
        result = {'_node': type(value).__name__}
        result.update({name: lower(child) for name, child in ast.iter_fields(value)
                       if name not in ('ctx', 'type_comment', 'returns', 'annotation')})
        if hasattr(value, 'lineno'):
            result['line'] = value.lineno
        return result
    if isinstance(value, list):
        return [lower(child) for child in value]
    if value is Ellipsis:
        return {'_node': 'Ellipsis'}
    if isinstance(value, bytes):
        return {'_node': 'Bytes', 'value': list(value)}
    return value


def native_records(tree):
    """Recreate DRF's tuple records without collections.namedtuple's Python methods."""
    body = []
    for node in tree.body:
        if (isinstance(node, ast.Assign) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)
                and node.value.func.id == 'namedtuple'):
            name = ast.literal_eval(node.value.args[0])
            fields = ast.literal_eval(node.value.args[1])
            arguments = ', '.join(fields)
            getters = '\n'.join(
                f'    {field} = _tuplegetter({index}, "Alias for field number {index}")'
                for index, field in enumerate(fields))
            source = f'''class {name}(tuple):
    """{name}({arguments})"""
    __slots__ = ()
    _fields = {tuple(fields)!r}
    __match_args__ = _fields
    _field_defaults = {{}}
    def __new__(cls, {arguments}):
        return tuple.__new__(cls, ({arguments},))
    @classmethod
    def _make(cls, iterable):
        result = tuple.__new__(cls, iterable)
        if len(result) != {len(fields)}:
            raise TypeError('Expected {len(fields)} arguments, got %d' % len(result))
        return result
    def _replace(self, **kwds):
        result = self._make(kwds.pop(name, value) for name, value in zip(self._fields, self))
        if kwds:
            error = TypeError if sys.version_info >= (3, 13) else ValueError
            raise error('Got unexpected field names: %r' % list(kwds))
        return result
    def _asdict(self):
        return dict(zip(self._fields, self))
    def __getnewargs__(self):
        return tuple(self)
    def __repr__(self):
        return self.__class__.__name__ + '(' + ', '.join(name + '=' + repr(value) for name, value in zip(self._fields, self)) + ')'
{getters}
'''
            body.extend(ast.parse(source).body)
        else:
            body.append(node)
    if len(body) != len(tree.body) or any(
            isinstance(node, ast.ClassDef) and node.name in ('FieldInfo', 'RelationInfo')
            for node in body):
        body[:0] = ast.parse('import sys\nfrom _collections import _tuplegetter').body
    tree.body = body
    return tree


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('checkout', type=Path)
    options = parser.parse_args()
    destination = Path(__file__).resolve().parents[1] / 'src' / 'programs'
    destination.mkdir(exist_ok=True)
    for version, reference in REFERENCES.items():
        modules = []
        for module in MODULES:
            source = subprocess.check_output(
                ['git', '-C', str(options.checkout), 'show',
                 f'{reference}:rest_framework/{module}.py'])
            modules.append({'name': 'rest_framework.' + module.replace('/', '.'),
                            'sha256': hashlib.sha256(source).hexdigest(),
                            'body': lower(native_records(ast.parse(source)))['body']})
        payload = {'reference': reference, 'version': version, 'modules': modules}
        (destination / f'drf-{version}.json').write_text(
            json.dumps(payload, separators=(',', ':'), ensure_ascii=True) + '\n')
    license_text = subprocess.check_output(
        ['git', '-C', str(options.checkout), 'show',
         f'{REFERENCES["3.18.1"]}:LICENSE.md']).decode()
    (destination / 'LICENSE').write_text(license_text)


if __name__ == '__main__':
    main()
