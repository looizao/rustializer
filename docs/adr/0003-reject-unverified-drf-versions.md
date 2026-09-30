# Reject unverified DRF versions at startup

Activation will refuse to start an application using a DRF version that Rustializer has not verified, rather than silently retaining the original engine or attempting unverified acceleration. This sacrifices automatic availability after an unsupported DRF upgrade in favor of an explicit supported-version contract. Supporting newer versions requires a verified compatibility update.

The startup check will use the DRF package version alone, without fingerprinting source files. We chose the simpler version-based check over exact source verification; modified installations and different source snapshots reporting a verified version will pass this check, although the compatibility tests establish evidence only for the chosen reference code.

Activation must occur during early startup, before application serializers or DRF serializer classes have been imported. Startup will reject activation that violates this requirement, because replacing exported classes cannot retarget already-created subclasses. Each application entry point must arrange this early activation before serializer imports.
