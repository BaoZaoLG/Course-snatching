# Cargo audit exceptions

`Cargo.lock` currently contains `quick-xml 0.39.4` only through the Linux/Wayland
build-time dependency `wayland-scanner`. The application is a Windows-only binary,
and `cargo tree --target x86_64-pc-windows-msvc -i quick-xml@0.39.4` confirms that
the crate is not part of the Windows build or shipped executable.

The Windows audit script therefore ignores these two advisories only after asserting
that the vulnerable crate is absent from the Windows dependency tree:

- RUSTSEC-2026-0194
- RUSTSEC-2026-0195

Remove the exceptions as soon as upstream Wayland dependencies move to
`quick-xml >= 0.41.0`. If the crate ever enters the Windows tree, the script fails
before invoking the ignored audit.
