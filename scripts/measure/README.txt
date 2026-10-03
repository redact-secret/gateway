Header-size measurement (#41, ADR 0023). Not part of the product binary or the qualification
build seam. Synthetic credentials only; nothing leaves loopback.

Setup, from the repository root (reuses the pinned SDK lockfiles of the qualification suites):

  (cd qualification/sdk/node && npm ci --ignore-scripts)
  (cd qualification/sdk/python && uv venv .venv && \
     uv pip install --require-hashes -r requirements.txt --python .venv/bin/python)

Run:

  node scripts/measure/header-sizes.mjs --json out.json

Set MEASURE_PYTHON to use another interpreter that has the pinned openai package. Remove
node_modules and the venv afterwards if disk is limited.
