#!/bin/sh
# ait-testkit Command-driver stub (protocol v1) — see
# docs/e2e-driver-protocol.md.
#
# Select it with:   AIT_E2E_DRIVER=docs/examples/e2e-driver-stub.sh
#
# One fresh subprocess per agent step: read the JSON request from stdin
# (ends at EOF), print the JSON response to stdout, exit 0. To adapt
# this to a real endpoint, replace the canned answers with a call to
# your model — secrets come from the inherited environment
# ($MY_API_KEY), never from this file.
request=$(cat)

case "$request" in
  *'"kind":"assert"'*)
    # Assert response: pass, or {"fail":{"reason":"…"}} with the
    # driver's explanation (it becomes the test failure's evidence).
    printf '%s' '{"v":1,"verdict":"pass"}'
    ;;
  *)
    # Act response: the semantic inputs to send —
    # {"key":"Alt+T"} | {"text":"…"} | {"bytes":"<hex>"}.
    printf '%s' '{"v":1,"inputs":[{"text":"echo hello from the driver"},{"key":"Enter"}]}'
    ;;
esac
