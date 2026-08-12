# Cutover contract fixture

This fixture is a deliberately small tracked repository used to prove the
audit's path, symbol, reference, prerequisite, already-absent HOLD,
handoff-replacement, isolation, packaging-handoff, and deletion behavior. It
never represents production AppData or an installed DevManager. Handoff rows
are distinct from deletion rows: they require replacement owners and package
identity files, and they never declare a `deletionSet`.
