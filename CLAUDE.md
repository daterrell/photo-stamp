# photo-stamp

Burns where and when a photo was taken into its corner, from its metadata. A Windows drop-folder client, a
Docker watcher, and a self-hosted place server. README.md has the features and the module table. Hosted on
Forgejo as `dave/photo-stamp` (`origin`), mirrored to GitHub. Default branch is `main`; changes go through PRs
that the user merges. The users of the Windows client are the user's non-technical family.

## Build and test

There is no local Rust toolchain; everything runs in the dev container through `dev.sh`:

- `./dev.sh cargo fmt --all --check`, `./dev.sh cargo clippy --workspace --all-targets --locked -- -D warnings`
  and `./dev.sh cargo test --workspace --locked` are exactly what CI runs. Run all three before pushing.
- The dev image has the exiftool, libheif and fonts the tests need. It is rebuilt when `docker/Dockerfile`
  changes.
- Windows build: `scripts/build-windows.sh` (needs only Docker). CI also clippy-checks the
  `x86_64-pc-windows-gnu` target without the `heif` feature, so code behind `cfg(windows)` can break CI while
  Linux tests pass.

## Versions and releases

- The version comes from git tags (`scripts/version.sh`), never from `Cargo.toml` (placeholder `0.0.0`). CI
  checkouts need full history for this.
- Every build of `main` is a beta (Windows zip in Forgejo's package registry, newest 10 kept; images tagged
  `:main` and `sha-…`). To release, tag a commit on `main`: `git tag v0.7.0 && git push origin v0.7.0`. CI
  then publishes the GitHub Release and the `:0.7.0` and `:latest` images.
- Clients update through the place server, which relays builds from the package registry. A bad update rolls
  back after 90 s, so don't change the restart handshake lightly.

## Gotchas

- CI runs on Forgejo's Docker-in-Docker runner, which can't bind-mount the checkout. That is why `dev.sh` copies
  the source in (`DEV_SH_COPY_SOURCE`) and keeps cargo caches and target dirs in named volumes. Don't replace
  that with `-v $PWD`.
- The user's family runs real installs. In tests, set `PHOTO_STAMP_DATA_DIR` and `PHOTO_STAMP_UI_PORT` so a test
  never touches a real install's data or port, and kill only the PIDs you started.
- In PowerShell P/Invoke, `$null` passed to a string parameter becomes `""`, so `FindWindowW` needs
  `IntPtr.Zero` for the title or it finds nothing.
- Pushing packages from CI needs the `PACKAGES_TOKEN` secret; the automatic token can't.
