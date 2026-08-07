# Reproduction Scenarios

Outputs are written under `.phase0/` and must not be committed.

```sh
mkdir -p .phase0/live-workspace
printf 'Phase 0 fixture\n' > .phase0/live-workspace/README.txt
```

## Lifecycle

```sh
cargo run -p sdk-probe -- \
  --cwd . \
  --output .phase0/smoke.jsonl
```

## File, built-in shell, and custom terminal

```sh
cargo run -p sdk-probe -- \
  --cwd .phase0/live-workspace \
  --output .phase0/success.jsonl \
  --approve-permissions \
  --prompt 'Inspect README.txt. Create result.txt containing exactly phase-0-ok. Then use the phase0_terminal tool to run /bin/sh with args ["-c", "for i in 1 2 3; do echo terminal-$i; sleep 0.2; done"].'
```

## Cancellation

```sh
cargo run -p sdk-probe -- \
  --cwd .phase0/live-workspace \
  --output .phase0/cancel.jsonl \
  --approve-permissions \
  --abort-after-ms 750 \
  --prompt 'Analyze every file in depth and produce a detailed report.'
```

## Failure

```sh
cargo run -p sdk-probe -- \
  --cwd .phase0/live-workspace \
  --output .phase0/failure.jsonl \
  --approve-permissions \
  --prompt 'Use phase0_terminal exactly once with program /definitely/not/a/program and no args. Report the failure and do not retry.'
```

## Fleet

```sh
cargo run -p sdk-probe -- \
  --cwd .phase0/live-workspace \
  --output .phase0/fleet.jsonl \
  --approve-permissions \
  --fleet-prompt 'Use one subagent to inspect README.txt and report its first line. Do not modify files.'
```

`--approve-permissions` is intentionally explicit. The probe denies tool
permissions by default.
