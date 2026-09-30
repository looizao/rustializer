"""Serializer-only Mayo workloads from an explicitly selected isolated copy."""
import datetime
import os
from pathlib import Path
import socket
import sys


def setup_mayo(copy):
    copy = copy.resolve(strict=True)
    original = (Path.home() / 'cactus/mayo-eh-api').resolve()
    if copy == original or copy.is_relative_to(original):
        raise ValueError('Mayo benchmarks require an isolated copy outside the original checkout')
    sys.path.insert(0, str(copy))
    os.environ.update(DJANGO_SETTINGS_MODULE='mayo_eh_api.settings.test',
                      DATABASE_URL='postgresql://127.0.0.1:9/unused',
                      REDIS_URL='redis://127.0.0.1:9/0',
                      TEMPORAL_REGISTER_SCHEDULES='False',
                      GOOGLE_APPLICATION_CREDENTIALS=str(copy / 'nonexistent-benchmark-credentials.json'))

    def reject_connection(*args, **kwargs):
        raise AssertionError('Serializer-only benchmarks must not open network connections')
    socket.socket.connect = reject_connection
    import django
    django.setup()
    import common
    assert Path(common.__file__).resolve().is_relative_to(copy)
    from django.db import connection

    def reject_queries(execute, sql, params, many, context):
        raise AssertionError('Serializer-only benchmarks must not query a database')
    connection.execute_wrappers.append(reject_queries)
    import rest_framework
    return django, rest_framework


def mayo_cases(records):
    from itinerary.models import ClinicalSite
    from itinerary.serializers.clinical_site import ClinicalSiteSerializer
    from notifications.models import Notification
    from notifications.serializers import NotificationListSerializer, ManualNotificationRequestSerializer

    row = ClinicalSite(id=1, code='rochester', name='Mayo Clinic Rochester',
                       timezone='US/Central', address='200 First St SW', city='Rochester',
                       state='MN', postal_code='55905', country='USA',
                       website='https://www.mayoclinic.org/')
    instant = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)
    notifications = [Notification(id=i + 1, title='Example notification', body='Example body',
                     created=instant, modified=instant) for i in range(records)]
    warm = ClinicalSiteSerializer()
    many = NotificationListSerializer(many=True)

    def validation(data):
        item = ManualNotificationRequestSerializer(data=data)
        valid = item.is_valid()
        return [valid, item.validated_data, item.errors]

    cases = {
        'clinical_site_warm_representation': (lambda: warm.to_representation(row), 1),
        'clinical_site_fresh_representation': (lambda: ClinicalSiteSerializer(row).data, 1),
        'notification_many_representation': (lambda: many.to_representation(notifications), records),
        'manual_notification_valid_input': (lambda: validation({'patient_id': 1, 'notification_type': 'DAY_SUMMARY'}), 1),
        'manual_notification_invalid_input': (lambda: validation({'patient_id': 1, 'medical_record_number': 'MRN-1', 'notification_type': 'DAY_SUMMARY'}), 1),
        'clinical_site_field_generation': (lambda: list(ClinicalSiteSerializer().fields), 1),
    }
    return cases, ClinicalSiteSerializer, row
