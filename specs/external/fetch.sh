#!/usr/bin/env bash
# Download the real-world audit corpus (docs/SPEC_COVERAGE.md) into this
# directory. The specs are multi-megabyte and deliberately NOT checked
# in; `tests/real_world.rs` (run with `cargo test --test real_world --
# --ignored`) skips with a pointer here when they are missing.
set -euo pipefail
cd "$(dirname "$0")"

fetch() {
    local name="$1" url="$2"
    if [ -s "$name" ]; then
        echo "have    $name"
    else
        echo "fetch   $name"
        curl -fsSL --retry 3 -o "$name" "$url"
    fi
}

# The canonical torture test (3.0.3, ~13 MB; progenitor runs it through
# typify upstream).
fetch github.json https://raw.githubusercontent.com/github/rest-api-description/main/descriptions/api.github.com/api.github.com.json

# Huge, allOf/anyOf-heavy (3.0.0, ~8 MB).
fetch stripe.json https://raw.githubusercontent.com/stripe/openapi/master/openapi/spec3.json

# Swagger 2.0 native — characterizes the version gate.
fetch docker-v2.0.yaml https://docs.docker.com/reference/api/engine/version/v1.47.yaml

# OpenAPI 3.1 — characterizes the 3.1 seam.
fetch museum-3.1.yaml https://raw.githubusercontent.com/Redocly/museum-openapi-example/main/openapi.yaml

# Mid-size 3.0.x with rich schema-level examples (wire round-trips).
fetch plaid.yml https://raw.githubusercontent.com/plaid/plaid-openapi/master/2020-09-14.yml
fetch digitalocean.yaml https://api-engineering.nyc3.cdn.digitaloceanspaces.com/spec-ci/DigitalOcean-public.v2.yaml

# serde_yaml refuses integer literals above u64::MAX (DigitalOcean's
# `group_concat_max_len.maximum: 18446744073709552000`); the JSON loader
# accepts the same number as f64. Normalize the literal to its exact
# float form — semantics-preserving, documented in the audit as a
# loader limitation.
if grep -q '18446744073709552000' digitalocean.yaml 2>/dev/null; then
    sed -i '' 's/18446744073709552000/1.8446744073709552e19/g' digitalocean.yaml
    echo "normalized digitalocean.yaml big-int literal (loader limitation)"
fi

echo "corpus complete:"
ls -lh *.json *.yaml *.yml 2>/dev/null | awk '{print "  " $5 "\t" $9}'
