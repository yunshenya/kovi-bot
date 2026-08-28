#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/download-model.sh [options]

Download the pinned MiniMind-3o runtime bundle into the default asset path.

Options:
  --variant text|full       Download text-only (default) or text+vision assets
  --dest PATH               Install into PATH instead of the default asset path
  --language-base-url URL   Override the language-model file base URL
  --vision-base-url URL     Override the vision-model file base URL
  --force                   Replace an existing destination after verification
  -h, --help                Show this help

The default URLs point to pinned upstream Hugging Face revisions. A release
mirror can be selected with the two base URL options or matching environment
variables.
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
variant="text"
destination="$repo_root/models/yunxi-intrinsic/minimind-3o"
force=0
language_base_url="${MODEL_LANGUAGE_BASE_URL:-https://huggingface.co/jingyaogong/minimind-3o/resolve/ee3febbd08cc5b2bd41c039c825a8934232fee33}"
vision_base_url="${MODEL_VISION_BASE_URL:-https://huggingface.co/jingyaogong/siglip2-base-p32-256-ve/resolve/9465d1dc89db6bc6227c5b6b0e0ca9b940325d62}"

while (($# > 0)); do
    case "$1" in
        --variant)
            (($# >= 2)) || die "--variant requires text or full"
            variant="$2"
            shift 2
            ;;
        --dest)
            (($# >= 2)) || die "--dest requires a path"
            destination="$2"
            shift 2
            ;;
        --language-base-url)
            (($# >= 2)) || die "--language-base-url requires a URL"
            language_base_url="$2"
            shift 2
            ;;
        --vision-base-url)
            (($# >= 2)) || die "--vision-base-url requires a URL"
            vision_base_url="$2"
            shift 2
            ;;
        --force)
            force=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (use --help for usage)"
            ;;
    esac
done

case "$variant" in
    text|full) ;;
    *) die "unsupported variant: $variant (expected text or full)" ;;
esac

require_command curl
language_base_url="${language_base_url%/}"
vision_base_url="${vision_base_url%/}"

if [[ "$variant" == "full" ]]; then
    manifest_source="$repo_root/model-assets/yunxi-intrinsic/minimind-3o-full/manifest.toml"
else
    manifest_source="$repo_root/model-assets/yunxi-intrinsic/minimind-3o-text/manifest.toml"
fi
notice_source="$repo_root/model-assets/yunxi-intrinsic/THIRD_PARTY_NOTICES"
[[ -f "$manifest_source" ]] || die "manifest source is missing: $manifest_source"
[[ -f "$notice_source" ]] || die "third-party notice is missing: $notice_source"

# Keep these pinned values in lockstep with the tracked variant manifests.
if command -v sha256sum >/dev/null 2>&1; then
    sha256_file() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_file() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    die "required command not found: sha256sum or shasum"
fi

file_size() {
    wc -c < "$1" | tr -d '[:space:]'
}

language_assets=(
    "config.json|1332|13ed7f8f6759d292b1948fc29d03a1ebf5c1b131d26cefcb19e86dad975784b6"
    "tokenizer.json|451182|71f32c68cf63a15355a8fc171b7594b3d41870fe0ddb54fc6aefa55f73a4a668"
    "tokenizer_config.json|7545|04ae7620b9cf93fd2d6fbf94936b0c3c4be65f30cd6ef6fa8741baac986525d1"
    "special_tokens_map.json|1059|f85ecedc16cf0083242e21ac2ef23ac613153b71240c7d2ab61de24f085d2db9"
    "generation_config.json|111|cd22f08997eb2353c809a1d9b523c42c924247eb8619aa3e408bba3448f6e270"
    "chat_template.jinja|3895|97c6a9ef3f8d35044f435bb7773f1af160a7328ab6e8d2e9d67702941320a711"
    "pytorch_model.bin|226324754|21530f9bbc540f461e2c0e29292ad359781d4d984d1e0c994510945f9b0edaab"
)
vision_assets=(
    "vision/config.json|410|5ad8dda7d55541c7749f9b1cc43fe8eb8c70d8664588d89f710242ce06b3167e"
    "vision/preprocessor_config.json|394|d14ba2ee3fd816f3de8abaddc31953565128eaf37c73ad4bed32101a98465aff"
    "vision/README.md|1432|7276ff3ab08b8d2bdd16e11de24c523d860e08141160529806a75c0526f03aa4"
    "vision/model.safetensors|189129296|c1e9cc19ed6704b87353ee00b9ff5d6191886d741898339984364f789c62810d"
)

destination_parent="$(dirname -- "$destination")"
mkdir -p -- "$destination_parent"
staging="$(mktemp -d "$destination_parent/.minimind-3o.XXXXXX")"

cleanup() {
    if [[ -n "${staging:-}" && -d "$staging" ]]; then
        rm -rf -- "$staging"
    fi
}
trap cleanup EXIT

download_file() {
    local url="$1"
    local target="$2"
    mkdir -p -- "$(dirname -- "$target")"
    printf 'downloading %s\n' "$url"
    if curl --fail --location --retry 3 --retry-delay 2 --continue-at - \
        --output "$target" "$url"; then
        return
    fi
    # A completed partial download can make curl return HTTP 416 on resume.
    rm -f -- "$target"
    curl --fail --location --retry 3 --retry-delay 2 --output "$target" "$url"
}

for spec in "${language_assets[@]}"; do
    IFS='|' read -r asset _ _ <<< "$spec"
    download_file "$language_base_url/$asset" "$staging/$asset"
done

if [[ "$variant" == "full" ]]; then
    for spec in "${vision_assets[@]}"; do
        IFS='|' read -r asset _ _ <<< "$spec"
        download_file "$vision_base_url/${asset#vision/}" "$staging/$asset"
    done
fi

install -m 0644 "$manifest_source" "$staging/manifest.toml"
install -m 0644 "$notice_source" "$staging/THIRD_PARTY_NOTICES"

verify_asset() {
    local root="$1"
    local spec="$2"
    local asset expected_size expected_hash actual_size actual_hash
    IFS='|' read -r asset expected_size expected_hash <<< "$spec"
    [[ -f "$root/$asset" ]] || die "downloaded asset is missing: $asset"
    actual_size="$(file_size "$root/$asset")"
    [[ "$actual_size" == "$expected_size" ]] || die \
        "size mismatch for $asset: expected $expected_size, got $actual_size"
    actual_hash="$(sha256_file "$root/$asset" | tr '[:upper:]' '[:lower:]')"
    [[ "$actual_hash" == "$expected_hash" ]] || die \
        "sha256 mismatch for $asset: expected $expected_hash, got $actual_hash"
}

for spec in "${language_assets[@]}"; do
    verify_asset "$staging" "$spec"
done
if [[ "$variant" == "full" ]]; then
    for spec in "${vision_assets[@]}"; do
        verify_asset "$staging" "$spec"
    done
fi
printf 'verified %s MiniMind-3o assets\n' "$variant"

if [[ -e "$destination" || -L "$destination" ]]; then
    (( force == 1 )) || die "destination already exists: $destination (use --force to replace it)"
    backup="${destination}.old.$$"
    mv -- "$destination" "$backup"
else
    backup=""
fi

if mv -- "$staging" "$destination"; then
    staging=""
    if [[ -n "$backup" ]]; then
        rm -rf -- "$backup"
    fi
else
    if [[ -n "$backup" && ! -e "$destination" && ! -L "$destination" ]]; then
        mv -- "$backup" "$destination"
    fi
    die "failed to install model bundle at $destination"
fi

printf 'installed %s MiniMind-3o bundle at %s\n' "$variant" "$destination"
