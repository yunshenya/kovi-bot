# Intrinsic model assets

The large intrinsic model weights are intentionally not tracked in the source
repository. Download a complete, verified bundle with:

```bash
./scripts/download-model.sh --variant text
```

Use `--variant full` when local vision inference is needed. Both variants are
installed under `models/yunxi-intrinsic/minimind-3o`, which is the default
`model.intrinsic.asset_dir` in `bot.conf.example.toml`.

The bundle manifests and third-party notices live in `model-assets/`; the
runtime directory remains ignored so a source checkout does not accidentally
carry hundreds of megabytes of weights.
