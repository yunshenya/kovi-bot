# Intrinsic model metadata

This directory contains the small, tracked metadata used to install the
external MiniMind-3o runtime bundles. The actual weight files stay outside the
Git repository under `models/yunxi-intrinsic/`.

Each variant has a complete manifest because the runtime verifies every listed
asset before loading it:

- `yunxi-intrinsic/minimind-3o-text/manifest.toml` enables text inference only.
- `yunxi-intrinsic/minimind-3o-full/manifest.toml` includes the vision assets.

Keep the manifests and the downloaded bundle in sync when publishing a mirror
or GitHub Release asset. The download script verifies the same hashes recorded
here before it atomically installs the selected bundle.
