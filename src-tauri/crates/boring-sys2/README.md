# Isolated BoringSSL native symbols

This is a build-only patch of `boring-sys2` **4.15.15**, the version already used
by rquest in this repository. It lets that BoringSSL run in one executable with an
OpenSSL build without duplicate symbols or ABI interposition. (It was introduced for
the since-removed Qoder client, which vendored OpenSSL 3.5.)
It does not update BoringSSL or change any TLS algorithms, extensions or profiles.

The unmodified upstream crate archive is committed under `vendor/`, verified by
SHA-256 before extraction, and never fetched during builds:

- Source: https://static.crates.io/crates/boring-sys2/boring-sys2-4.15.15.crate
- SHA-256: `ab6c876a958d17057d9d3a31075bb9609237413a1fe665432782c31ef596eb6b`
- Rust bindings/build scaffold: upstream MIT license, `LICENSE-MIT`.
- Native source: upstream licenses preserved in the archive and `LICENSE-BORINGSSL`.

`src/lib.rs`, `build/config.rs`, and the build scaffold come from that archive.
The changes to `build/main.rs` extract its native sources/toolchain files/patches,
invoke `build/prefix.rs`, and supply bindgen link names. Existing upstream native
patches are still applied. `Cargo.toml` adds only build dependencies needed for
archive extraction, integrity verification and target object parsing.

The native build has two passes:

1. Compile the target's original static archives and enumerate their defined C
   exports using the Rust `object` reader (ELF, Mach-O or COFF).
2. Generate BoringSSL's supported C/GNU assembly/NASM prefix headers and rebuild
   with `BORINGSSL_PREFIX=abv_bssl`. Fail if any original C export remains.

Bindgen retains the Rust API and links to the prefixed native exports. C++ mangled
symbols are left unchanged; they do not share OpenSSL's C ABI. No duplicate-symbol
suppression, platform `objcopy`, runtime TLS library loading or protocol upgrade
is involved. This patch requires the bundled non-FIPS source; custom/precompiled
BoringSSL overrides and FIPS mode fail explicitly.

The normal CMake/compiler/libclang requirements are unchanged. The extra native
pass increases a cold build's time.
