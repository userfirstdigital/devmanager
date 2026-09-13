# File-service fixtures

These fixtures contain only non-secret example content. The integration tests
create their mutable files inside temporary directories so fixture files never
become mutation targets.

- `text/hello.txt` exercises UTF-8 text metadata.
- `secret/.env.example` exercises secret-like path classification without a
  real credential value.
