# Chan extension archive

Each platform archive uses one layout:

```text
mobile-chat/
  mobile-chat-extension[.exe]
  mobile-chat.toml
  licenses/LICENSE-APACHE
```

`mobile-chat.toml` here is a template. Its `command` is a placeholder, because Chan spawns an extension with the extensions directory as its working directory and therefore needs an absolute path. The installer is what writes the real declaration, with the path it just installed to, in the target platform's own syntax.

Tagged releases publish `mobile-chat-linux-x86_64.tar.gz`, `mobile-chat-linux-aarch64.tar.gz`, `mobile-chat-windows-x86_64.zip`, and `mobile-chat-macos-aarch64.tar.gz`. Each binary is compiled and tested on a native GitHub-hosted runner; the Linux binaries target musl, so they carry no libc dependency. `SHA256SUMS` covers all four archives, and the release also carries the root `install.sh`.

`scripts/package-chan-extension.py` assembles the archives. Every entry is stamped with the HEAD commit time and normalized ownership, so packaging the same commit twice produces byte-identical archives. `scripts/install-chan-extension.sh` builds and installs from a checkout instead, for development.

The release installer detects the operating system and architecture, verifies the archive against `SHA256SUMS` before extracting anything, installs under `~/.local/lib/mobile-chat`, writes `~/.chan/extensions/mobile-chat.toml`, and leaves the Chan restart to the operator. Chan itself ships no Mobile Chat files.
