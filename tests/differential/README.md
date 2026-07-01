# Differential tests (agsh vs bash/sh)

`diff.py` runs each script under agsh and a reference shell and compares
**stdout + exit code** (stderr text is ignored; agsh prefixes diagnostics).

```sh
cargo build
python3 tests/differential/diff.py        # REF=bash by default
REF=sh python3 tests/differential/diff.py  # compare against /bin/sh
```

Exit code is 0 when the only mismatches are the documented `EXPECTED_DIFFS`,
else 1. Add a case to `TESTS`; if it must diverge from bash, justify it in
`EXPECTED_DIFFS`.
