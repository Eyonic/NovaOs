# Third-party code

| Directory | What | Version | License | Source |
|---|---|---|---|---|
| `uacpi/` | uACPI, a portable ACPI implementation (tables, AML interpreter, events, sleep states) | 6.1.1 (see `uacpi/VERSION`) | MIT (`uacpi/LICENSE`) | https://github.com/uACPI/uACPI, tag `6.1.1` (tarball sha256 `40f2e278b2080e69f4eaa3384404944874d430e50b72c1066cab2ba476e2b769`) |

`uacpi/` holds the release's `include/`, `source/`, `LICENSE` and `README.md`, unmodified
(the test suite and build files are left out). `kernel/build.rs` compiles it; Nova implements
its kernel API in `kernel/src/acpi.rs`, and `kernel/acpi/nova_acpi.c` is Nova's glue.

The Linux kernel source used by the optional Linux driver layer is not stored here; see
`docs/linux-drivers.md`.
